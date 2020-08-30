#![allow(dead_code)]

use super::*;
use async_std::net::{TcpListener, TcpStream};
use async_std::prelude::*;
use async_std::sync::{channel, Receiver, Sender};
use bytes::buf::BufMutExt;
use bytes::{Bytes, BytesMut};
use futures::join;
use ledger::Block;
use log::*;
use manager::ProtocolMessage;
use rand::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::fmt::Display;
use std::io::Write;
use std::net::SocketAddr;
use std::str::FromStr;
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
    Ping,
    Pong,
    PeersRequest,
    PeersResponse { contacts: Vec<SocketAddr> },
    BlocksRequest { timeslot: u64 },
    BlocksResponse { timeslot: u64, blocks: Vec<Block> },
    BlockProposal { block: Block },
}

impl Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Ping => "Ping",
                Self::Pong => "Pong",
                Self::PeersRequest => "PeersRequest",
                Self::PeersResponse { .. } => "PeersResponse",
                Self::BlocksRequest { .. } => "BlockRequest",
                Self::BlocksResponse { .. } => "BlockResponse",
                Self::BlockProposal { .. } => "BlockProposal",
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
    NewPeer {
        peer_addr: SocketAddr,
        stream: TcpStream,
    },
    RemovedPeer {
        peer_addr: SocketAddr,
    },
    InboundMessage {
        peer_addr: SocketAddr,
        message: Message,
    },
    OutboundMessage {
        message: ProtocolMessage,
    },
}

pub struct Router {
    node_id: NodeID,
    node_type: NodeType,
    node_addr: SocketAddr,
    connections: HashMap<SocketAddr, Sender<Bytes>>,
    peers: HashSet<SocketAddr>,
}

impl Router {
    /// create a new empty router
    pub fn new(node_id: NodeID, node_type: NodeType, node_addr: SocketAddr) -> Router {
        Router {
            node_id,
            node_type,
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
            info!("Sending a {} message to {}", message, node_addr);
            self.maybe_send_bytes_to(node_addr, bytes.clone());
        }
    }

    /// send a message to all but one peer (who sent you the message)
    pub fn regossip(&self, sender: &SocketAddr, message: Message) {
        let bytes = message.to_bytes();
        for node_addr in self.peers.iter() {
            if node_addr != sender {
                info!("Sending a {} message to {}", message, node_addr);
                self.maybe_send_bytes_to(node_addr, bytes.clone());
            }
        }
    }

    /// send a message to specific node by node_id
    pub fn send(&self, receiver: &SocketAddr, message: Message) {
        info!("Sending a {} message to {}", message, receiver);
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

    // retrieve the socket addr for each peer, except the one asking
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
        let max_message_length = 16 * 1024;
        // We support up to 16 kiB message + 2 byte header, so since we may have message across 2
        // read buffers, allocate enough space to contain up to 2 such messages
        let mut buffer = BytesMut::with_capacity((header_length + max_message_length) * 2);
        let mut buffer_contents_bytes = 0;
        buffer.resize(buffer.capacity(), 0);
        // Auxiliary buffer that we will swap with primary on each iteration
        let mut aux_buffer = BytesMut::with_capacity((header_length + max_message_length) * 2);
        aux_buffer.resize(aux_buffer.capacity(), 0);

        // TODO: Handle error?
        while let Ok(read_size) = stream.read(&mut buffer[buffer_contents_bytes..]).await {
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
    });

    messages_receiver
}

async fn connect(peer_addr: SocketAddr, broker_sender: Sender<NetworkEvent>) {
    let stream = TcpStream::connect(peer_addr).await.unwrap();
    on_connected(peer_addr, stream, broker_sender).await;
}

