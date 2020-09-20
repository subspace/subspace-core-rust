pub(crate) mod messages;

use crate::block::Block;
use crate::manager::ProtocolMessage;
use crate::network::messages::{BlocksRequest, BlocksResponse, FromBytes, ToBytes};
use crate::transaction::SimpleCreditTx;
use crate::{console, MAX_PEERS};
use crate::{crypto, DEV_GATEWAY_ADDR};
use async_std::net::{TcpListener, TcpStream};
use async_std::sync::{channel, Receiver, Sender};
use async_std::task::JoinHandle;
use bytes::buf::BufMutExt;
use bytes::{Bytes, BytesMut};
use futures::lock::Mutex as AsyncMutex;
use futures::{join, AsyncReadExt, AsyncWriteExt, StreamExt};
use futures_lite::future;
use log::*;
use messages::{GossipMessage, Message, Request, RequestMessage, ResponseMessage};
use rand::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::fmt::{Debug, Display};
use std::io::Write;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;
use std::{fmt, io, mem};

/* Todo
 *
 * Fix all unwrap calls
 * Ensure message size does not exceed 16k
 * Exchange peers on sync (and ensure peers request works)
 * Add another peer to replace the dropped one
 * Handle empty block responses, currently that peer will randomly come again soon
 * Handle errors as results
 * Write tests
 * Filter duplicate message with cache at manager using get_id
 * Handle get peers response with outbound message correctly
 *
*/

const MAX_MESSAGE_CONTENTS_LENGTH: usize = 2usize.pow(16) - 1;
// TODO: What should this timeout be?
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

pub type NodeID = [u8; 32];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum NodeType {
    Gateway,
    Peer,
    Farmer,
}

impl Display for NodeType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            NodeType::Gateway => write!(f, "Gateway"),
            NodeType::Farmer => write!(f, "Farmer"),
            NodeType::Peer => write!(f, "Peer"),
        }
    }
}

impl FromStr for NodeType {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "peer" => Ok(Self::Peer),
            "farmer" => Ok(Self::Farmer),
            "gateway" => Ok(Self::Gateway),
            _ => Err(()),
        }
    }
}

enum NetworkEvent {
    InboundMessage {
        peer_addr: SocketAddr,
        message: Message,
    },
}

struct Router {
    node_id: NodeID,
    node_addr: SocketAddr,
    connections: HashMap<SocketAddr, Sender<Bytes>>,
    peers: HashSet<SocketAddr>,
}

impl Router {
    /// create a new empty router
    fn new(node_id: NodeID, node_addr: SocketAddr) -> Router {
        Router {
            node_id,
            node_addr,
            connections: HashMap::new(),
            peers: HashSet::new(),
        }
    }

    /// add a new connection, possibly add a new peer
    fn add(&mut self, node_addr: SocketAddr, sender: Sender<Bytes>) {
        self.connections.insert(node_addr, sender);

        // if peers is low, add to peers
        // later explicitly ask to reduce churn
        if self.peers.len() < MAX_PEERS {
            self.peers.insert(node_addr);
        }
    }

    /// get a connection by node id
    fn get_connection(&self, node_addr: &SocketAddr) -> Option<&Sender<Bytes>> {
        self.connections.get(node_addr)
    }

    /// remove a connection and peer if connection is removed
    fn remove(&mut self, peer_addr: SocketAddr) {
        // ToDo: Add another peer to replace the removed one

        if self.connections.contains_key(&peer_addr) {
            self.connections.remove(&peer_addr);
            self.peers.remove(&peer_addr);
        }
    }

    /// Send a message to all peers
    fn gossip(&self, message: GossipMessage) {
        let message = Message::Gossip(message);
        let bytes = message.to_bytes();
        for node_addr in self.peers.iter() {
            trace!("Sending a {} message to {}", message, node_addr);
            self.maybe_send_bytes_to(node_addr, bytes.clone());
        }
    }

