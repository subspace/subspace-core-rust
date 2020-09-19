use crate::block::Block;
use crate::manager::ProtocolMessage;
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
use log::*;
use rand::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::fmt::Display;
use std::io;
use std::io::Write;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::{fmt, mem};

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

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub enum Message {
    PeersRequest,
    PeersResponse { contacts: Vec<SocketAddr> },
    BlocksRequest { timeslot: u64 },
    BlocksResponse { timeslot: u64, blocks: Vec<Block> },
    BlockProposal { block: Block },
    TxProposal { tx: SimpleCreditTx },
}

impl Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::PeersRequest => "PeersRequest",
                Self::PeersResponse { .. } => "PeersResponse",
                Self::BlocksRequest { .. } => "BlockRequest",
                Self::BlocksResponse { .. } => "BlockResponse",
                Self::BlockProposal { .. } => "BlockProposal",
                Self::TxProposal { .. } => "TxProposal",
            }
        )
    }
}

impl Message {
    pub fn to_bytes(&self) -> Bytes {
        let mut writer = BytesMut::new().writer();
        bincode::serialize_into(&mut writer, self).unwrap();
        writer.into_inner().freeze()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ()> {
        bincode::deserialize(bytes).map_err(|error| {
            debug!("Failed to deserialize network message: {}", error);
        })
    }

    pub fn get_id(&self) -> [u8; 32] {
        crypto::digest_sha_256(&self.to_bytes())
    }
}

enum NetworkEvent {
    InboundMessage {
        peer_addr: SocketAddr,
        message: Message,
    },
}

pub struct Router {
    node_id: NodeID,
    node_addr: SocketAddr,
    connections: HashMap<SocketAddr, Sender<Bytes>>,
    peers: HashSet<SocketAddr>,
}

impl Router {
    /// create a new empty router
    pub fn new(node_id: NodeID, node_addr: SocketAddr) -> Router {
        Router {
            node_id,
            node_addr,
            connections: HashMap::new(),
            peers: HashSet::new(),
        }
    }