async fn on_connected(
    peer_addr: SocketAddr,
    stream: TcpStream,
    broker_sender: Sender<NetworkEvent>,
) {
    broker_sender
        .send({
            let stream = stream.clone();

            NetworkEvent::NewPeer { peer_addr, stream }
        })
        .await;

    let mut messages_receiver = read_messages(stream);

    while let Some(message) = messages_receiver.next().await {
        if let Ok(message) = message {
            // info!("{:?}", message);
            broker_sender
                .send(NetworkEvent::InboundMessage { peer_addr, message })
                .await;
        }
    }

    broker_sender
        .send(NetworkEvent::RemovedPeer { peer_addr })
        .await;
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
    let socket = TcpListener::bind(addr).await.unwrap();

    let mut connections = socket.incoming();
    info!("Network is listening on TCP socket for inbound connections");

    // receives protocol messages from manager
    let protocol_receiver_loop = async {
        info!("Network is listening for protocol messages");
        loop {
            if let Ok(message) = main_to_net_rx.recv().await {
                // forward to broker as protocol message
                broker_sender
                    .send(NetworkEvent::OutboundMessage { message })
                    .await;
            }
        }
    };

    // receives new connection requests over the TCP socket
    let new_connection_loop = async {
        while let Some(stream) = connections.next().await {
            let broker_sender = broker_sender.clone();

            async_std::task::spawn(async move {
                info!("New inbound TCP connection initiated");

                let stream = stream.unwrap();
                let peer_addr = stream.peer_addr().unwrap();
                on_connected(peer_addr, stream, broker_sender).await;
            });
        }
    };

    // receives network messages from peers and protocol messages from manager
    // maintains an async channel between each open socket and sender half
    let broker_loop = async {
        let mut router = Router::new(node_id, node_type, socket.local_addr().unwrap());

        while let Some(event) = broker_receiver.next().await {
            match event {
                NetworkEvent::InboundMessage { peer_addr, message } => {
                    // messages received over the network from another peer, send to manager or handle internally
                    info!("Received a {} network message from {}", message, peer_addr);

                    // ToDo: (later) implement a cache of last x messages (only if block or tx)

                    match message {
                        Message::Ping => {
                            // send a pong response

                            router.send(&peer_addr, Message::Pong);
                        }
                        Message::Pong => {
                            // do nothing for now

                            // ToDo: latency timing
                        }
                        Message::PeersRequest => {
                            // retrieve peers and send over the wire

                            // ToDo: fully implement and test

                            let contacts = router.get_contacts(&peer_addr);

                            router.send(&peer_addr, Message::PeersResponse { contacts });
                        }
                        Message::PeersResponse { contacts } => {
                            // ToDo: match responses to request id, else ignore

                            // convert binary to peers, for each peer, attempt to connect
                            // need to write another method to add peer on connection
                            for potential_peer_addr in contacts.iter().copied() {
                                while router.peers.len() < MAX_PEERS {
                                    let broker_sender = broker_sender.clone();
                                    async_std::task::spawn(async move {
                                        connect(potential_peer_addr, broker_sender).await;
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
                    }
                }
                NetworkEvent::OutboundMessage { message } => {
                    // messages received from manager that need to be sent over the network to peers
                    match message {
                        ProtocolMessage::BlocksRequest { timeslot } => {
                            // ledger requested a block at a given index
                            // send a block_request to one peer chosen at random from gossip group

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

                            router.send(&node_addr, Message::BlocksResponse { timeslot, blocks });
                        }
                        ProtocolMessage::BlockProposalRemote { block, peer_addr } => {
                            // propagating a block received over the network that was valid
                            // do not send back to the node who sent to you

                            router.regossip(&peer_addr, Message::BlockProposal { block });
                        }
                        ProtocolMessage::BlockProposalLocal { block } => {
                            // propagating a block generated locally, send to all

                            router.gossip(Message::BlockProposal { block });
                        }
                        ProtocolMessage::StateUpdateRequest => {
                            let state = router.get_state();
                            net_to_main_tx
                                .send(ProtocolMessage::StateUpdateResponse { state })
                                .await;
                        }
                        _ => panic!(
                            "Network protocol listener has received an unknown protocol message!"
                        ),
                    }
                }
                NetworkEvent::NewPeer {
                    peer_addr,
                    mut stream,
                } => {
                    info!("Broker is adding a new peer");
                    let (client_sender, mut client_receiver) = channel::<Bytes>(32);
                    router.add(peer_addr, client_sender);

                    // listen for new messages from the broker and send back to peer over stream
                    async_std::task::spawn(async move {
                        while let Some(bytes) = client_receiver.next().await {
                            let length = bytes.len() as u16;
                            stream.write_all(&length.to_le_bytes()).await.unwrap();
                            stream.write_all(&bytes).await.unwrap();
                        }
                    });
                }
                NetworkEvent::RemovedPeer { peer_addr } => {
                    router.remove(peer_addr);
                    info!("Broker has dropped a peer who disconnected");
                }
            }
        }
    };

    let network_startup = async {
        // if not gateway, connect to the gateway
        if node_type != NodeType::Gateway {
            info!("Connecting to gateway node");

            let broker_sender = broker_sender.clone();
            connect(gateway_addr, broker_sender).await;
        }
    };

    join!(
        protocol_receiver_loop,
        new_connection_loop,
        broker_loop,
        network_startup
    );
}
