use crate::block::{Block, Content, Data, Proof};
use crate::farmer::{FarmerMessage, Solution};
use crate::timer::EpochTracker;
use crate::transaction::{AccountAddress, AccountState, CoinbaseTx, Transaction, TxId};
use crate::{
    crypto, sloth, timer, BlockId, ContentId, ProofId, Tag, BLOCK_REWARD,
    CHALLENGE_LOOKBACK_EPOCHS, PRIME_SIZE_BITS, TIMESLOTS_PER_EPOCH, TIMESLOT_DURATION,
};

use crate::metablocks::MetaBlocks;
use async_std::sync::Sender;
use log::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::TryInto;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/* ToDo
 * ----
 *
 * Fix: self-adjusting difficulty
 * Track chain quality
 * Ensure that commits to the ledger are fully atomic
 *
 * TESTING
 * -------
 * Piece count is always 256 for testing for the merkle tree
 * Plot size is configurable, but must be a multiple of 256
 * For each challenge, solver will check every 256th piece starting at index and return the top N
 * We want to start with 256 x 256 pieces
 * This mean for each challenge the expected quality should be below 2^32 / 2^8 -> 2^24
 *
*/

/*
    X 1. Replace block_id with proof_id throughout
    X 2. Solve blocks (choose correct parent)
    X 3. Stage blocks
    X 4. Apply blocks (order then apply tx to balances)
    5. Derive randomness for next epoch
    6. Sync in the happy path
    X 7. Apply transactions
    8. Sync with late blocks

    Case 1: Single block in each timeslot (some may be skipped)
    Case 2: Multiple blocks in each timeslot
    Case 3: Multiple blocks in successive timeslots
    Case 4: Late blocks (seen by all in the next round)
    Case 5: Unseen pointers (seen by some in the round, some in the next)

*/

pub type BlockHeight = u64;

#[derive(Debug, Clone)]
pub struct PendingBlock {
    content_id: ContentId,
    // TODO: maybe don't need here since we have BlockStatus
    is_referenced: bool,
}

pub struct Ledger {
    pub balances: HashMap<AccountAddress, AccountState>,
    pub metablocks: MetaBlocks,
    pub pending_blocks_by_height: BTreeMap<BlockHeight, BTreeMap<ProofId, PendingBlock>>,
    pub applied_blocks_by_height: BTreeMap<BlockHeight, Vec<ProofId>>,
    // pub ordered_proof_ids_by_timeslot: HashMap<u64, Vec<ProofId>>,
    pub cached_proof_ids_by_timeslot: BTreeMap<u64, Vec<ProofId>>,
    pub txs: HashMap<TxId, Transaction>,
    pub tx_mempool: HashSet<TxId>,
    pub epoch_tracker: EpochTracker,
    pub timer_is_running: bool,
    pub quality: u32,
    pub keys: ed25519_dalek::Keypair,
    pub sloth: sloth::Sloth,
    pub genesis_timestamp: u64,
    pub genesis_piece_hash: [u8; 32],
    pub merkle_root: Vec<u8>,
    pub merkle_proofs: Vec<Vec<u8>>,
    pub tx_payload: Vec<u8>,
}

impl Ledger {
    pub fn new(
        merkle_root: Vec<u8>,
        genesis_piece_hash: [u8; 32],
        keys: ed25519_dalek::Keypair,
        tx_payload: Vec<u8>,
        merkle_proofs: Vec<Vec<u8>>,
        epoch_tracker: EpochTracker,
    ) -> Ledger {
        // init sloth
        let prime_size = PRIME_SIZE_BITS;
        let sloth = sloth::Sloth::init(prime_size);

        // TODO: all of these data structures need to be periodically truncated
        Ledger {
            balances: HashMap::new(),
            metablocks: MetaBlocks::new(),
            txs: HashMap::new(),
            tx_mempool: HashSet::new(),
            applied_blocks_by_height: BTreeMap::new(),
            // ordered_proof_ids_by_timeslot: HashMap::new(),
            cached_proof_ids_by_timeslot: BTreeMap::new(),
            pending_blocks_by_height: BTreeMap::new(),
            genesis_timestamp: 0,
            timer_is_running: false,
            quality: 0,
            epoch_tracker,
            merkle_root,
            genesis_piece_hash,
            sloth,
            keys,
            tx_payload,
            merkle_proofs,
        }
    }