    /// Send a message to all but one peer (who sent you the message)
    fn regossip(&self, sender: &SocketAddr, message: GossipMessage) {
        let message = Message::Gossip(message);
        let bytes = message.to_bytes();
        for node_addr in self.peers.iter() {
            if node_addr != sender {
                trace!("Sending a {} message to {}", message, node_addr);
                self.maybe_send_bytes_to(node_addr, bytes.clone());
            }
        }
    }

    /// send a message to specific node by node_id
    fn send(&self, receiver: &SocketAddr, message: Message) {
        trace!("Sending a {} message to {}", message, receiver);
        // TODO: Should be `Message` here, not `Message`
        self.maybe_send_bytes_to(receiver, message.to_bytes());
    }

    fn maybe_send_bytes_to(&self, addr: &SocketAddr, bytes: Bytes) {
        if let Some(client_sender) = self.get_connection(addr).cloned() {
            async_std::task::spawn(async move {
                client_sender.send(bytes).await;
            });
        }
    }

    /// get a peer at random
    fn get_random_peer(&self) -> Option<SocketAddr> {
        self.peers.iter().choose(&mut rand::thread_rng()).copied()
    }

    /// get a peer at random excluding a specific peer
    fn get_random_peer_excluding(&self, node_addr: SocketAddr) -> Option<SocketAddr> {
        self.peers
            .iter()
            .filter(|&peer_addr| !peer_addr.eq(&node_addr))
            .choose(&mut rand::thread_rng())
            .copied()
    }

    /// retrieve the socket addr for each peer, except the one asking
    fn get_contacts(&self, exception: &SocketAddr) -> Vec<SocketAddr> {
        self.peers
            .iter()
            .filter(|&peer| !peer.eq(&exception))
            .copied()
            .collect()
    }

    fn get_state(&self) -> console::AppState {
        console::AppState {
            node_type: String::from(""),
            node_id: hex::encode(&self.node_id[0..8]),
            node_addr: self.node_addr.to_string(),
            connections: self.connections.len().to_string(),
            peers: self.peers.len().to_string(),
            pieces: String::from(""),
            blocks: String::from(""),
        }
    }
}

/// Returns Option<(message_bytes, consumed_bytes)>
fn extract_message(input: &[u8]) -> Option<(Result<Message, ()>, usize)> {
    if input.len() <= 2 {
        None
    } else {
        let (message_length_bytes, remainder) = input.split_at(2);
        let message_length = u16::from_le_bytes(message_length_bytes.try_into().unwrap()) as usize;

        if remainder.len() < message_length {
            None
        } else {
            let message = Message::from_bytes(&remainder[..message_length]);

            Some((message, 2 + message_length))
        }
    }
}

fn read_messages(mut stream: TcpStream) -> Receiver<Result<Message, ()>> {
    let (messages_sender, messages_receiver) = channel(10);

    async_std::task::spawn(async move {
        let header_length = 2;
        let max_message_length = MAX_MESSAGE_CONTENTS_LENGTH;
        // We support up to 16 kiB message + 2 byte header, so since we may have message across 2
        // read buffers, allocate enough space to contain up to 2 such messages
        let mut buffer = BytesMut::with_capacity((header_length + max_message_length) * 2);
        let mut buffer_contents_bytes = 0;
        buffer.resize(buffer.capacity(), 0);
        // Auxiliary buffer that we will swap with primary on each iteration
        let mut aux_buffer = BytesMut::with_capacity((header_length + max_message_length) * 2);
        aux_buffer.resize(aux_buffer.capacity(), 0);

        loop {
            match stream.read(&mut buffer[buffer_contents_bytes..]).await {
                Ok(read_size) => {
                    if read_size == 0 {
                        // peer disconnected, exit the loop
                        break;
                    }

                    buffer_contents_bytes += read_size;

                    // Read as many messages as possible starting from the beginning
                    let mut offset = 0;
                    while let Some((message, consumed_bytes)) =
                        extract_message(&buffer[offset..buffer_contents_bytes])
                    {
                        messages_sender.send(message).await;
                        // Move cursor forward
                        offset += consumed_bytes;
                    }

                    // Copy unprocessed remainder from `buffer` to `aux_buffer`
                    aux_buffer
                        .as_mut()
                        .write_all(&buffer[offset..buffer_contents_bytes])
                        .unwrap();
                    // Decrease useful contents length by processed amount
                    buffer_contents_bytes -= offset;
                    // Swap buffers to avoid additional copying
                    mem::swap(&mut aux_buffer, &mut buffer);
                }
                Err(error) => {
                    warn!("Failed to read bytes: {}", error);
                    break;
                }
            }
        }
    });

    messages_receiver
}

