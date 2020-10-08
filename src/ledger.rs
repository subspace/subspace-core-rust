use crate::block::{Block, Content, Data, Proof};
use crate::farmer::Solution;
use crate::timer::EpochTracker;
use crate::transaction::{AccountAddress, AccountState, CoinbaseTx, Transaction, TxId};
use crate::{
    crypto, sloth, ContentId, ProofId, Tag, BLOCK_REWARD, CHALLENGE_LOOKBACK_EPOCHS,
    CONFIRMATION_DEPTH, MAX_EARLY_TIMESLOTS, MAX_LATE_TIMESLOTS, PRIME_SIZE_BITS,
    TIMESLOTS_PER_EPOCH, TIMESLOT_DURATION,
};

use crate::metablocks::{MetaBlock, MetaBlocks};
use async_std::task::JoinHandle;
use log::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::TryInto;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/* TESTING
 * Piece count is always 256 for testing for the merkle tree
 * Plot size is configurable, but must be a multiple of 256
 * For each challenge, solver will check every 256th piece starting at index and return the top N
 * We want to start with 256 x 256 pieces
 * This mean for each challenge the expected quality should be below 2^32 / 2^8 -> 2^24
 *
*/

// TODO: can we sync blocks by epoch

// sync process
// each proposer block and state block is stored in blocks by timeslot
// request at each timeslot and return all blocks for that slot
// bundle any transactions with transaction blocks

// add a notion of tx blocks

pub type BlockHeight = u64;
pub type Timeslot = u64;

pub struct Head {
    block_height: u64,
    content_id: ContentId,
}

// block: cached || staged
// cached due to: received before sync or blocks found close together

// on receipt of new block via gossip
// if the parent is in metablocks -> stage
// else -> cache into cached_blocks_by_pending_parent: <ContentId, Vec<Block>>
// on receipt of parent -> stage cached block

// what to do when we are syncing blocks?
// cache all incoming gossip
// request blocks from peer by timeslot, validate and stage
// if block is cached, remove from cache
// once we arrive at the current timeslot, apply all cached gossip

pub struct Ledger {
    /// the current confirmed credit balance of all subspace accounts
    pub balances: HashMap<AccountAddress, AccountState>,
    /// storage container for blocks with metadata
    pub metablocks: MetaBlocks,
    /// proof_ids for the last N blocks, to prevent duplicate gossip and content spamming
    pub recent_proof_ids: HashSet<ProofId>,
    /// record that allows for syncing the ledger by timeslot
    pub proof_ids_by_timeslot: BTreeMap<Timeslot, Vec<ProofId>>,
    /// container for blocks received who have an unknown parent
    pub cached_blocks_by_parent_content_id: HashMap<ContentId, Vec<Block>>,
    /// temporary container for blocks seen before their timeslot has arrived
    pub early_blocks_by_timeslot: BTreeMap<Timeslot, Vec<Block>>,
    /// all confirmed proposer blocks
    pub blocks_on_longest_chain: HashSet<ProofId>,
    /// fork tracker for pending blocks, used to find the current head of longest chain
    pub heads: Vec<Head>,
    /// container for all txs
    pub txs: HashMap<TxId, Transaction>,
    /// tracker for txs that have not yet been included in a tx block
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
    pub current_timeslot: u64,
    timer_handle: Option<JoinHandle<()>>,
}

impl Drop for Ledger {
    fn drop(&mut self) {
        let timer_handle: JoinHandle<()> = self.timer_handle.take().unwrap();
        async_std::task::spawn(async move {
            timer_handle.cancel().await;
        });
    }
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

        // spawn a background task
        // assign to join_handle

        let timer_handle = async_std::task::spawn(async {
            // TODO: listen on the channel

            // listen for the next timeslot
            // increment the timeslot count
            // stage early blocks for that timeslot
        });

