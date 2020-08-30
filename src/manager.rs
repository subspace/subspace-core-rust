#![allow(dead_code)]

use super::*;
use crate::solver::SolverMessage;
use crate::timer::EpochTracker;
use async_std::sync::{Receiver, Sender};
use async_std::task;
use console::AppState;
use futures::join;
use ledger::Block;
use log::*;
use network::NodeType;
use solver::Solution;
use std::fmt;
use std::fmt::Display;
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/*
 * Sync Workflow
 *
 * Peer requests block at height 0
 * GW sends block 0
 * Peer checks to see if has any cached blocks that reference the block received
 * If yes, peer recursively applies cached blocks
 * If no, peer requests the next block
 *
 * Need to pull old code for apply_pending_chlildren
 * Need to complete the code when blocks are received ahead of the current timeslot (wait for arrival)
 *
*/

// TODO: Split this into multiple enums
pub enum ProtocolMessage {
    /// On sync, main forwards block request to Net for tx from self to peer
    BlocksRequest {
        timeslot: u64,
    },
    /// peer receives at Net and forwards request to main for fetch
    BlocksRequestFrom {
        node_addr: SocketAddr,
        timeslot: u64,
    },
    /// peer main forwards response back to Net for tx
    BlocksResponseTo {
        node_addr: SocketAddr,
        blocks: Vec<Block>,
        timeslot: u64,
    },
    /// self receives at Net and forwards response back to Main
    BlocksResponse {
        blocks: Vec<Block>,
        timeslot: u64,
    },
    /// Net receives new full block, validates/applies, sends back to net for re-gossip
    BlockProposalRemote {
        block: Block,
        peer_addr: SocketAddr,
    },
    /// A valid full block has been produced locally and needs to be gossiped
    BlockProposalLocal {
        block: Block,
    },
    /// Solver sends a set of solutions back to main for application
    BlockSolutions {
        solutions: Vec<Solution>,
    },
    BlockArrived {
        block: Block,
        peer_addr: SocketAddr,
        cached: bool,
    },
    StartFarming,
    StopFarming,
    /// Main sends a state update request to manager for console state
    StateUpdateRequest,
    /// Manager sends a state update response to main for console state
    StateUpdateResponse {
        state: AppState,
    },
}

impl Display for ProtocolMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::BlocksRequest { .. } => "BlockRequest",
                Self::BlocksResponse { .. } => "BlockResponse",
                Self::BlocksRequestFrom { .. } => "BlockRequestFrom",
                Self::BlocksResponseTo { .. } => "BlockResponseTo",
                Self::BlockProposalRemote { .. } => "BlockProposalRemote",
                Self::BlockProposalLocal { .. } => "BlockProposalLocal",
                Self::BlockSolutions { .. } => "BlockSolutions",
                Self::BlockArrived { .. } => "BlockArrived",
                Self::StartFarming => "StartFarming",
                Self::StopFarming => "StopFarming",
                Self::StateUpdateRequest { .. } => "StateUpdateRequest",
                Self::StateUpdateResponse { .. } => "StateUpdateResponse",
            }
        )
    }
}