    /// add a new connection, possibly add a new peer
    pub fn add(&mut self, node_addr: SocketAddr, sender: Sender<Bytes>) {
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
    pub fn remove(&mut self, peer_addr: SocketAddr) {
        // ToDo: Add another peer to replace the removed one

        if self.connections.contains_key(&peer_addr) {
            self.connections.remove(&peer_addr);
            self.peers.remove(&peer_addr);
        }
    }

    /// send a message to all peers
    pub fn gossip(&self, message: Message) {
        let bytes = message.to_bytes();
        for node_addr in self.peers.iter() {
            trace!("Sending a {} message to {}", message, node_addr);
            self.maybe_send_bytes_to(node_addr, bytes.clone());
        }
    }

    /// send a message to all but one peer (who sent you the message)
    pub fn regossip(&self, sender: &SocketAddr, message: Message) {
        let bytes = message.to_bytes();
        for node_addr in self.peers.iter() {
            if node_addr != sender {
                trace!("Sending a {} message to {}", message, node_addr);
                self.maybe_send_bytes_to(node_addr, bytes.clone());
            }
        }
    }

    /// send a message to specific node by node_id
    pub fn send(&self, receiver: &SocketAddr, message: Message) {
        trace!("Sending a {} message to {}", message, receiver);
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
    pub fn get_random_peer(&self) -> Option<SocketAddr> {
        self.peers.iter().choose(&mut rand::thread_rng()).copied()
    }

    /// get a peer at random excluding a specific peer
    pub fn get_random_peer_excluding(&self, node_addr: SocketAddr) -> Option<SocketAddr> {
        self.peers
            .iter()
            .filter(|&peer_addr| !peer_addr.eq(&node_addr))
            .choose(&mut rand::thread_rng())
            .copied()
    }

    /// retrieve the socket addr for each peer, except the one asking
    pub fn get_contacts(&self, exception: &SocketAddr) -> Vec<SocketAddr> {
        self.peers
            .iter()
            .filter(|&peer| !peer.eq(&exception))
            .copied()
            .collect()
    }

    pub fn get_state(&self) -> console::AppState {
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
        let max_message_length = 2usize.pow(16) - 1;
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

struct Inner {
    // TODO: Remove `broker_sender`
    broker_sender: Sender<NetworkEvent>,
    connections_handle: StdMutex<Option<JoinHandle<()>>>,
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
struct Network {
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
        let router = Router::new(node_id, listener.local_addr()?);

        let inner = Arc::new(Inner {
            broker_sender: broker_sender.clone(),
            connections_handle: StdMutex::default(),
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
        self.inner.router.lock().await.add(peer_addr, client_sender);

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
                // trace!("{:?}", message);
                self.inner
                    .broker_sender
                    .send(NetworkEvent::InboundMessage { peer_addr, message })
                    .await;
            }
        }

        self.inner.router.lock().await.remove(peer_addr);
        info!("Broker has dropped a peer who disconnected");
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
) {
    let gateway_addr: std::net::SocketAddr = DEV_GATEWAY_ADDR.parse().unwrap();
    let (broker_sender, mut broker_receiver) = channel::<NetworkEvent>(32);

    // create the tcp listener
    let addr = if matches!(node_type, NodeType::Gateway) {
        gateway_addr
    } else {
        local_addr
    };
    // TODO: This works because of `join!()` at the end that prevents `network` from being dropped
    let network = Network::new(node_id, addr, broker_sender.clone())
        .await
        .unwrap();

    // receives protocol messages from manager
    let protocol_receiver_loop = async {
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
                            router.send(&peer, Message::BlocksRequest { timeslot });
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

                        network
                            .inner
                            .router
                            .lock()
                            .await
                            .send(&node_addr, Message::BlocksResponse { timeslot, blocks });
                    }
                    ProtocolMessage::BlockProposalRemote { block, peer_addr } => {
                        // propagating a block received over the network that was valid
                        // do not send back to the node who sent to you

                        network
                            .inner
                            .router
                            .lock()
                            .await
                            .regossip(&peer_addr, Message::BlockProposal { block });
                    }
                    ProtocolMessage::TxProposalRemote { tx, peer_addr } => {
                        // propagating a tx received over the network that was valid
                        // do not send back to the node who sent to you

                        network
                            .inner
                            .router
                            .lock()
                            .await
                            .regossip(&peer_addr, Message::TxProposal { tx });
                    }
                    ProtocolMessage::BlockProposalLocal { block } => {
                        // propagating a block generated locally, send to all

                        network
                            .inner
                            .router
                            .lock()
                            .await
                            .gossip(Message::BlockProposal { block });
                    }
                    ProtocolMessage::TxProposalLocal { tx } => {
                        // propagating a tx generated locally, send to all

                        network
                            .inner
                            .router
                            .lock()
                            .await
                            .gossip(Message::TxProposal { tx });
                    }
                    ProtocolMessage::StateUpdateRequest => {
                        let state = network.inner.router.lock().await.get_state();
                        net_to_main_tx
                            .send(ProtocolMessage::StateUpdateResponse { state })
                            .await;
                    }
                    _ => panic!(
                        "Network protocol listener has received an unknown protocol message!"
                    ),
                }
            }
        }
    };

    // receives network messages from peers and protocol messages from manager
    // maintains an async channel between each open socket and sender half
    let broker_loop = async {
        while let Some(event) = broker_receiver.next().await {
            match event {
                NetworkEvent::InboundMessage { peer_addr, message } => {
                    // messages received over the network from another peer, send to manager or handle internally
                    trace!("Received a {} network message from {}", message, peer_addr);

                    // ToDo: (later) implement a cache of last x messages (only if block or tx)

                    match message {
                        Message::PeersRequest => {
                            // retrieve peers and send over the wire

                            // ToDo: fully implement and test

                            let contacts =
                                network.inner.router.lock().await.get_contacts(&peer_addr);

                            network
                                .inner
                                .router
                                .lock()
                                .await
                                .send(&peer_addr, Message::PeersResponse { contacts });
                        }
                        Message::PeersResponse { contacts } => {
                            // ToDo: match responses to request id, else ignore

                            // convert binary to peers, for each peer, attempt to connect
                            // need to write another method to add peer on connection
                            for potential_peer_addr in contacts.iter().copied() {
                                while network.inner.router.lock().await.peers.len() < MAX_PEERS {
                                    let broker_sender = broker_sender.clone();
                                    let network = network.clone();
                                    async_std::task::spawn(async move {
                                        network.connect_to(peer_addr).await.unwrap();
                                    });
                                }
                            }

                            // if we still have too few peers, should we try another peer
                        }
                        Message::BlocksRequest { timeslot } => {
                            let net_to_main_tx = net_to_main_tx.clone();
                            let message = ProtocolMessage::BlocksRequestFrom {
                                node_addr: peer_addr,
                                timeslot,
                            };

                            async_std::task::spawn(async move {
                                net_to_main_tx.send(message).await;
                            });
                        }
                        Message::BlocksResponse { timeslot, blocks } => {
                            // TODO: Handle the case where peer does not have the block
                            let net_to_main_tx = net_to_main_tx.clone();
                            async_std::task::spawn(async move {
                                net_to_main_tx
                                    .send(ProtocolMessage::BlocksResponse { timeslot, blocks })
                                    .await;
                            });

                            // if no block in response, request from a different peer
                            // match block {
                            //     Some(block) => {
                            //         let net_to_main_tx = net_to_main_tx.clone();

                            //         async_std::task::spawn(async move {
                            //             net_to_main_tx
                            //                 .send(ProtocolMessage::BlockResponse { block })
                            //                 .await;
                            //         });
                            //     }
                            //     None => {
                            //         info!("Peer did not have block at desired index, requesting from a different peer");

                            //         if let Some(new_peer) =
                            //             router.get_random_peer_excluding(peer_addr)
                            //         {
                            //             router.send(&new_peer, Message::BlockRequest { index });
                            //         } else {
                            //             info!("Failed to request block: no other peers found");
                            //         }
                            //         continue;
                            //     }
                            // }
                        }
                        Message::BlockProposal { block } => {
                            // send to main

                            let net_to_main_tx = net_to_main_tx.clone();

                            async_std::task::spawn(async move {
                                net_to_main_tx
                                    .send(ProtocolMessage::BlockProposalRemote { block, peer_addr })
                                    .await;
                            });
                        }
                        Message::TxProposal { tx } => {
                            // send to main

                            let net_to_main_tx = net_to_main_tx.clone();

                            async_std::task::spawn(async move {
                                net_to_main_tx
                                    .send(ProtocolMessage::TxProposalRemote { tx, peer_addr })
                                    .await;
                            });
                        }
                    }
                }
            }
        }
    };

    let network_startup = async {
        // if not gateway, connect to the gateway
        if node_type != NodeType::Gateway {
            info!("Connecting to gateway node");

            let broker_sender = broker_sender.clone();
            network.connect_to(gateway_addr).await.unwrap();
        }
    };

    join!(protocol_receiver_loop, broker_loop, network_startup);
}