#[derive(Debug)]
pub(crate) enum RequestError {
    ConnectionClosed,
    BadResponse,
    MessageTooLong,
    NoPeers,
    TimedOut,
}

type Response = Result<Vec<u8>, RequestError>;

#[derive(Default)]
struct RequestsContainer {
    next_id: u32,
    handlers: HashMap<u32, async_oneshot::Sender<ResponseMessage>>,
}

struct Inner {
    // TODO: Remove `broker_sender`
    broker_sender: Sender<NetworkEvent>,
    connections_handle: StdMutex<Option<JoinHandle<()>>>,
    gossip_sender: async_channel::Sender<(SocketAddr, GossipMessage)>,
    gossip_receiver: StdMutex<Option<async_channel::Receiver<(SocketAddr, GossipMessage)>>>,
    request_sender: async_channel::Sender<(
        SocketAddr,
        RequestMessage,
        async_oneshot::Sender<ResponseMessage>,
    )>,
    request_receiver: StdMutex<
        Option<
            async_channel::Receiver<(
                SocketAddr,
                RequestMessage,
                async_oneshot::Sender<ResponseMessage>,
            )>,
        >,
    >,
    requests_container: Arc<AsyncMutex<RequestsContainer>>,
    router: AsyncMutex<Router>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Stop accepting new connections, this will also drop the listener and close the socket
        async_std::task::spawn(
            self.connections_handle
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .cancel(),
        );
    }
}

#[derive(Clone)]
pub struct Network {
    inner: Arc<Inner>,
}

impl Network {
    // TODO: Remove `broker_sender`
    async fn new(
        node_id: NodeID,
        addr: SocketAddr,
        broker_sender: Sender<NetworkEvent>,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let (gossip_sender, gossip_receiver) =
            async_channel::bounded::<(SocketAddr, GossipMessage)>(32);
        let (request_sender, request_receiver) = async_channel::bounded::<(
            SocketAddr,
            RequestMessage,
            async_oneshot::Sender<ResponseMessage>,
        )>(32);
        let requests_container = Arc::<AsyncMutex<RequestsContainer>>::default();
        let router = Router::new(node_id, listener.local_addr()?);

        let inner = Arc::new(Inner {
            broker_sender: broker_sender.clone(),
            connections_handle: StdMutex::default(),
            gossip_sender,
            gossip_receiver: StdMutex::new(Some(gossip_receiver)),
            request_sender,
            request_receiver: StdMutex::new(Some(request_receiver)),
            requests_container,
            router: AsyncMutex::new(router),
        });

        let network = Self { inner };

        let connections_handle = {
            let network_weak = network.downgrade();

            async_std::task::spawn(async move {
                let mut connections = listener.incoming();

                info!("Listening on TCP socket for inbound connections");

                while let Some(stream) = connections.next().await {
                    info!("New inbound TCP connection initiated");

                    let stream = stream.unwrap();
                    let peer_addr = stream.peer_addr().unwrap();
                    if let Some(network) = network_weak.upgrade() {
                        async_std::task::spawn(network.on_connected(peer_addr, stream));
                    } else {
                        break;
                    }
                }
            })
        };

        network
            .inner
            .connections_handle
            .lock()
            .unwrap()
            .replace(connections_handle);

        Ok(network)
    }