/// Starts the manager process, a broker loop that acts as the central async message hub for the node
pub async fn run(
    node_type: NodeType,
    genesis_piece_hash: [u8; 32],
    ledger: &mut ledger::Ledger,
    any_to_main_rx: Receiver<ProtocolMessage>,
    main_to_net_tx: Sender<ProtocolMessage>,
    main_to_main_tx: Sender<ProtocolMessage>,
    state_sender: crossbeam_channel::Sender<AppState>,
    timer_to_solver_tx: Sender<SolverMessage>,
    epoch_tracker: EpochTracker,
) {
    let protocol_listener = async {
        info!("Main protocol loop is running...");
        let is_farming = matches!(node_type, NodeType::Gateway | NodeType::Farmer);

        // if gateway init the genesis block set and then start the timer
        if node_type == NodeType::Gateway {
            // init ledger from genesis
            ledger.init_from_genesis().await;

            // TODO: are we sure this is starting at the exact right time?
            // start timer loop
            ledger.timer_is_running = true;

            let timer_to_solver_tx = timer_to_solver_tx.clone();
            let epoch_tracker = epoch_tracker.clone();
            async_std::task::spawn(async move {
                timer::run(
                    timer_to_solver_tx,
                    epoch_tracker,
                    CHALLENGE_LOOKBACK * TIMESLOTS_PER_EPOCH as u64,
                    true,
                )
                .await;
            });
        } else {
            // create the initial epoch
            epoch_tracker.advance_epoch().await;
        }

        loop {
            if let Ok(message) = any_to_main_rx.recv().await {
                match message {
                    ProtocolMessage::BlocksRequestFrom {
                        node_addr,
                        timeslot,
                    } => {
                        // TODO: check to make sure that the requested timeslot is not ahead of local timeslot
                        let blocks = ledger.get_blocks_by_timeslot(timeslot);

                        let message = ProtocolMessage::BlocksResponseTo {
                            node_addr,
                            blocks,
                            timeslot,
                        };
                        main_to_net_tx.send(message).await;
                    }
                    ProtocolMessage::BlocksResponse { blocks, timeslot } => {
                        // TODO: this is mainly for testing, later this will be replaced by state chain sync
                        // so there is no need for validating the block or timestamp

                        // TODO: sort the blocks lexicographically (on client or server)

                        // apply each block for the timeslot
                        for block in blocks.into_iter() {
                            ledger.apply_block_from_sync(block).await;
                        }

                        // increment the epoch on boundary
                        if (timeslot + 1) % TIMESLOTS_PER_EPOCH as u64 == 0 {
                            // create new epoch
                            let current_epoch = epoch_tracker.advance_epoch().await;

                            info!(
                                "Closed randomness for epoch {} during sync",
                                current_epoch - 1
                            );

                            info!(
                                "Created a new empty epoch during sync blocks for index {}",
                                current_epoch
                            );
                        }

                        let next_timeslot_arrival_time = Duration::from_millis(
                            ((timeslot + 1) * TIMESLOT_DURATION) + ledger.genesis_timestamp as u64,
                        );

                        let time_now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .expect("Time went backwards");

                        error!(
                            "next_timeslot_arrival_time {} time_now {}",
                            next_timeslot_arrival_time.as_millis(),
                            time_now.as_millis()
                        );

                        if next_timeslot_arrival_time < time_now {
                            // request the next timeslot
                            main_to_net_tx
                                .send(ProtocolMessage::BlocksRequest {
                                    timeslot: timeslot + 1,
                                })
                                .await;
                        } else {
                            // once we have all blocks, apply cached gossip
                            // call sync and start timer
                            info!("Applying cached blocks");
                            if !ledger.apply_cached_blocks(timeslot).await {
                                panic!("Unable to sync the ledger, invalid blocks!");
                            }

                            ledger
                                .start_timer_from_genesis_time(
                                    timer_to_solver_tx.clone(),
                                    is_farming,
                                )
                                .await;
                        }
                    }
                    ProtocolMessage::BlockProposalRemote { block, peer_addr } => {
                        info!(
                            "Received a block via gossip, with {} parents",
                            block.content.parent_ids.len()
                        );
                        let block_id = block.get_id();

                        if ledger.metablocks.contains_key(&block_id) {
                            info!("Received a block proposal via gossip for known block, ignoring");
                            continue;
                        }

                        if !ledger.timer_is_running {
                            info!(
                                "Caching a block received via gossip before the ledger is synced"
                            );
                            ledger.cache_remote_block(block);
                            continue;
                        }

                        let block_arrival_time = Duration::from_millis(
                            (block.proof.timeslot * TIMESLOT_DURATION)
                                + ledger.genesis_timestamp as u64,
                        );

                        let time_now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .expect("Time went backwards");

                        let lower_bound = time_now - EPOCH_GRACE_PERIOD;
                        let upper_bound = time_now + EPOCH_GRACE_PERIOD;

                        if block_arrival_time < lower_bound {
                            let wait_time = time_now - block_arrival_time;
                            warn!("Received an early block via gossip, waiting {} ms for block arrival!", wait_time.as_millis());

                            let sender = main_to_main_tx.clone();
                            async_std::task::spawn(async move {
                                async_std::task::sleep(wait_time).await;
                                sender
                                    .send(ProtocolMessage::BlockArrived {
                                        block,
                                        peer_addr,
                                        cached: false,
                                    })
                                    .await;
                            });

                            continue;
                        }

                        if block_arrival_time > upper_bound {
                            // block is too late, ignore
                            warn!("Received a late block via gossip, ignoring");
                            continue;
                        }

                        // check that we have the randomness for the desired epoch
                        // then apply the block

                        let randomness_epoch =
                            epoch_tracker.get_loopback_epoch(block.proof.epoch).await;

                        if !randomness_epoch.is_closed {
                            warn!("Unable to apply block received via gossip, the randomness epoch is still open!");
                        }

                        // TODO: if the timeslot is beyond the grace period, ignore the block
                        // TODO: important -- this may lead to forks if nodes are malicous
                        // TODO: this means we need to support forks in some fashion

                        // how do we know if we have to cache the block (if received during sync?)

                        // check if the block is valid and apply
                        if ledger.validate_and_apply_remote_block(block.clone()).await {
                            main_to_net_tx
                                .send(ProtocolMessage::BlockProposalRemote {
                                    block: block,
                                    peer_addr,
                                })
                                .await;
                        }
                    }
                    ProtocolMessage::BlockArrived {
                        block,
                        peer_addr,
                        cached: _,
                    } => {
                        info!(
                            "A new block has arrived with id: {}",
                            hex::encode(&block.get_id()[0..8])
                        );

                        if ledger.validate_and_apply_remote_block(block.clone()).await {
                            main_to_net_tx
                                .send(ProtocolMessage::BlockProposalRemote {
                                    block: block,
                                    peer_addr,
                                })
                                .await;
                        }

                        // ToDo: Have to wipe cached blocks at some point to prevent memory leak

                        // if cached {
                        //     // block was cached and has arrived on sync
                        //     // check for more cached pending children
                        //     if let Some(children) =
                        //         ledger.pending_children_for_parent.get(&block_id)
                        //     {
                        //         arrive_pending_children(ledger, children.clone(), &main_to_main_tx)
                        //             .await;
                        //     }
                        // }
                    }
                    ProtocolMessage::BlockSolutions { solutions } => {
                        // TODO: split into two functions
                        if solutions.is_empty() {
                            ledger.create_and_apply_local_block(None).await;
                        } else {
                            for solution in solutions.into_iter() {
                                let block = ledger
                                    .create_and_apply_local_block(Some(solution))
                                    .await
                                    .unwrap();
                                main_to_net_tx
                                    .send(ProtocolMessage::BlockProposalLocal {
                                        block: block.clone(),
                                    })
                                    .await;
                            }
                        }
                    }
                    ProtocolMessage::StateUpdateResponse { mut state } => {
                        state.node_type = node_type.to_string();
                        state.peers = state.peers + "/" + &MAX_PEERS.to_string()[..];
                        state.blocks = ledger.get_block_height().to_string();
                        state.pieces = match node_type {
                            NodeType::Gateway => PLOT_SIZE.to_string(),
                            NodeType::Farmer => PLOT_SIZE.to_string(),
                            NodeType::Peer => 0.to_string(),
                        };
                        state_sender.send(state).unwrap();
                    }
                    _ => panic!("Main protocol listener has received an unknown protocol message!"),
                }
            }
        }
    };

    let protocol_startup = async {
        info!("Calling protocol startup");

        match node_type {
            NodeType::Gateway => {
                // send genesis challenge to solver
                // this will start an eval loop = solve -> create block -> gossip -> solve ...
                info!(
                    "Starting gateway with genesis epoch challenge: {}",
                    hex::encode(&genesis_piece_hash[0..8])
                );
            }
            NodeType::Peer | NodeType::Farmer => {
                // start syncing the ledger at the genesis block
                // this will start a sync loop that should complete when fully synced
                // at that point node will simply listen and solve
                task::sleep(Duration::from_secs(1)).await;
                info!("New peer starting ledger sync with gateway");
                main_to_net_tx
                    .send(ProtocolMessage::BlocksRequest { timeslot: 0 })
                    .await;

                // on genesis block

                // init the timer
            }
        };

        // send state update requests in a loop to network
        if CONSOLE {
            loop {
                main_to_net_tx
                    .send(ProtocolMessage::StateUpdateRequest)
                    .await;
                task::sleep(Duration::from_millis(1000)).await;
            }
        }
    };

    join!(protocol_listener, protocol_startup);
}