        // TODO: all of these data structures need to be periodically truncated
        Ledger {
            balances: HashMap::new(),
            metablocks: MetaBlocks::new(),
            recent_proof_ids: HashSet::new(),
            proof_ids_by_timeslot: BTreeMap::new(),
            cached_blocks_by_parent_content_id: HashMap::new(),
            early_blocks_by_timeslot: BTreeMap::new(),
            blocks_on_longest_chain: HashSet::new(),
            heads: Vec::new(),
            txs: HashMap::new(),
            tx_mempool: HashSet::new(),
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
            current_timeslot: 0,
            timer_handle: Some(timer_handle),
        }
    }

    /// Update the timeslot, then validates and stages all early blocks that have arrived
    pub async fn next_timeslot(&mut self) {
        self.current_timeslot += 1;

        // apply all early blocks
        if self
            .early_blocks_by_timeslot
            .contains_key(&self.current_timeslot)
        {
            for block in self
                .early_blocks_by_timeslot
                .get(&self.current_timeslot)
                .unwrap()
                .clone()
            {
                debug!("Timeslot has arrived for early block, validating and staging");

                if self.is_valid_proposer_block_that_has_arrived(&block).await {
                    // TODO: have to make sure we don't reference the block (just check for last timeslot in create block)
                    self.stage_block(&block).await;
                }
            }

            self.early_blocks_by_timeslot.remove(&self.current_timeslot);
        }
    }

    /// Returns all blocks seen for a given timeslot
    pub fn get_blocks_by_timeslot(&self, timeslot: u64) -> Vec<Block> {
        self.proof_ids_by_timeslot
            .get(&timeslot)
            .map(|proof_ids| {
                proof_ids
                    .iter()
                    .map(|proof_id| self.metablocks.blocks.get(proof_id).unwrap().block.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// returns the tip of the longest chain as seen by this node
    pub fn get_head(&self) -> ContentId {
        self.heads[0].content_id
    }

    /// updates an existing branch, setting to head if longest, or creates a new branch
    pub fn update_heads(&mut self, parent_id: ContentId, content_id: ContentId, block_height: u64) {
        for (index, head) in self.heads.iter_mut().enumerate() {
            if head.content_id == parent_id {
                // updated existing head
                head.block_height += 1;
                head.content_id = content_id;

                // check if existing branch has overtaken the current head
                if index != 0 && head.block_height > self.heads[0].block_height {
                    self.heads.swap(0, index);
                }
                return;
            }
        }

        // else create a new branch -- cannot be longest head (unless first head)
        self.heads.push(Head {
            content_id,
            block_height,
        });
    }

    /// removes a branch that is equal to the current confirmed ledger
    pub fn prune_branch(&mut self, content_id: ContentId) {
        let mut remove_index: Option<usize> = None;
        for (index, head) in self.heads.iter().enumerate() {
            if head.content_id == content_id {
                if index == 0 {
                    panic!("Cannot prune head of the longest chain!");
                }

                remove_index = Some(index);
            }
        }

        self.heads.remove(remove_index.expect("Branch must exist"));
    }

    /// Start a new chain from genesis as a gateway node
    // TODO: this should solve from some genesis state block
    pub async fn init_from_genesis(&mut self) -> u64 {
        self.genesis_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;

        let mut timestamp = self.genesis_timestamp as u64;
        let mut parent_id: ContentId = [0u8; 32];

        for _ in 0..CHALLENGE_LOOKBACK_EPOCHS {
            let current_epoch_index = self
                .epoch_tracker
                .advance_epoch(&self.blocks_on_longest_chain)
                .await;
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
            }
        }

        self.genesis_timestamp
    }

    /// create a new block locally from a valid farming solution
    pub async fn create_and_apply_local_block(
        &mut self,
        solution: Solution,
        sibling_content_ids: Vec<ContentId>,
    ) -> Block {
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

        let mut longest_content_id = self.get_head();
        if sibling_content_ids
            .iter()
            .any(|content_id| content_id == &longest_content_id)
        {
            // the block is referencing a sibling, get its parent instead
            let sibling_proof_id = self
                .metablocks
                .content_to_proof_map
                .get(&longest_content_id)
                .expect("Sibling is in metablocks");

            let sibling_metablock = self
                .metablocks
                .blocks
                .get(sibling_proof_id)
                .expect("Sibling is in metablocks");

            longest_content_id = sibling_metablock.block.content.parent_id;
        }

        debug!(
            "Parent content id for locally created block is: {}",
            hex::encode(&longest_content_id[0..8])
        );

        // create the coinbase tx
        let proof_id = proof.get_id();
        let coinbase_tx = CoinbaseTx::new(BLOCK_REWARD, self.keys.public, proof_id);
        let mut tx_ids = vec![coinbase_tx.get_id()];

        // TODO: split between proposer and tx block

        // add all txs in the mempool, sorted by hash
        let mut pending_tx_ids: Vec<TxId> = self.tx_mempool.iter().cloned().collect();
        pending_tx_ids.sort();
        for tx_id in pending_tx_ids.into_iter() {
            tx_ids.push(tx_id);
        }

        let mut content = Content {
            parent_id: longest_content_id,
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

        let is_valid = self.validate_block(&block).await;
        assert!(is_valid, "Local block must always be valid");

        block
    }

    /// Validates that a block is internally consistent
    async fn validate_block(&self, block: &Block) -> bool {
        // get correct randomness for this block
        let epoch = self
            .epoch_tracker
            .get_lookback_epoch(block.proof.epoch)
            .await;

        if !epoch.is_closed {
            // TODO: ensure this cannot be exploited to crash a node
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
            // TODO: blacklist this peer
            return false;
        }

        true
    }

    /// Validates a proposer block received via sync during startup
    pub async fn is_valid_proposer_block_from_sync(&mut self, block: &Block) -> bool {
        // TODO: is this from the timeslot requested? else error

        // is this a new block? else error
        let proof_id = block.proof.get_id();

        // is this a new block?
        if self.recent_proof_ids.contains(&proof_id) {
            error!("Received a block proposal via gossip for known block, ignoring");
            return false;
        }

        // TODO: make this into a self-pruning data structure
        // else add
        self.recent_proof_ids.insert(proof_id);

        // have we already received parent? else error
        if !self
            .metablocks
            .content_to_proof_map
            .contains_key(&block.content.parent_id)
        {
            error!("Received a block via sync with unknown parent");
            return false;
        }

        let parent_proof_id = self
            .metablocks
            .content_to_proof_map
            .get(&block.content.parent_id)
            .unwrap();
        let parent_metablock = self.metablocks.blocks.get(parent_proof_id).unwrap().clone();

        // ensure the parent is from an earlier timeslot
        if parent_metablock.block.proof.timeslot >= block.proof.timeslot {
            error!("Received a block via sync whose parent is in the future");
            return false;
        }

        // is the parent not too far back? (no deep forks)
        // compare parent block height to current block height of longest chain
        if parent_metablock.height + CONFIRMATION_DEPTH as u64 >= self.heads[0].block_height {
            error!("Receive a block via sync that would cause a deep fork");
            return false;
        }

        // block is valid?
        if !(self.validate_block(block).await) {
            return false;
        }

        true
    }

    /// Validates a proposer block received via gossip
    pub async fn is_valid_proposer_block_from_gossip(&mut self, block: &Block) -> bool {
        debug!(
            "Validating remote block for epoch: {} at timeslot {}",
            block.proof.epoch, block.proof.timeslot
        );

        let proof_id = block.proof.get_id();

        // is this a new block?
        if self.recent_proof_ids.contains(&proof_id) {
            warn!("Received a block proposal via gossip for known block, ignoring");
            return false;
        }

        // TODO: make this into a self-pruning data structure
        // else add
        self.recent_proof_ids.insert(proof_id);

        // If node is still syncing the ledger, cache and apply on sync
        if !self.timer_is_running {
            trace!("Caching a block received via gossip before the ledger is synced");
            self.cache_remote_block(block);
            return false;
        }

        // has the proof's timeslot arrived?
        if self.current_timeslot < block.proof.timeslot {
            if self.current_timeslot - MAX_EARLY_TIMESLOTS > block.proof.timeslot {
                // TODO: flag this peer
                debug!("Ignoring a block that is too early");
                return false;
            }

            // else cache and wait for arrival
            self.early_blocks_by_timeslot
                .entry(block.proof.timeslot)
                .and_modify(|blocks| blocks.push(block.clone()))
                .or_insert(vec![block.clone()]);

            debug!("Caching a block that is early");
            return false;
        }

        // is the timeslot recent enough?
        if block.proof.timeslot > self.current_timeslot + MAX_LATE_TIMESLOTS {
            // TODO: flag this peer
            error!("Received a late block via gossip, ignoring");
            return false;
        }

        // If we are not aware of the blocks parent, cache and apply once parent is seen
        if !self
            .metablocks
            .content_to_proof_map
            .contains_key(&block.content.parent_id)
        {
            debug!("Caching a block received via gossip with unknown parent");
            self.cache_remote_block(block);
            return false;
        }

        let parent_proof_id = self
            .metablocks
            .content_to_proof_map
            .get(&block.content.parent_id)
            .unwrap();
        let parent_metablock = self.metablocks.blocks.get(parent_proof_id).unwrap().clone();

        // ensure the parent is from an earlier timeslot
        if parent_metablock.block.proof.timeslot >= block.proof.timeslot {
            // TODO: blacklist this peer
            debug!("Ignoring a block whose parent is in the future");
            return false;
        }

        // is the parent not too far back? (no deep forks)
        // compare parent block height to current block height of longest chain
        if parent_metablock.height + CONFIRMATION_DEPTH as u64 >= self.heads[0].block_height {
            // TODO: blacklist this peer
            debug!("Ignoring a block that would cause a deep fork");
            return false;
        }

        // is the block valid?
        if !(self.validate_block(block).await) {
            return false;
        }

        true
    }

    /// Completes validation for a cached proposer block received via gossip whose parent has been staged
    pub async fn is_valid_proposer_block_from_cache(&mut self, block: &Block) -> bool {
        // is parent from earlier timeslot?
        let parent_proof_id = self
            .metablocks
            .content_to_proof_map
            .get(&block.content.parent_id)
            .unwrap();
        let parent_metablock = self.metablocks.blocks.get(parent_proof_id).unwrap().clone();

        // ensure the parent is from an earlier timeslot
        if parent_metablock.block.proof.timeslot >= block.proof.timeslot {
            // TODO: blacklist this peer
            debug!("Ignoring a block whose parent is in the future");
            return false;
        }

        // removed check for arrival time window, as this doesn't seem to apply to cached blocks

        // is the block valid?
        if !(self.validate_block(block).await) {
            return false;
        }

        true
    }

    /// Completes validation for a proposer block received via gossip that was ahead of the timeslot received in and has now arrived
    pub async fn is_valid_proposer_block_that_has_arrived(&mut self, block: &Block) -> bool {
        // block is valid
        if !(self.validate_block(block).await) {
            return false;
        }

        // If we are not aware of the blocks parent, cache and apply once parent is seen
        if !self
            .metablocks
            .content_to_proof_map
            .contains_key(&block.content.parent_id)
        {
            debug!("Caching a block received via gossip with unknown parent");
            self.cache_remote_block(block);
            return false;
        }

        let parent_proof_id = self
            .metablocks
            .content_to_proof_map
            .get(&block.content.parent_id)
            .unwrap();
        let parent_metablock = self.metablocks.blocks.get(parent_proof_id).unwrap().clone();

        // ensure the parent is from an earlier timeslot
        if parent_metablock.block.proof.timeslot >= block.proof.timeslot {
            // TODO: blacklist this peer
            debug!("Ignoring a block whose parent is in the future");
            return false;
        }

        // is the parent not too far back? (no deep forks)
        // compare parent block height to current block height of longest chain
        if parent_metablock.height + CONFIRMATION_DEPTH as u64 >= self.heads[0].block_height {
            // TODO: blacklist this peer
            debug!("Ignoring a block that would cause a deep fork");
            return false;
        }

        true
    }

    /// Prepare the block for application once it is "seen" by some other block
    pub async fn stage_block(&mut self, block: &Block) {
        // TODO: this should be hardcoded into the reference implementation
        if self.genesis_timestamp == 0 {
            self.genesis_timestamp = block.content.timestamp;
        }

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

        let proof_id = block.proof.get_id();

        // TODO: make sure the branch will not below the current confirmed block height

        // check if block received during sync is in cached gossip and remove
        if self.metablocks.contains_key(&proof_id) {
            // remove from cached gossip
            let mut is_empty = false;
            self.cached_blocks_by_parent_content_id
                .entry(block.content.parent_id)
                .and_modify(|blocks| {
                    blocks
                        .iter()
                        .position(|block| block.proof.get_id() == proof_id)
                        .map(|index| blocks.remove(index));
                    is_empty = blocks.is_empty();
                });

            if is_empty {
                self.cached_blocks_by_parent_content_id
                    .remove(&block.content.parent_id);
            }
        }

        // save block -> metablocks, blocks by timeslot
        let metablock = self.metablocks.save(pruned_block);
        self.proof_ids_by_timeslot
            .entry(block.proof.timeslot)
            .and_modify(|blocks| blocks.push(metablock.proof_id))
            .or_insert(vec![metablock.proof_id]);

        // update head of this branch
        self.update_heads(
            metablock.block.content.parent_id,
            metablock.content_id,
            metablock.height,
        );

        // confirm the k-deep parent
        let mut parent_content_id = metablock.block.content.parent_id;
        let mut confirmation_depth: usize = 0;
        loop {
            match self
                .metablocks
                .content_to_proof_map
                .get(&metablock.block.content.parent_id)
            {
                Some(parent_proof_id) => {
                    let parent_block = self
                        .metablocks
                        .blocks
                        .get(parent_proof_id)
                        .expect("Must have block if in content_to_proof_map")
                        .clone();

                    parent_content_id = parent_block.block.content.parent_id;
                    confirmation_depth += 1;
                    if confirmation_depth == CONFIRMATION_DEPTH {
                        self.confirm_block(&parent_block).await;
                        break;
                    }
                }
                None => break,
            }
        }

        // add to epoch tracker
        self.epoch_tracker
            .add_block_to_epoch(
                block.proof.epoch,
                metablock.height,
                proof_id,
                &self.blocks_on_longest_chain,
            )
            .await;
    }

    /// Stage all cached descendants for a given parent
    pub async fn stage_cached_children(&mut self, parent_id: ContentId) {
        let mut blocks = self
            .cached_blocks_by_parent_content_id
            .get(&parent_id)
            .cloned()
            .unwrap_or_default();

        while blocks.len() > 0 {
            let mut additional_blocks: Vec<Block> = Vec::new();
            for block in blocks.drain(..) {
                if self
                    .is_valid_proposer_block_from_cache(&block.clone())
                    .await
                {
                    self.stage_block(&block.clone()).await;

                    self.cached_blocks_by_parent_content_id
                        .get(&block.content.get_id())
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .for_each(|block| additional_blocks.push(block.clone()));
                }
            }

            std::mem::swap(&mut blocks, &mut additional_blocks);
        }
    }

    /// Applies the txs in a block to balances when it is k-deep
    pub async fn confirm_block(&mut self, metablock: &MetaBlock) -> bool {
        debug!(
            "Confirming block with proof_id: {}",
            hex::encode(&metablock.proof_id[0..8])
        );

        // TODO: modify to verify tx blocks and that the first tx is always a coinbase tx
        // do we have all txs referenced?
        for tx_id in metablock.block.content.tx_ids.iter() {
            if !self.txs.contains_key(tx_id) {
                error!("Cannot confirm block, includes unknown txs");
                return false;
            }
        }

        // add to longest chain
        self.blocks_on_longest_chain.insert(metablock.proof_id);

        // TODO: add block header to state buffer

        // TODO: order all tx blocks
        // apply all tx (confirm balance is still available and not already applied)
        for tx_id in metablock.block.content.tx_ids.iter() {
            match self.txs.get(tx_id).unwrap() {
                Transaction::Coinbase(tx) => {
                    // create or update account state
                    self.balances
                        .entry(tx.to_address)
                        .and_modify(|account_state| account_state.balance += BLOCK_REWARD)
                        .or_insert(AccountState {
                            nonce: 0,
                            balance: BLOCK_REWARD,
                        });

                    debug!("Applied a coinbase tx to balances");

                    // TODO: add to state, may remove from tx db here
                }
                Transaction::Credit(tx) => {
                    // TODO: apply tx to state buffer, may remove from tx db here...

                    // check if the tx has already been applied
                    if !self.tx_mempool.contains(tx_id) {
                        warn!(
                            "Transaction has already been referenced by a previous block, skipping"
                        );
                        continue;
                    }

                    // remove from mem pool
                    self.tx_mempool.remove(tx_id);

                    // ensure the tx is still valid
                    let sender_account_state = self
                        .balances
                        .get(&tx.from_address)
                        .expect("Existence of account state has already been validated");

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
                }
            }
        }

        if metablock.height > 0 {
            // get the parent_content_id of this block
            let parent_content_id = self
                .metablocks
                .content_to_proof_map
                .get(&metablock.block.content.parent_id)
                .expect("Parent will be in metablocks");

            // fetch the parent block and drain all children that are not this block
            let siblings: Vec<ProofId> = self
                .metablocks
                .blocks
                .get_mut(parent_content_id)
                .expect("Parent will be in metablocks")
                .children
                .drain_filter(|proof_id| proof_id != &metablock.proof_id)
                .collect();

            self.prune_children(siblings);
        }

        // TODO: update chain quality

        true
    }

    /// Recursively removes all siblings and their descendants when a new block is confirmed
    pub fn prune_children(&mut self, proof_ids: Vec<ProofId>) {
        for child_proof_id in proof_ids.iter() {
            // remove from metablocks
            let metablock = self
                .metablocks
                .blocks
                .remove(child_proof_id)
                .expect("Child will be in metablocks");

            // remove from content to proof map
            self.metablocks
                .content_to_proof_map
                .remove(&metablock.content_id);

            if metablock.children.is_empty() {
                // leaf node, remove the branch from heads
                self.prune_branch(metablock.content_id);
            } else {
                // repeat with this blocks children
                self.prune_children(metablock.children);
            }
        }
    }

    /// cache a block received via gossip ahead of the current epoch
    /// block will be staged once it's parent is seen
    pub fn cache_remote_block(&mut self, block: &Block) {
        self.cached_blocks_by_parent_content_id
            .entry(block.content.parent_id)
            .and_modify(|blocks| blocks.push(block.clone()))
            .or_insert(vec![block.clone()]);
    }

    /// apply the cached block to the ledger
    ///
    /// Returns last (potentially unfinished) timeslot
    // pub async fn apply_cached_blocks(&mut self, timeslot: u64) -> Result<u64, ()> {
    //     for current_timeslot in timeslot.. {
    //         if let Some(proof_ids) = self.cached_proof_ids_by_timeslot.remove(&current_timeslot) {
    //             for proof_id in proof_ids.iter() {
    //                 let cached_block = self.metablocks.blocks.get(proof_id).unwrap().block.clone();
    //                 if !self.validate_and_stage_remote_block(cached_block).await {
    //                     return Err(());
    //                 }
    //             }
    //         }
    //
    //         if self.cached_proof_ids_by_timeslot.is_empty() {
    //             return Ok(current_timeslot);
    //         }
    //
    //         if current_timeslot % TIMESLOTS_PER_EPOCH as u64 == 0 {
    //             // create the new epoch
    //             let current_epoch = self
    //                 .epoch_tracker
    //                 .advance_epoch(&self.blocks_on_longest_chain)
    //                 .await;
    //
    //             debug!(
    //                 "Closed randomness for epoch {} during apply cached blocks",
    //                 current_epoch - CHALLENGE_LOOKBACK_EPOCHS
    //             );
    //
    //             debug!("Creating a new empty epoch for epoch {}", current_epoch);
    //         }
    //     }
    //
    //     Ok(timeslot)
    // }

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