    /// Send a message to all peers
    pub(crate) async fn gossip(&self, message: GossipMessage) {
        self.inner.router.lock().await.gossip(message);
    }

    /// Send a message to all but one peer (who sent you the message)
    pub(crate) async fn regossip(&self, sender: &SocketAddr, message: GossipMessage) {
        self.inner.router.lock().await.regossip(sender, message);
    }

    pub(crate) async fn request_blocks(
        &self,
        request: BlocksRequest,
    ) -> Result<BlocksResponse, RequestError> {
        let response = self
            .request_internal(RequestMessage::BlocksRequest(request))
            .await?;

        match response {
            ResponseMessage::BlocksResponse(response) => Ok(response),
            _ => Err(RequestError::BadResponse),
        }
    }

    pub(crate) fn get_gossip_receiver(
        &self,
    ) -> Option<async_channel::Receiver<(SocketAddr, GossipMessage)>> {
        self.inner.gossip_receiver.lock().unwrap().take()
    }

    pub(crate) fn get_requests_receiver(
        &self,
    ) -> Option<
        async_channel::Receiver<(
            SocketAddr,
            RequestMessage,
            async_oneshot::Sender<ResponseMessage>,
        )>,
    > {
        self.inner.request_receiver.lock().unwrap().take()
    }

    pub(crate) async fn get_state(&self) -> console::AppState {
        self.inner.router.lock().await.get_state()
    }

    fn downgrade(&self) -> NetworkWeak {
        let inner = Arc::downgrade(&self.inner);
        NetworkWeak { inner }
    }

    async fn connect_to(&self, peer_addr: SocketAddr) -> io::Result<()> {
        let stream = TcpStream::connect(peer_addr).await?;
        async_std::task::spawn(self.clone().on_connected(peer_addr, stream));

        Ok(())
    }

    async fn on_connected(self, peer_addr: SocketAddr, mut stream: TcpStream) {
        let (client_sender, mut client_receiver) = channel::<Bytes>(32);
        self.inner
            .router
            .lock()
            .await
            .add(peer_addr, client_sender.clone());

        let mut messages_receiver = read_messages(stream.clone());

        // listen for new messages from the broker and send back to peer over stream
        async_std::task::spawn(async move {
            while let Some(bytes) = client_receiver.next().await {
                let length = bytes.len() as u16;
                stream.write_all(&length.to_le_bytes()).await.unwrap();
                stream.write_all(&bytes).await.unwrap();
            }
        });

        while let Some(message) = messages_receiver.next().await {
            if let Ok(message) = message {
                match message {
                    Message::Gossip(message) => {
                        drop(self.inner.gossip_sender.send((peer_addr, message)).await);
                    }
                    Message::Request { id, message } => {
                        let (response_sender, response_receiver) = async_oneshot::oneshot();
                        drop(
                            self.inner
                                .request_sender
                                .send((peer_addr, message, response_sender))
                                .await,
                        );
                        {
                            let client_sender = client_sender.clone();
                            async_std::task::spawn(async move {
                                if let Ok(message) = response_receiver.await {
                                    drop(
                                        client_sender
                                            .send(Message::Response { id, message }.to_bytes())
                                            .await,
                                    );
                                }
                            });
                        }
                    }
                    Message::Response { id, message } => {
                        if let Some(response_sender) = self
                            .inner
                            .requests_container
                            .lock()
                            .await
                            .handlers
                            .remove(&id)
                        {
                            drop(response_sender.send(message));
                        } else {
                            debug!("Received response for unknown request {}", id);
                        }
                    }
                }
            }
        }

        self.inner.router.lock().await.remove(peer_addr);
        info!("Broker has dropped a peer who disconnected");
    }

