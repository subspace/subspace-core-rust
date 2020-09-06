use crate::crypto;
use crate::utils;
use crate::BlockId;
use crate::EpochChallenge;
use crate::SlotChallenge;
use crate::TIMESLOTS_PER_EPOCH;
use log::{debug, warn};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Epoch {
    /// has the randomness been derived and the epoch closed?
    pub is_closed: bool,
    /// timeslot indices and vec of block ids, some will be empty, some one, some many
    timeslots: HashMap<u64, Vec<BlockId>>,
    /// challenges derived from randomness at closure, one per timeslot
    challenges: Vec<SlotChallenge>,
    /// overall randomness for this epoch
    pub randomness: EpochChallenge,
}

// TODO: Make into an enum for a cleaner implementation, separate into active and closed epoch
impl Epoch {
    pub(super) fn new(index: u64) -> Epoch {
        let randomness = crypto::digest_sha_256(&index.to_le_bytes());

        Epoch {
            is_closed: false,
            timeslots: HashMap::new(),
            challenges: Vec::with_capacity(TIMESLOTS_PER_EPOCH as usize),
            randomness,
        }
    }

    /// Returns `true` in case no blocks for this timeslot existed before
    pub(super) fn add_block_to_timeslot(&mut self, timeslot: u64, block_id: BlockId) {
        if self.is_closed {
            warn!("Epoch already closed, skipping adding block to time slot");
            return;
        }
        debug!("Adding block to time slot");
        let timeslot_index = timeslot % TIMESLOTS_PER_EPOCH;
        self.timeslots
            .entry(timeslot_index)
            .and_modify(|list| {
                list.push(block_id);
            })
            .or_insert_with(|| vec![block_id]);
    }

    pub fn get_challenge_for_timeslot(&self, timeslot: u64) -> SlotChallenge {
        // TODO: this should panic if the epoch is still open
        let timeslot_index = timeslot % TIMESLOTS_PER_EPOCH;
        // TODO: No guarantee index exists
        self.challenges[timeslot_index as usize]
    }

    pub(super) fn close(&mut self) {
        let xor_result =
            self.timeslots
                .values()
                .flatten()
                .fold(self.randomness, |mut randomness, block_id| {
                    utils::xor_bytes(&mut randomness, &block_id[..]);
                    randomness
                });
        self.randomness = crypto::digest_sha_256(&xor_result);
        println!(
            "{:?}",
            self.timeslots
                .values()
                .flatten()
                .map(|x| hex::encode(&x))
                .collect::<Vec<_>>()
        );

        for timeslot_index in 0..TIMESLOTS_PER_EPOCH {
            let slot_seed = [&self.randomness[..], &timeslot_index.to_le_bytes()[..]].concat();
            self.challenges.push(crypto::digest_sha_256(&slot_seed));
        }

        self.is_closed = true;
    }
}