    pub fn get_blocks_by_height(&self, height: u64) -> Vec<Block> {
        self.applied_blocks_by_height
            .get(&height)
            .map(|proof_ids| {
                proof_ids
                    .iter()
                    .map(|proof_id| self.metablocks.blocks.get(proof_id).unwrap().block.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Start a new chain from genesis as a gateway node
    pub async fn init_from_genesis(&mut self, timer_to_solver_tx: Sender<FarmerMessage>) {
        self.genesis_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;

        let mut timestamp = self.genesis_timestamp as u64;
        let mut parent_id: BlockId = [0u8; 32];

        for _ in 0..CHALLENGE_LOOKBACK_EPOCHS {
            let current_epoch_index = self.epoch_tracker.advance_epoch().await;
            let current_epoch = self.epoch_tracker.get_epoch(current_epoch_index).await;
            info!(
                "Advanced to epoch {} during genesis init",
                current_epoch_index
            );

            for current_timeslot in (0..TIMESLOTS_PER_EPOCH)
                .map(|timeslot_index| timeslot_index + current_epoch_index * TIMESLOTS_PER_EPOCH)
            {
                let proof = Proof {
                    randomness: self.genesis_piece_hash,
                    epoch: current_epoch_index,
                    timeslot: current_timeslot,
                    public_key: self.keys.public.to_bytes(),
                    tag: Tag::default(),
                    // TODO: Fix this
                    nonce: u64::from_le_bytes(
                        crypto::create_hmac(&[], b"subspace")[0..8]
                            .try_into()
                            .unwrap(),
                    ),
                    piece_index: 0,
                    solution_range: current_epoch.solution_range,
                };

                let proof_id = proof.get_id();
                let coinbase_tx = CoinbaseTx::new(BLOCK_REWARD, self.keys.public, proof_id);

                let mut content = Content {
                    parent_id,
                    uncle_ids: vec![],
                    proof_id,
                    proof_signature: self.keys.sign(&proof_id).to_bytes().to_vec(),
                    timestamp,
                    tx_ids: vec![coinbase_tx.get_id()],
                    signature: Vec::new(),
                };

                content.signature = self.keys.sign(&content.get_id()).to_bytes().to_vec();

                let data = Data {
                    encoding: Vec::new(),
                    merkle_proof: Vec::new(),
                };

                let block = Block {
                    proof,
                    coinbase_tx,
                    content,
                    data: Some(data),
                };

                // prepare the block for application to the ledger
                self.stage_block(&block).await;

                parent_id = block.content.get_id();

                debug!(
                    "Applied a genesis block to ledger with content id {}",
                    hex::encode(&parent_id[0..8])
                );
                let time_now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Time went backwards")
                    .as_millis();

                timestamp += TIMESLOT_DURATION;

                //TODO: this should wait for the correct time to arrive rather than waiting for a fixed amount of time
                async_std::task::sleep(Duration::from_millis(timestamp - time_now as u64)).await;

                // order blocks and apply transactions
                self.apply_referenced_blocks().await;
            }
        }

        self.start_timer(
            timer_to_solver_tx,
            CHALLENGE_LOOKBACK_EPOCHS * TIMESLOTS_PER_EPOCH as u64,
            true,
        );
    }

    /// Searches for a pending block and references it, if not yet referenced
    /// returns false if the pending block is not found
    pub fn reference_pending_block(&mut self, proof_id: &ProofId) -> bool {
        for pending_blocks in self.pending_blocks_by_height.values_mut() {
            match pending_blocks.get_mut(proof_id) {
                Some(pending_block) => {
                    self.metablocks.reference(*proof_id);
                    pending_block.is_referenced = true;
                    return true;
                }
                None => {}
            };
        }

        false
    }

    pub fn apply_pending_block(&mut self, proof_id: &ProofId, block_height: &u64) {
        self.metablocks.apply(*proof_id);
        let pending_blocks = self
            .pending_blocks_by_height
            .get_mut(&block_height)
            .unwrap();
        pending_blocks.remove(proof_id);
        if pending_blocks.len() == 0 {
            self.pending_blocks_by_height.remove(&block_height);
        }
    }

    /// Prepare the block for application once it is "seen" by some other block
    async fn stage_block(&mut self, block: &Block) {
        // save the coinbase tx
        self.txs.insert(
            block.coinbase_tx.get_id(),
            Transaction::Coinbase(block.coinbase_tx.clone()),
        );

        let mut pruned_block = block.clone();
        pruned_block.prune();
        // TODO: Everything that happens here may need to be reversed if `add_block_to_epoch()` at
        //  the end fails, which implies that this function should have a lock and not be called
        //  concurrently
        let metablock = self.metablocks.stage(pruned_block);

        // skip the genesis block
        if block.proof.timeslot != 0 {
            // collect parent and uncle pointers seen by this block
            let mut content_ids_seen_by_this_block = block.content.uncle_ids.clone();
            content_ids_seen_by_this_block.push(block.content.parent_id);

            // attempt to reference each pending block seen
            content_ids_seen_by_this_block
                .iter()
                .map(|content_id| {
                    self.metablocks
                        .get_proof_id_from_content_id(*content_id)
                        .clone()
                })
                .collect::<Vec<ProofId>>()
                .iter()
                .for_each(|proof_id| {
                    // if it doesn't show as a pending block panic
                    if !self.reference_pending_block(&proof_id) {
                        // TODO: this should instead discard the block or wait for its parents
                        panic!("Cannot stage block that references an unknown content block");
                    }
                });
        }

        let pending_block = PendingBlock {
            content_id: metablock.content_id,
            is_referenced: false,
        };

        // add the block to pending blocks
        self.pending_blocks_by_height
            .entry(metablock.height)
            .and_modify(|pending_blocks| {
                pending_blocks.insert(metablock.content_id, pending_block.clone());
            })
            .or_insert({
                let mut btreemap = BTreeMap::new();
                btreemap.insert(metablock.proof_id, pending_block);
                btreemap
            });

        // Adds a pointer to this block id for the given timeslot in the ledger
        // Sorts on each insertion
    }

    /// order all seen blocks and apply transactions
    async fn apply_referenced_blocks(&mut self) {
        // apply highest block first, then smallest proof
        for (block_height, pending_blocks) in self.pending_blocks_by_height.clone().iter().rev() {
            for (proof_id, pending_block) in pending_blocks.iter() {
                if pending_block.is_referenced == true {
                    // get the block
                    let metablock = self.metablocks.blocks.get(proof_id).unwrap().clone();
                    let block = metablock.block;

                    // change state to applied and remove from pending blocks
                    self.apply_pending_block(proof_id, block_height);

                    // add to applied blocks by height
                    self.applied_blocks_by_height
                        .entry(metablock.height)
                        .and_modify(|proof_ids| {
                            proof_ids.push(*proof_id);
                            proof_ids.sort();
                        })
                        .or_insert(vec![*proof_id]);

                    // TODO: apply block to state buffer

                    // apply all txs that have not been applied (may be duplicates)

                    // apply the coinbase tx
                    match self.txs.get(&block.content.tx_ids[0]).unwrap() {
                        Transaction::Coinbase(tx) => {
                            // create or update account state
                            self.balances
                                .entry(tx.to_address)
                                .and_modify(|account_state| account_state.balance += BLOCK_REWARD)
                                .or_insert(AccountState {
                                    nonce: 0,
                                    balance: BLOCK_REWARD,
                                });

                            // TODO: add to state, may remove from tx db here
                        }
                        _ => panic!("The first tx must be a coinbase tx"),
                    };

                    // apply remaining credit txs
                    for tx_id in block.content.tx_ids.iter().skip(1) {
                        // make sure the first is a coinbase tx

                        match self.txs.get(tx_id).unwrap() {
                            Transaction::Credit(tx) => {
                                // check if the tx has already been applied
                                if !self.tx_mempool.contains(tx_id) {
                                    warn!("Transaction has already been referenced by a previous block, skipping");
                                    continue;
                                }

                                // ensure the tx is still valid
                                let sender_account_state =
                                    self.balances.get(&tx.from_address).expect(
                                        "Existence of account state has already been validated",
                                    );

                                if sender_account_state.balance < tx.amount {
                                    error!("Invalid transaction, from account state has insufficient funds, transaction will not be applied");
                                    continue;
                                }

                                if sender_account_state.nonce >= tx.nonce {
                                    error!("Invalid transaction, tx nonce has already been used, transaction will not be applied");
                                    continue;
                                }

                                // debit the sender
                                self.balances
                                    .entry(tx.from_address)
                                    .and_modify(|account_state| account_state.balance -= tx.amount);

                                // credit  the receiver
                                self.balances
                                    .entry(tx.to_address)
                                    .and_modify(|account_state| account_state.balance += tx.amount)
                                    .or_insert(AccountState {
                                        nonce: 0,
                                        balance: tx.amount,
                                    });

                                // TODO: pay tx fee to farmer

                                // remove from mem pool
                                self.tx_mempool.remove(tx_id);

                                // TODO: apply tx to state buffer, may remove from tx db here...
                            }
                            _ => panic!("Only the first tx may be a coinbase tx"),
                        };
                    }

                    // add to epoch tracker
                    self.epoch_tracker
                        .add_block_to_epoch(
                            block.proof.epoch,
                            block.proof.timeslot,
                            *proof_id,
                            block.proof.solution_range,
                        )
                        .await;

                    // TODO: update chain quality
                }
            }
        }
    }

    /// roll back a block and all tx in the event of a fork and re-org
    async fn _revert_referenced_block(&mut self) {
        // TODO: complete this to handle forks and re-orgs
    }

    /// create a new block locally from a valid farming solution
    pub async fn create_and_apply_local_block(&mut self, solution: Solution) -> Block {
        let proof = Proof {
            randomness: solution.randomness,
            epoch: solution.epoch_index,
            timeslot: solution.timeslot,
            public_key: self.keys.public.to_bytes(),
            tag: solution.tag,
            // TODO: Fix this
            nonce: u64::from_le_bytes(
                crypto::create_hmac(&solution.encoding, b"subspace")[0..8]
                    .try_into()
                    .unwrap(),
            ),
            piece_index: solution.piece_index,
            solution_range: solution.solution_range,
        };
        let data = Data {
            encoding: solution.encoding.to_vec(),
            merkle_proof: crypto::get_merkle_proof(solution.proof_index, &self.merkle_proofs),
        };
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;

        // for the largest block height, take the smallest proof as parent block
        let (_, pending_block) = self
            .pending_blocks_by_height
            .last_key_value()
            .expect("There should always be at least one pending level")
            .1
            .first_key_value()
            .expect("There should always be at least on pending block for each level");

        let longest_content_id = pending_block.content_id;

        debug!(
            "Parent content id for locally created block is: {}",
            hex::encode(&longest_content_id[0..8])
        );

        // TODO: this is not correct
        // get all other unseen blocks as uncles
        let unseen_uncles = self
            .pending_blocks_by_height
            .values()
            .map(|pending_blocks| pending_blocks.values())
            .flatten()
            .filter(|pending_block| pending_block.is_referenced == false)
            .map(|pending_block| pending_block.content_id)
            .filter(|content_id| content_id != &longest_content_id)
            .collect();

        // create the coinbase tx
        let proof_id = proof.get_id();
        let coinbase_tx = CoinbaseTx::new(BLOCK_REWARD, self.keys.public, proof_id);
        let mut tx_ids = vec![coinbase_tx.get_id()];

        // add all txs in the mempool, sorted by hash
        let mut pending_tx_ids: Vec<TxId> = self.tx_mempool.iter().cloned().collect();
        pending_tx_ids.sort();
        for tx_id in pending_tx_ids.into_iter() {
            tx_ids.push(tx_id);
        }

        let mut content = Content {
            parent_id: longest_content_id,
            uncle_ids: unseen_uncles,
            proof_id,
            proof_signature: self.keys.sign(&proof.get_id()).to_bytes().to_vec(),
            timestamp,
            tx_ids,
            signature: Vec::new(),
        };

        content.signature = self.keys.sign(&content.get_id()).to_bytes().to_vec();

        let block = Block {
            proof,
            coinbase_tx,
            content,
            data: Some(data),
        };

        // get correct randomness for this block
        let epoch = self
            .epoch_tracker
            .get_lookback_epoch(block.proof.epoch)
            .await;

        // let challenge_timeslot =
        //     block.proof.timeslot - CHALLENGE_LOOKBACK * TIMESLOTS_PER_EPOCH;

        if !epoch.is_closed {
            panic!("Epoch being used for randomness is still open!");
        }

        // check if the block is valid
        let is_valid = block.is_valid(
            &self.merkle_root,
            &self.genesis_piece_hash,
            &epoch.randomness,
            &epoch.get_challenge_for_timeslot(block.proof.timeslot),
            &self.sloth,
        );
        assert!(is_valid, "Local block must always be valid");

        // stage the block for application once it is referenced
        self.stage_block(&block).await;

        block
    }

    /// cache a block received via gossip ahead of the current epoch
    pub fn cache_remote_block(&mut self, block: Block) {
        // cache the block
        // TODO: Does this need to be inserted here at all?
        self.metablocks.cache(block.clone());

        let proof_id = block.proof.get_id();

        // add to cached blocks tracker
        self.cached_proof_ids_by_timeslot
            .entry(block.proof.timeslot)
            .and_modify(|proof_ids| proof_ids.push(proof_id))
            .or_insert(vec![proof_id]);
    }

    /// validate and apply a block received via gossip
    pub async fn validate_and_apply_remote_block(&mut self, block: Block) -> bool {
        debug!(
            "Validating and applying block for epoch: {} at timeslot {}",
            block.proof.epoch, block.proof.timeslot
        );

        // get correct randomness for this block
        let epoch = self
            .epoch_tracker
            .get_lookback_epoch(block.proof.epoch)
            .await;

        if !epoch.is_closed {
            panic!("Epoch being used for randomness is still open!");
        }

        // check if the block is valid
        if !block.is_valid(
            &self.merkle_root,
            &self.genesis_piece_hash,
            &epoch.randomness,
            &epoch.get_challenge_for_timeslot(block.proof.timeslot),
            &self.sloth,
        ) {
            return false;
        }

        // TODO: validate all transactions for this block

        // apply the block to the ledger
        self.stage_block(&block).await;

        // TODO: apply children of this block that were depending on it

        true
    }

    // TODO: Where is validation???
    /// validate a block received via sync from another node
    pub async fn apply_block_from_sync(&mut self, block: Block) {
        if self.genesis_timestamp == 0 {
            self.genesis_timestamp = block.content.timestamp;
        }
        let block_id = block.get_id();
        // apply the block to the ledger
        self.stage_block(&block).await;

        // check if the block is in pending gossip and remove
        {
            let mut is_empty = false;
            self.cached_proof_ids_by_timeslot
                .entry(block.proof.timeslot)
                .and_modify(|block_ids| {
                    block_ids
                        .iter()
                        .position(|blk_id| *blk_id == block_id)
                        .map(|index| block_ids.remove(index));
                    is_empty = block_ids.is_empty();
                });
            if is_empty {
                self.cached_proof_ids_by_timeslot
                    .remove(&block.proof.timeslot);
            }
        }

        debug!(
            "Applied new block during sync at timeslot: {}",
            block.proof.timeslot
        );
    }

    /// validate and apply a cached block from gossip after syncing the ledger
    pub async fn validate_and_apply_cached_block(&mut self, block: Block) -> bool {
        //TODO: must handle the case where the epoch is still open

        let randomness_epoch_index = block.proof.epoch - CHALLENGE_LOOKBACK_EPOCHS;
        let challenge_timeslot = block.proof.timeslot;
        info!(
            "Validating and applying cached block for epoch: {} at timeslot {}",
            randomness_epoch_index, challenge_timeslot
        );

        // get correct randomness for this block
        let epoch = self.epoch_tracker.get_epoch(randomness_epoch_index).await;

        // check if the block is valid
        if !block.is_valid(
            &self.merkle_root,
            &self.genesis_piece_hash,
            &epoch.randomness,
            &epoch.get_challenge_for_timeslot(challenge_timeslot),
            &self.sloth,
        ) {
            return false;
        }

        // apply the block to the ledger
        self.stage_block(&block).await;

        true
    }

    /// apply the cached block to the ledger
    ///
    /// Returns last (potentially unfinished) timeslot
    pub async fn apply_cached_blocks(&mut self, timeslot: u64) -> Result<u64, ()> {
        for current_timeslot in timeslot.. {
            if let Some(proof_ids) = self.cached_proof_ids_by_timeslot.remove(&current_timeslot) {
                for proof_id in proof_ids.iter() {
                    let cached_block = self.metablocks.blocks.get(proof_id).unwrap().block.clone();
                    if !self.validate_and_apply_cached_block(cached_block).await {
                        return Err(());
                    }
                }
            }

            if self.cached_proof_ids_by_timeslot.is_empty() {
                return Ok(current_timeslot);
            }

            if current_timeslot % TIMESLOTS_PER_EPOCH as u64 == 0 {
                // create the new epoch
                let current_epoch = self.epoch_tracker.advance_epoch().await;

                debug!(
                    "Closed randomness for epoch {} during apply cached blocks",
                    current_epoch - CHALLENGE_LOOKBACK_EPOCHS
                );

                debug!("Creating a new empty epoch for epoch {}", current_epoch);
            }
        }

        Ok(timeslot)
    }

    /// start the timer after syncing the ledger
    pub fn start_timer(
        &mut self,
        timer_to_farmer_tx: Sender<FarmerMessage>,
        elapsed_timeslots: u64,
        is_farming: bool,
    ) {
        self.timer_is_running = true;

        async_std::task::spawn(timer::run(
            timer_to_farmer_tx,
            self.epoch_tracker.clone(),
            elapsed_timeslots,
            is_farming,
            self.genesis_timestamp,
        ));
    }

    /// Retrieve the balance for a given node id
    pub fn get_account_state(&self, id: &[u8]) -> Option<AccountState> {
        self.balances.get(id).copied()
    }

    /// Print the balance of all accounts in the ledger
    pub fn print_balances(&self) {
        info!("Current balance of accounts:\n");
        for (id, account_state) in self.balances.iter() {
            info!(
                "Account: {} \t {} \t credits",
                hex::encode(id),
                account_state.balance
            );
        }
    }
}

// #[cfg(test)]
// mod tests {

//     use super::*;
//     use std::time::{SystemTime, UNIX_EPOCH};

//     // #[test]
//     // fn block() {
//     //     let tx_payload = crypto::generate_random_piece().to_vec();
//     //     let block = Block::new(
//     //         SystemTime::now()
//     //             .duration_since(UNIX_EPOCH)
//     //             .expect("Time went backwards")
//     //             .as_millis(),
//     //         crypto::random_bytes_32(),
//     //         crypto::random_bytes_32(),
//     //         crypto::random_bytes_32(),
//     //         crypto::random_bytes_32(),
//     //         [0u8; 64].to_vec(),
//     //         tx_payload,
//     //     );
//     //     let block_id = block.get_id();
//     //     let block_vec = block.to_bytes();
//     //     let block_copy = Block::from_bytes(&block_vec).unwrap();
//     //     let block_copy_id = block_copy.get_id();
//     //     assert_eq!(block_id, block_copy_id);
//     // }

//     // #[test]
//     // fn auxiliary_data() {
//     //     let encoding = crypto::generate_random_piece();
//     //     let (merkle_proofs, _) = crypto::build_merkle_tree();
//     //     let proof = Proof::new(encoding, merkle_proofs[17].clone(), 17u64, 245u64);
//     //     let proof_id = proof.get_id();
//     //     let proof_vec = proof.to_bytes();
//     //     let proof_copy = Proof::from_bytes(&proof_vec).unwrap();
//     //     let proof_copy_id = proof_copy.get_id();
//     //     assert_eq!(proof_id, proof_copy_id);
//     // }
// }