    /// Non-generic method to avoid significant duplication in final binary
    async fn request_internal(
        &self,
        message: RequestMessage,
    ) -> Result<ResponseMessage, RequestError> {
        let router = self.inner.router.lock().await;
        let peer = match router.get_random_peer() {
            Some(peer) => peer,
            None => {
                return Err(RequestError::NoPeers);
            }
        };

        let id;
        let (response_sender, response_receiver) = async_oneshot::oneshot();
        let requests_container = &self.inner.requests_container;

        {
            let mut requests_container = requests_container.lock().await;

            id = requests_container.next_id;

            requests_container.next_id = requests_container.next_id.wrapping_add(1);
            // TODO: No one writes to this yet
            requests_container.handlers.insert(id, response_sender);
        }

        let message = Message::Request { id, message }.to_bytes();
        if message.len() > MAX_MESSAGE_CONTENTS_LENGTH {
            requests_container.lock().await.handlers.remove(&id);

            return Err(RequestError::MessageTooLong);
        }

        // TODO: Should be a better method for this (maybe without router)
        router.maybe_send_bytes_to(&peer, message);
        drop(router);

        future::or(
            async move {
                response_receiver
                    .await
                    .map_err(|_| RequestError::ConnectionClosed {})
            },
            async move {
                async_io::Timer::after(REQUEST_TIMEOUT).await;

                requests_container.lock().await.handlers.remove(&id);

                Err(RequestError::TimedOut)
            },
        )
        .await
    }
}

#[derive(Clone)]
struct NetworkWeak {
    inner: Weak<Inner>,
}

impl NetworkWeak {
    fn upgrade(&self) -> Option<Network> {
        self.inner.upgrade().map(|inner| Network { inner })
    }
}

