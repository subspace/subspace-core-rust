#![feature(try_blocks)]
#![feature(drain_filter)]

use async_std::sync::{Arc, Mutex};
use std::collections::HashMap;
use std::time::Duration;

pub mod console;
pub mod crypto;
pub mod ledger;
pub mod manager;
pub mod network;
pub mod plot;
pub mod plotter;
pub mod pseudo_wallet;
pub mod sloth;
pub mod solver;
pub mod timer;
pub mod utils;

// TODO: Should make into actual structs
pub type Piece = [u8; PIECE_SIZE];
pub type IV = [u8; IV_SIZE];
pub type NodeID = IV;
pub type Tag = u64;
pub type BlockId = [u8; 32];
pub type ProofId = [u8; 32];
pub type ContentId = [u8; 32];
pub type PublicKey = [u8; 32];
pub type ExpandedIV = [u8; PRIME_SIZE_BYTES];
pub type EpochRandomness = Arc<Mutex<HashMap<u64, [u8; 32]>>>;
pub type EpochChallenge = [u8; 32];
pub type SlotChallenge = [u8; 32];

pub const PRIME_SIZE_BITS: usize = 256;
pub const PRIME_SIZE_BYTES: usize = PRIME_SIZE_BITS / 8;
pub const IV_SIZE: usize = 32;
pub const PIECE_SIZE: usize = 4096;
pub const PIECE_COUNT: usize = 256;
pub const REPLICATION_FACTOR: usize = 256;
pub const PLOT_SIZE: usize = PIECE_COUNT * REPLICATION_FACTOR;
pub const BLOCKS_PER_ENCODING: usize = PIECE_SIZE / PRIME_SIZE_BYTES;
pub const ENCODING_LAYERS_TEST: usize = 1;
pub const ENCODING_LAYERS_PROD: usize = BLOCKS_PER_ENCODING;
pub const PLOT_UPDATE_INTERVAL: usize = 10000;
pub const MAX_PEERS: usize = 8;
pub const INITIAL_QUALITY_THRESHOLD: u8 = 0;
pub const CONFIRMATION_DEPTH: usize = 6;
pub const DEV_GATEWAY_ADDR: &str = "127.0.0.1:8080";
pub const TEST_GATEWAY_ADDR: &str = "127.0.0.1:8080";
pub const CONSOLE: bool = false;
// TODO: build duration object here and only define once
// TODO: add documentation on allowed parameters for time
pub const TIMESLOT_DURATION: u64 = 250;
pub const CHALLENGE_LOOKBACK: u64 = 4;
pub const TIMESLOTS_PER_EPOCH: usize = 4;
pub const EPOCH_GRACE_PERIOD: Duration =
    Duration::from_millis(TIMESLOTS_PER_EPOCH as u64 * TIMESLOT_DURATION);
pub const SOLUTION_RANGE: u64 = std::u64::MAX / PLOT_SIZE as u64 / 2;