pub async fn run(
    node_type: NodeType,
    node_id: NodeID,
    local_addr: SocketAddr,
    net_to_main_tx: Sender<ProtocolMessage>,
    main_to_net_rx: Receiver<ProtocolMessage>,
) -> Network {
    let gateway_addr: std::net::SocketAddr = DEV_GATEWAY_ADDR.parse().unwrap();
    let (broker_sender, mut broker_receiver) = channel::<NetworkEvent>(32);

    // create the tcp listener
    let addr = if matches!(node_type, NodeType::Gateway) {
        gateway_addr
    } else {
        local_addr
    };
    let network = Network::new(node_id, addr, broker_sender.clone())
        .await
        .unwrap();

    // receives protocol messages from manager
    let protocol_receiver_loop = {
        let network = network.clone();

        async move {
            info!("Network is listening for protocol messages");
            loop {
                if let Ok(message) = main_to_net_rx.recv().await {
                    // messages received from manager that need to be sent over the network to peers
                    match message {
                        ProtocolMessage::BlocksRequest { timeslot } => {
                            // ledger requested a block at a given index
                            // send a block_request to one peer chosen at random from gossip group

                            let router = network.inner.router.lock().await;
                            if let Some(peer) = router.get_random_peer() {
                                // router.send(
                                //     &peer,
                                //     Message::Request {
                                //         id: 0,
                                //         message: RequestMessage::BlocksRequest { timeslot },
                                //     },
                                // );
                            } else {
                                info!("Failed to request block at index {}: no peers", timeslot);
                            }
                        }
                        ProtocolMessage::BlocksResponseTo {
                            node_addr,
                            blocks,
                            timeslot,
                        } => {
                            // send a block back to a peer that has requested it from you
                            // network.inner.router.lock().await.send(
                            //     &node_addr,
                            //     Message::Response {
                            //         id: 0,
                            //         message: ResponseMessage::BlocksResponse { timeslot, blocks },
                            //     },
                            // );
                        }
                        _ => panic!(
                            "Network protocol listener has received an unknown protocol message!"
                        ),
                    }
                }
            }
        }
    };

    // receives network messages from peers and protocol messages from manager
    // maintains an async channel between each open socket and sender half
    let broker_loop = {
        let network = network.clone();

        async move {
            while let Some(event) = broker_receiver.next().await {
                match event {
                    NetworkEvent::InboundMessage { peer_addr, message } => {
                        // messages received over the network from another peer, send to manager or handle internally
                        trace!("Received a {} network message from {}", message, peer_addr);

                        // ToDo: (later) implement a cache of last x messages (only if block or tx)

                        match message {
                            Message::Gossip(_) => {
                                error!("Shoudn't get here")
                                // TODO: Remove, never called
                            }
                            // Message::PeersRequest => {
                            //     // retrieve peers and send over the wire
                            //
                            //     // ToDo: fully implement and test
                            //
                            //     let contacts =
                            //         network.inner.router.lock().await.get_contacts(&peer_addr);
                            //
                            //     network
                            //         .inner
                            //         .router
                            //         .lock()
                            //         .await
                            //         .send(&peer_addr, Message::PeersResponse { contacts });
                            // }
                            // Message::PeersResponse { contacts } => {
                            //     // ToDo: match responses to request id, else ignore
                            //
                            //     // convert binary to peers, for each peer, attempt to connect
                            //     // need to write another method to add peer on connection
                            //     for potential_peer_addr in contacts.iter().copied() {
                            //         while network.inner.router.lock().await.peers.len() < MAX_PEERS
                            //         {
                            //             let broker_sender = broker_sender.clone();
                            //             let network = network.clone();
                            //             async_std::task::spawn(async move {
                            //                 network.connect_to(peer_addr).await.unwrap();
                            //             });
                            //         }
                            //     }
                            //
                            //     // if we still have too few peers, should we try another peer
                            // }
                            Message::Request { id, message } => {
                                // match message {
                                //     RequestMessage::BlocksRequest { timeslot } => {
                                //         let net_to_main_tx = net_to_main_tx.clone();
                                //         let message = ProtocolMessage::BlocksRequestFrom {
                                //             node_addr: peer_addr,
                                //             timeslot,
                                //         };
                                //
                                //         async_std::task::spawn(async move {
                                //             net_to_main_tx.send(message).await;
                                //         });
                                //     }
                                // };
                            }
                            Message::Response { id, message } => {
                                // match message {
                                //     ResponseMessage::BlocksResponse { timeslot, blocks } => {
                                //         // TODO: Handle the case where peer does not have the block
                                //         let net_to_main_tx = net_to_main_tx.clone();
                                //         async_std::task::spawn(async move {
                                //             net_to_main_tx
                                //                 .send(ProtocolMessage::BlocksResponse {
                                //                     timeslot,
                                //                     blocks,
                                //                 })
                                //                 .await;
                                //         });
                                //
                                //         // if no block in response, request from a different peer
                                //         // match block {
                                //         //     Some(block) => {
                                //         //         let net_to_main_tx = net_to_main_tx.clone();
                                //
                                //         //         async_std::task::spawn(async move {
                                //         //             net_to_main_tx
                                //         //                 .send(ProtocolMessage::BlockResponse { block })
                                //         //                 .await;
                                //         //         });
                                //         //     }
                                //         //     None => {
                                //         //         info!("Peer did not have block at desired index, requesting from a different peer");
                                //
                                //         //         if let Some(new_peer) =
                                //         //             router.get_random_peer_excluding(peer_addr)
                                //         //         {
                                //         //             router.send(&new_peer, Message::BlockRequest { index });
                                //         //         } else {
                                //         //             info!("Failed to request block: no other peers found");
                                //         //         }
                                //         //         continue;
                                //         //     }
                                //         // }
                                //     }
                                // }
                            }
                        }
                    }
                }
            }
        }
    };

    // if not gateway, connect to the gateway
    if node_type != NodeType::Gateway {
        info!("Connecting to gateway node");

        network.connect_to(gateway_addr).await.unwrap();
    }

    async_std::task::spawn(async move {
        join!(protocol_receiver_loop, broker_loop);
    });

    network
}
