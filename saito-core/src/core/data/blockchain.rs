use std::collections::VecDeque;
use std::io::Error;
use std::sync::Arc;

use ahash::AHashMap;
use async_recursion::async_recursion;
use rayon::prelude::*;
use tokio::sync::mpsc::Sender;
use tokio::sync::RwLock;
use tracing::{debug, error, info, trace, warn};

use crate::common::defs::{
    push_lock, Currency, SaitoHash, UtxoSet, LOCK_ORDER_MEMPOOL, LOCK_ORDER_WALLET,
};
use crate::core::data::block::{Block, BlockType};
use crate::core::data::blockring::BlockRing;
use crate::core::data::mempool::Mempool;
use crate::core::data::network::Network;
use crate::core::data::storage::Storage;
use crate::core::data::transaction::{Transaction, TransactionType};
use crate::core::data::wallet::Wallet;
use crate::core::mining_thread::MiningEvent;
use crate::{lock_for_read, lock_for_write};

// length of 1 genesis period
pub const GENESIS_PERIOD: u64 = 100_000;
// prune blocks from index after N blocks
pub const PRUNE_AFTER_BLOCKS: u64 = 6;
// max recursion when paying stakers -- number of blocks including  -- number of blocks including GTT
pub const MAX_STAKER_RECURSION: u64 = 3;
// max token supply - used in validating block #1
pub const MAX_TOKEN_SUPPLY: Currency = 10_000_000_000_000_000_000_000_000_000;
// minimum golden tickets required ( NUMBER_OF_TICKETS / number of preceding blocks )
pub const MIN_GOLDEN_TICKETS_NUMERATOR: u64 = 2;
// minimum golden tickets required ( number of tickets / NUMBER_OF_PRECEDING_BLOCKS )
pub const MIN_GOLDEN_TICKETS_DENOMINATOR: u64 = 6;

pub fn bit_pack(top: u32, bottom: u32) -> u64 {
    ((top as u64) << 32) + (bottom as u64)
}

pub fn bit_unpack(packed: u64) -> (u32, u32) {
    // Casting from a larger integer to a smaller integer (e.g. u32 -> u8) will truncate, no need to mask this
    let bottom = packed as u32;
    let top = (packed >> 32) as u32;
    (top, bottom)
}

pub enum AddBlockResult {
    BlockAdded,
    BlockAlreadyExists,
    FailedButRetry,
    FailedNotValid,
}

#[derive(Debug)]
pub struct Blockchain {
    pub utxoset: UtxoSet,
    pub blockring: BlockRing,
    pub blocks: AHashMap<SaitoHash, Block>,
    pub wallet_lock: Arc<RwLock<Wallet>>,
    pub genesis_block_id: u64,
    fork_id: SaitoHash,
}

impl Blockchain {
    #[allow(clippy::new_without_default)]
    pub fn new(wallet_lock: Arc<RwLock<Wallet>>) -> Self {
        Blockchain {
            utxoset: AHashMap::with_capacity(10_000_000),
            blockring: BlockRing::new(),
            blocks: AHashMap::new(),
            wallet_lock,
            genesis_block_id: 0,
            fork_id: [0; 32],
        }
    }
    pub fn init(&mut self) -> Result<(), Error> {
        Ok(())
    }

    pub fn set_fork_id(&mut self, fork_id: SaitoHash) {
        self.fork_id = fork_id;
    }

    pub fn get_fork_id(&self) -> &SaitoHash {
        &self.fork_id
    }

    #[tracing::instrument(level = "info", skip_all)]
    #[async_recursion]
    pub async fn add_block(
        &mut self,
        mut block: Block,
        network: &Network,
        storage: &mut Storage,
        sender_to_miner: Sender<MiningEvent>,
        mempool: &mut Mempool,
    ) -> AddBlockResult {
        // confirm hash first
        // block.generate_pre_hash();
        // block.generate_hash();
        block.generate();

        debug!(
            "add_block {:?} with id : {:?} with latest id : {:?} with tx count : {:?}",
            &hex::encode(&block.hash),
            block.id,
            self.get_latest_block_id(),
            block.transactions.len()
        );

        // start by extracting some variables that we will use
        // repeatedly in the course of adding this block to the
        // blockchain and our various indices.
        let block_hash = block.hash;
        let block_id = block.id;
        let previous_block_hash = self.blockring.get_latest_block_hash();
        // let previous_block_hash = block.previous_block_hash;

        // sanity checks
        if self.blocks.contains_key(&block_hash) {
            error!(
                "block already exists in blockchain {:?}. not adding",
                &hex::encode(&block.hash)
            );
            return AddBlockResult::BlockAlreadyExists;
        }

        //
        // TODO -- david review -- should be no need for recursive fetch
        // as each block will fetch the parent on arrival and processing
        // and we may want to tag and use the degree of distance to impose
        // penalties on routing peers.
        //
        // get missing block
        //
        if !self.blockring.is_empty() && self.get_block(&block.previous_block_hash).is_none() {
            if block.previous_block_hash == [0; 32] {
                trace!(
                    "hash is empty for parent of block : {:?}",
                    hex::encode(block.hash)
                );
            } else if block.source_connection_id.is_some() {
                let block_hash = block.previous_block_hash;
                let block_in_mempool_queue;
                {
                    block_in_mempool_queue = mempool
                        .blocks_queue
                        .par_iter()
                        .any(|b| block_hash == b.hash);
                }
                if !block_in_mempool_queue {
                    let result = network
                        .fetch_missing_block(
                            block_hash,
                            block.source_connection_id.as_ref().unwrap(),
                        )
                        .await;
                    if result.is_err() {
                        warn!(
                            "couldn't fetch block : {:?}",
                            hex::encode(block.previous_block_hash)
                        );
                        todo!()
                    }
                } else {
                    debug!(
                        "previous block : {:?} is in the mempool. not fetching",
                        hex::encode(block_hash)
                    );
                }

                debug!("adding block : {:?} back to mempool so it can be processed again after the previous block : {:?} is added",
                                    hex::encode(block.hash),
                                    hex::encode(block.previous_block_hash));
                // TODO : mempool can grow if an attacker keep sending blocks with non existing parents. need to fix. can use an expiry time perhaps?
                mempool.add_block(block);
                return AddBlockResult::FailedButRetry;
            } else {
                debug!(
                    "block : {:?} source connection id not set",
                    hex::encode(block.hash)
                );
            }
        } else {
            debug!(
                "previous block : {:?} exists in blockchain",
                hex::encode(block.previous_block_hash)
            );
        }

        //
        // pre-validation
        //
        // this would be a great place to put in a prevalidation check
        // once we are finished implementing Saito Classic. Goal would
        // be a fast form of lite-validation just to determine that it
        // is worth going through the more general effort of evaluating
        // this block for consensus.
        //

        //
        // save block to disk
        //
        // we have traditionally saved blocks to disk AFTER validating them
        // but this can slow down block propagation. So it may be sensible
        // to start a save earlier-on in the process so that we can relay
        // the block faster serving it off-disk instead of fetching it
        // repeatedly from memory. Exactly when to do this is left as an
        // optimization exercise.
        //

        //
        // insert block into hashmap and index
        //
        // the blockring is a BlockRing which lets us know which blocks (at which depth)
        // form part of the longest-chain. We also use the BlockRing to track information
        // on network congestion (how many block candidates exist at various depths and
        // in the future potentially the amount of work on each viable fork chain.
        //
        // we are going to transfer ownership of the block into the HashMap that stores
        // the block next, so we insert it into our BlockRing first as that will avoid
        // needing to borrow the value back for insertion into the BlockRing.
        //
        // TODO : check if this "if" condition can be moved to an assert
        if !self
            .blockring
            .contains_block_hash_at_block_id(block_id, block_hash)
        {
            self.blockring.add_block(&block);
        } else {
            // error!(
            //     "block : {:?} is already in blockring. therefore not adding",
            //     hex::encode(block.hash)
            // );
            // return AddBlockResult::BlockAlreadyExists;
        }
        //
        // blocks are stored in a hashmap indexed by the block_hash. we expect all
        // all block_hashes to be unique, so simply insert blocks one-by-one on
        // arrival if they do not exist.

        if !self.blocks.contains_key(&block_hash) {
            self.blocks.insert(block_hash, block);
        } else {
            error!(
                "BLOCK IS ALREADY IN THE BLOCKCHAIN, WHY ARE WE ADDING IT????? {:?}",
                block.hash
            );
            return AddBlockResult::BlockAlreadyExists;
        }

        //
        // find shared ancestor of new_block with old_chain
        //
        let mut new_chain: Vec<[u8; 32]> = Vec::new();
        let mut old_chain: Vec<[u8; 32]> = Vec::new();
        let mut shared_ancestor_found = false;
        let mut new_chain_hash = block_hash;
        let mut old_chain_hash = previous_block_hash;
        let mut am_i_the_longest_chain = false;

        while !shared_ancestor_found {
            trace!(
                "checking new chain hash : {:?}",
                hex::encode(new_chain_hash)
            );
            // TODO : following 2 lines can be optimized for a single search
            if self.blocks.contains_key(&new_chain_hash) {
                if self.blocks.get(&new_chain_hash).unwrap().in_longest_chain {
                    shared_ancestor_found = true;
                    trace!("shared ancestor found : {:?}", hex::encode(new_chain_hash));
                    break;
                } else if new_chain_hash == [0; 32] {
                    break;
                }
                new_chain.push(new_chain_hash);
                new_chain_hash = self
                    .blocks
                    .get(&new_chain_hash)
                    .unwrap()
                    .previous_block_hash;
            } else {
                break;
            }
        }

        // and get existing current chain for comparison
        if shared_ancestor_found {
            debug!("shared ancestor found");

            while new_chain_hash != old_chain_hash {
                if self.blocks.contains_key(&old_chain_hash) {
                    old_chain.push(old_chain_hash);
                    old_chain_hash = self
                        .blocks
                        .get(&old_chain_hash)
                        .unwrap()
                        .previous_block_hash;
                    if old_chain_hash == [0; 32] {
                        break;
                    }
                } else {
                    break;
                }
            }
        } else {
            debug!(
                "block without parent. block : {:?}, latest : {:?}",
                hex::encode(block_hash),
                hex::encode(previous_block_hash)
            );

            //
            // we have a block without a parent.
            //
            // if this is our first block, the blockring will have no entry yet
            // and block_ring_lc_pos (longest_chain_position) will be pointing
            // at None. We use this to determine if we are a new chain instead
            // of creating a separate variable to manually track entries.
            //
            if self.blockring.is_empty() {

                //
                // no need for action as fall-through will result in proper default
                // behavior. we have the comparison here to separate expected from
                // unexpected / edge-case issues around block receipt.
                //
            } else {
                //
                // TODO - implement logic to handle once nodes can connect
                //
                // if this not our first block, handle edge-case around receiving
                // block 503 before block 453 when block 453 is our expected proper
                // next block and we are getting blocks out-of-order because of
                // connection or network issues.

                if previous_block_hash != [0; 32]
                    && previous_block_hash == self.get_latest_block_hash()
                {
                    info!("blocks received out-of-order issue. handling edge case...");

                    let disconnected_block_id = self.get_latest_block_id();
                    for i in block_id + 1..disconnected_block_id {
                        let disconnected_block_hash =
                            self.blockring.get_longest_chain_block_hash_by_block_id(i);
                        if disconnected_block_hash != [0; 32] {
                            self.blockring.on_chain_reorganization(
                                i,
                                disconnected_block_hash,
                                false,
                            );
                            let disconnected_block = self.get_mut_block(&disconnected_block_hash);
                            if let Some(disconnected_block) = disconnected_block {
                                disconnected_block.in_longest_chain = false;
                            }
                        }
                    }

                    new_chain.clear();
                    new_chain.push(block_hash);
                    am_i_the_longest_chain = true;
                }
            }
        }

        // at this point we should have a shared ancestor or not
        // find out whether this new block is claiming to require chain-validation
        if !am_i_the_longest_chain && self.is_new_chain_the_longest_chain(&new_chain, &old_chain) {
            am_i_the_longest_chain = true;
        }

        //
        // now update blockring so it is not empty
        //
        // we do this down here instead of automatically on
        // adding a block, as we want to have the above check
        // for handling the edge-case of blocks received in the
        // wrong order. the longest_chain check also requires a
        // first-block-received check that is conducted against
        // the blockring.
        //
        self.blockring.empty = false;

        //
        // validate
        //
        // blockchain validate "validates" the new_chain by unwinding the old
        // and winding the new, which calling validate on any new previously-
        // unvalidated blocks. When the longest-chain status of blocks changes
        // the function on_chain_reorganization is triggered in blocks and
        // with the BlockRing. We fail if the newly-preferred chain is not
        // viable.
        //
        return if am_i_the_longest_chain {
            debug!("this is the longest chain");
            self.blocks.get_mut(&block_hash).unwrap().in_longest_chain = true;

            let does_new_chain_validate = self
                .validate(new_chain.as_slice(), old_chain.as_slice(), storage)
                .await;

            if does_new_chain_validate {
                self.add_block_success(block_hash, network, storage, mempool)
                    .await;

                let difficulty = self.blocks.get(&block_hash).unwrap().difficulty;

                info!("sending longest chain block added event to miner : hash : {:?} difficulty : {:?}", hex::encode(block_hash), difficulty);
                // TODO : remove the sender by using a return value.
                sender_to_miner
                    .send(MiningEvent::LongestChainBlockAdded {
                        hash: block_hash,
                        difficulty,
                    })
                    .await
                    .unwrap();
                AddBlockResult::BlockAdded
            } else {
                warn!(
                    "new chain doesn't validate with hash : {:?}",
                    hex::encode(block_hash)
                );
                self.blocks.get_mut(&block_hash).unwrap().in_longest_chain = false;
                self.add_block_failure(&block_hash, mempool).await;
                AddBlockResult::FailedButRetry
            }
        } else {
            debug!("this is not the longest chain");
            self.add_block_success(block_hash, network, storage, mempool)
                .await;
            AddBlockResult::BlockAdded
        };
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub async fn add_block_success(
        &mut self,
        block_hash: SaitoHash,
        network: &Network,
        storage: &mut Storage,
        mempool: &mut Mempool,
    ) {
        debug!("add_block_success : {:?}", hex::encode(block_hash));
        // trace!(
        //     " ... blockchain.add_block_success: {:?}",
        //     create_timestamp()
        // );
        // print blockring longest_chain_block_hash infor
        self.print(10);

        //
        // save to disk
        //
        {
            let block = self.get_mut_block(&block_hash).unwrap();
            if block.block_type != BlockType::Header {
                // TODO : this will have an impact when the block sizes are getting large or there are many forks. need to handle this
                storage.write_block_to_disk(block).await;
            } else {
                debug!(
                    "block : {:?} not written to disk as type : {:?}",
                    hex::encode(block.hash),
                    block.block_type
                );
            }
            network.propagate_block(block).await;
        }

        //
        // TODO: clean up mempool - I think we shouldn't cleanup mempool here.
        //  because that's already happening in send_blocks_to_blockchain
        //  So who is in charge here?
        //  is send_blocks_to_blockchain calling add_block or
        //  is blockchain calling mempool.on_chain_reorganization?
        //
        //
        {
            mempool
                .transactions
                .retain(|_, tx| tx.validate_against_utxoset(&self.utxoset));
            let block = self.get_mut_block(&block_hash).unwrap();
            // we calling delete_tx after removing invalidated txs, to make sure routing work is calculated after removing all the txs
            mempool.delete_transactions(&block.transactions);
        }

        //
        // propagate block to network
        //
        // TODO : notify other threads and propagate to other peers

        // {
        //     // TODO : no need to access block multiple times. combine with previous call in block save call
        //     let block = self.get_mut_block(&block_hash).await;
        // }

        // global_sender
        //     .send(GlobalEvent::BlockchainSavedBlock { hash: block_hash })
        //     .expect("error: BlockchainSavedBlock message failed to send");
        // trace!(" ... block save done:            {:?}", create_timestamp());

        //
        // update_genesis_period and prune old data - MOVED to on_chain_reorganization()
        //
        // self.update_genesis_period().await;

        //
        // fork id  - MOVED to on_chain_reorganization()
        //
        // let fork_id = self.generate_fork_id(block_id);
        // self.set_fork_id(fork_id);

        //
        // ensure pruning of next block OK will have the right CVs
        //
        if self.get_latest_block_id() > GENESIS_PERIOD {
            let pruned_block_hash = self.blockring.get_longest_chain_block_hash_by_block_id(
                self.get_latest_block_id() - GENESIS_PERIOD,
            );

            assert_ne!(pruned_block_hash, [0; 32]);

            //
            // TODO
            //
            // handle this more efficiently - we should be able to prepare the block
            // in advance so that this doesn't take up time in block production. we
            // need to generate_metadata_hashes so that the slips know the utxo_key
            // to use to check the utxoset.
            //
            {
                let pblock = self.get_mut_block(&pruned_block_hash).unwrap();
                pblock
                    .upgrade_block_to_block_type(BlockType::Full, storage)
                    .await;
            }
        }
        info!("block {:?} added successfully", hex::encode(block_hash));
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub async fn add_block_failure(&mut self, block_hash: &SaitoHash, mempool: &mut Mempool) {
        info!("add block failed : {:?}", hex::encode(block_hash));

        mempool.delete_block(block_hash);
        let mut block = self.blocks.remove(block_hash).unwrap();

        if block.creator == mempool.public_key {
            let transactions = &mut block.transactions;
            let prev_count = transactions.len();
            let transactions: Vec<Transaction> = transactions
                .par_drain(..)
                .with_min_len(10)
                .filter(|tx| {
                    // TODO : what other types should be added back to the mempool
                    if tx.transaction_type == TransactionType::Normal {
                        // TODO : is there a way to not validate these again ?
                        return tx.validate(&self.utxoset);
                    }
                    return false;
                })
                .collect();
            // transactions.retain(|tx| tx.validate(&self.utxoset));
            info!(
                "adding {:?} transactions back to mempool. dropped {:?} invalid transactions",
                transactions.len(),
                (prev_count - transactions.len())
            );
            for tx in transactions {
                mempool.transactions.insert(tx.signature, tx);
            }
            mempool.new_tx_added = true;
        }
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub fn generate_fork_id(&self, block_id: u64) -> SaitoHash {
        let mut fork_id = [0; 32];
        let mut current_block_id = block_id;

        //
        // roll back to last even 10 blocks
        // TODO : don't need the for loop just get the last 10's multiple [current_block_id = current_block_id - (current_block_id % 10)]
        for i in 0..10 {
            if (current_block_id - i) % 10 == 0 {
                current_block_id -= i;
                break;
            }
        }

        //
        // loop backwards through blockchain
        //
        for i in 0..16 {
            if i == 0 {
                current_block_id -= 0;
            }
            if i == 1 {
                current_block_id -= 10;
            }
            if i == 2 {
                current_block_id -= 10;
            }
            if i == 3 {
                current_block_id -= 10;
            }
            if i == 4 {
                current_block_id -= 10;
            }
            if i == 5 {
                current_block_id -= 10;
            }
            if i == 6 {
                current_block_id -= 25;
            }
            if i == 7 {
                current_block_id -= 25;
            }
            if i == 8 {
                current_block_id -= 100;
            }
            if i == 9 {
                current_block_id -= 300;
            }
            if i == 10 {
                current_block_id -= 500;
            }
            if i == 11 {
                current_block_id -= 4000;
            }
            if i == 12 {
                current_block_id -= 10000;
            }
            if i == 13 {
                current_block_id -= 20000;
            }
            if i == 14 {
                current_block_id -= 50000;
            }
            if i == 15 {
                current_block_id -= 100000;
            }

            //
            // do not loop around if block id < 0
            //
            if current_block_id > block_id || current_block_id == 0 {
                break;
            }

            //
            // index to update
            //
            let index = 2 * i;

            //
            //
            //
            let block_hash = self
                .blockring
                .get_longest_chain_block_hash_by_block_id(current_block_id);
            fork_id[index] = block_hash[index];
            fork_id[index + 1] = block_hash[index + 1];
        }

        fork_id
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub fn generate_last_shared_ancestor(
        &self,
        peer_latest_block_id: u64,
        fork_id: SaitoHash,
    ) -> u64 {
        let my_latest_block_id = self.get_latest_block_id();

        let mut peer_block_id = peer_latest_block_id;
        let mut my_block_id = my_latest_block_id;

        let weights = vec![
            0, 10, 10, 10, 10, 10, 25, 25, 100, 300, 500, 4000, 10000, 20000, 50000, 100000,
        ];
        if peer_latest_block_id >= my_latest_block_id {
            // roll back to last even 10 blocks
            peer_block_id = peer_block_id - (peer_block_id % 10);

            // their fork id
            for (index, weight) in weights.iter().enumerate() {
                if peer_block_id <= *weight {
                    return 0;
                }
                peer_block_id -= weight;

                // do not loop around if block id < 0
                if peer_block_id > peer_latest_block_id {
                    return 0;
                }

                // index in fork_id hash
                let index = 2 * index;

                // compare input hash to my hash
                if peer_block_id <= my_block_id {
                    let block_hash = self
                        .blockring
                        .get_longest_chain_block_hash_by_block_id(peer_block_id);
                    if fork_id[index] == block_hash[index]
                        && fork_id[index + 1] == block_hash[index + 1]
                    {
                        return peer_block_id;
                    }
                }
            }
        } else {
            for (index, weight) in weights.iter().enumerate() {
                if my_block_id <= *weight {
                    return 0;
                }
                my_block_id -= weight;

                // do not loop around if block id < 0
                if my_block_id > my_latest_block_id {
                    return 0;
                }

                // index in fork_id hash
                let index = 2 * index;

                // compare input hash to my hash
                if peer_block_id <= my_block_id {
                    let block_hash = self
                        .blockring
                        .get_longest_chain_block_hash_by_block_id(peer_block_id);
                    if fork_id[index] == block_hash[index]
                        && fork_id[index + 1] == block_hash[index + 1]
                    {
                        return peer_block_id;
                    }
                }
            }
        }

        // no match? return 0 -- no shared ancestor
        0
    }
    pub fn print(&self, count: u64) {
        let latest_block_id = self.get_latest_block_id();
        let mut current_id = latest_block_id;

        let mut min_id = 0;
        if latest_block_id > count {
            min_id = latest_block_id - count;
        }
        info!("------------------------------------------------------");
        while current_id > 0 && current_id >= min_id {
            let hash = self
                .blockring
                .get_longest_chain_block_hash_by_block_id(current_id);
            if hash == [0; 32] {
                break;
            }
            info!(
                "{} - {:?}",
                current_id,
                hex::encode(
                    self.blockring
                        .get_longest_chain_block_hash_by_block_id(current_id)
                )
            );
            current_id -= 1;
        }
        info!("------------------------------------------------------");
    }

    pub fn get_latest_block(&self) -> Option<&Block> {
        let block_hash = self.blockring.get_latest_block_hash();
        self.blocks.get(&block_hash)
    }

    pub fn get_latest_block_hash(&self) -> SaitoHash {
        self.blockring.get_latest_block_hash()
    }

    pub fn get_latest_block_id(&self) -> u64 {
        self.blockring.get_latest_block_id()
    }

    pub fn get_block_sync(&self, block_hash: &SaitoHash) -> Option<&Block> {
        self.blocks.get(block_hash)
    }

    // #[tracing::instrument(level = "info", skip_all)]
    pub fn get_block(&self, block_hash: &SaitoHash) -> Option<&Block> {
        //

        self.blocks.get(block_hash)
    }

    pub fn get_mut_block(&mut self, block_hash: &SaitoHash) -> Option<&mut Block> {
        //
        self.blocks.get_mut(block_hash)
    }

    pub fn is_block_indexed(&self, block_hash: SaitoHash) -> bool {
        if self.blocks.contains_key(&block_hash) {
            return true;
        }
        false
    }

    pub fn contains_block_hash_at_block_id(&self, block_id: u64, block_hash: SaitoHash) -> bool {
        self.blockring
            .contains_block_hash_at_block_id(block_id, block_hash)
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub fn is_new_chain_the_longest_chain(
        &self,
        new_chain: &[SaitoHash],
        old_chain: &[SaitoHash],
    ) -> bool {
        debug!("checking for longest chain");
        if self.blockring.is_empty() {
            return true;
        }
        if old_chain.len() > new_chain.len() {
            warn!(
                "WARN: old chain length : {:?} is greater than new chain length : {:?}",
                old_chain.len(),
                new_chain.len()
            );
            return false;
        }

        if self.blockring.get_latest_block_id() >= self.blocks.get(&new_chain[0]).unwrap().id {
            return false;
        }

        let mut old_bf: Currency = 0;
        let mut new_bf: Currency = 0;

        for hash in old_chain.iter() {
            old_bf += self.blocks.get(hash).unwrap().burnfee;
        }
        for hash in new_chain.iter() {
            if let Some(x) = self.blocks.get(hash) {
                new_bf += x.burnfee;
            } else {
                return false;
            }
            //new_bf += self.blocks.get(hash).unwrap().get_burnfee();
        }
        //
        // new chain must have more accumulated work AND be longer
        //
        old_chain.len() < new_chain.len() && old_bf <= new_bf
    }

    //
    // when new_chain and old_chain are generated the block_hashes are added
    // to their vectors from tip-to-shared-ancestors. if the shared ancestors
    // is at position [0] in our blockchain for instance, we may receive:
    //
    // new_chain --> adds the hashes in this order
    //   [5] [4] [3] [2] [1]
    //
    // old_chain --> adds the hashes in this order
    //   [4] [3] [2] [1]
    //
    // unwinding requires starting from the BEGINNING of the vector, while
    // winding requires starting from th END of the vector. the loops move
    // in opposite directions.
    //
    #[tracing::instrument(level = "info", skip_all)]
    pub async fn validate(
        &mut self,
        new_chain: &[SaitoHash],
        old_chain: &[SaitoHash],
        storage: &Storage,
    ) -> bool {
        debug!("validating chains");

        let previous_block_hash;
        let has_gt;
        {
            let block = self.blocks.get(new_chain[0].as_ref()).unwrap();
            previous_block_hash = block.previous_block_hash;
            has_gt = block.has_golden_ticket;
        }
        //
        // ensure new chain has adequate mining support to be considered as
        // a viable chain. we handle this check here as opposed to handling
        // it in wind_chain as we only need to check once for the entire chain
        //
        if !self.is_golden_ticket_count_valid(previous_block_hash, has_gt) {
            return false;
        }

        if old_chain.is_empty() {
            self.wind_chain(new_chain, old_chain, new_chain.len() - 1, false, storage)
                .await
        } else if !new_chain.is_empty() {
            self.unwind_chain(new_chain, old_chain, 0, true, storage)
                .await
        } else {
            warn!("lengths are inappropriate");
            false
        }
    }

    pub fn is_golden_ticket_count_valid(
        &self,
        previous_block_hash: SaitoHash,
        current_block_has_golden_ticket: bool,
    ) -> bool {
        let mut golden_tickets_found = 0;
        let mut search_depth_index = 0;
        let mut latest_block_hash = previous_block_hash;

        for i in 0..MIN_GOLDEN_TICKETS_DENOMINATOR {
            search_depth_index += 1;

            if let Some(block) = self.get_block_sync(&latest_block_hash) {
                if i == 0 && block.id < MIN_GOLDEN_TICKETS_DENOMINATOR {
                    golden_tickets_found = MIN_GOLDEN_TICKETS_DENOMINATOR;
                    break;
                }

                // the latest block will not have has_golden_ticket set yet
                // so it is possible we undercount the latest block. this
                // is dealt with by manually checking for the existence of
                // a golden ticket if we only have 1 golden ticket below.
                if block.has_golden_ticket {
                    golden_tickets_found += 1;
                }
                latest_block_hash = block.previous_block_hash;
            } else {
                break;
            }
        }

        if golden_tickets_found < MIN_GOLDEN_TICKETS_NUMERATOR
            && search_depth_index >= MIN_GOLDEN_TICKETS_DENOMINATOR
            && current_block_has_golden_ticket
        {
            golden_tickets_found += 1;
        }

        if golden_tickets_found < MIN_GOLDEN_TICKETS_NUMERATOR
            && search_depth_index >= MIN_GOLDEN_TICKETS_DENOMINATOR
        {
            warn!(
                "not enough golden tickets : found = {:?} depth = {:?}",
                golden_tickets_found, search_depth_index
            );
            return false;
        }
        true
    }

    //
    // when new_chain and old_chain are generated the block_hashes are added
    // to their vectors from tip-to-shared-ancestors. if the shared ancestors
    // is at position [0] for instance, we may receive:
    //
    // new_chain --> adds the hashes in this order
    //   [5] [4] [3] [2] [1]
    //
    // old_chain --> adds the hashes in this order
    //   [4] [3] [2] [1]
    //
    // unwinding requires starting from the BEGINNING of the vector, while
    // winding requires starting from the END of the vector. the loops move
    // in opposite directions. the argument current_wind_index is the
    // position in the vector NOT the ordinal number of the block_hash
    // being processed. we start winding with current_wind_index 4 not 0.
    //
    #[async_recursion]
    #[tracing::instrument(level = "info", skip_all)]
    pub async fn wind_chain(
        &mut self,
        new_chain: &[SaitoHash],
        old_chain: &[SaitoHash],
        current_wind_index: usize,
        wind_failure: bool,
        storage: &Storage,
    ) -> bool {
        // trace!(" ... blockchain.wind_chain strt: {:?}", create_timestamp());

        //
        // if we are winding a non-existent chain with a wind_failure it
        // means our wind attempt failed and we should move directly into
        // add_block_failure() by returning false.
        //
        if wind_failure && new_chain.is_empty() {
            return false;
        }

        //
        // winding the chain requires us to have certain data associated
        // with the block and the transactions, particularly the tx hashes
        // that we need to generate the slip UUIDs and create the tx sigs.
        //
        // we fetch the block mutably first in order to update these vars.
        // we cannot just send the block mutably into our regular validate()
        // function because of limitatins imposed by Rust on mutable data
        // structures. So validation is "read-only" and our "write" actions
        // happen first.
        //
        let block_hash = new_chain.get(current_wind_index).unwrap();

        {
            let block = self.get_mut_block(block_hash).unwrap();

            block
                .upgrade_block_to_block_type(BlockType::Full, storage)
                .await;

            let latest_block_id = block.id;

            //
            // ensure previous blocks that may be needed to calculate the staking
            // tables or the nolan that are potentially falling off the chain have
            // full access to their transaction data.
            //
            for i in 1..MAX_STAKER_RECURSION {
                if i >= latest_block_id {
                    break;
                }
                let bid = latest_block_id - i;
                let previous_block_hash =
                    self.blockring.get_longest_chain_block_hash_by_block_id(bid);
                if self.is_block_indexed(previous_block_hash) {
                    let block = self.get_mut_block(&previous_block_hash).unwrap();
                    block
                        .upgrade_block_to_block_type(BlockType::Full, storage)
                        .await;
                }
            }
        }

        let block = self.blocks.get(block_hash).unwrap();
        assert_eq!(block.block_type, BlockType::Full);

        let does_block_validate = block.validate(self, &self.utxoset).await;

        if does_block_validate {
            // blockring update
            self.blockring
                .on_chain_reorganization(block.id, block.hash, true);

            //
            // TODO - wallet update should be optional, as core routing nodes
            // will not want to do the work of scrolling through the block and
            // updating their wallets by default. wallet processing can be
            // more efficiently handled by lite-nodes.
            //
            {
                // trace!(" ... wallet processing start:    {}", create_timestamp());
                let (mut wallet, _wallet_) = lock_for_write!(self.wallet_lock, LOCK_ORDER_WALLET);

                wallet.on_chain_reorganization(block, true);

                // trace!(" ... wallet processing stop:     {}", create_timestamp());
            }
            let block_id = block.id;
            drop(block);
            // utxoset update
            {
                let block = self.blocks.get_mut(block_hash).unwrap();
                block.on_chain_reorganization(&mut self.utxoset, true);
            }

            self.on_chain_reorganization(block_id, true, storage).await;

            //
            // we have received the first entry in new_blocks() which means we
            // have added the latest tip. if the variable wind_failure is set
            // that indicates that we ran into an issue when winding the new_chain
            // and what we have just processed is the old_chain (being rewound)
            // so we should exit with failure.
            //
            // otherwise we have successfully wound the new chain, and exit with
            // success.
            //
            if current_wind_index == 0 {
                if wind_failure {
                    return false;
                }
                return true;
            }

            let res = self
                .wind_chain(new_chain, old_chain, current_wind_index - 1, false, storage)
                .await;
            res
        } else {
            //
            // we have had an error while winding the chain. this requires us to
            // unwind any blocks we have already wound, and rewind any blocks we
            // have unwound.
            //
            // we set wind_failure to "true" so that when we reach the end of
            // the process of rewinding the old-chain, our wind_chain function
            // will know it has rewound the old chain successfully instead of
            // successfully added the new chain.
            //
            error!(
                "ERROR: this block : {:?} does not validate!",
                hex::encode(block.hash)
            );
            if current_wind_index == new_chain.len() - 1 {
                //
                // this is the first block we have tried to add
                // and so we can just roll out the older chain
                // again as it is known good.
                //
                // note that old and new hashes are swapped
                // and the old chain is set as null because
                // we won't move back to it. we also set the
                // resetting_flag to 1 so we know to fork
                // into addBlockToBlockchainFailure
                //
                // true -> force -> we had issues, is failure
                //
                // new_chain --> hashes are still in this order
                //   [5] [4] [3] [2] [1]
                //
                // we are at the beginning of our own vector so we have nothing
                // to unwind. Because of this, we start WINDING the old chain back
                // which requires us to start at the END of the new chain vector.
                //
                if !old_chain.is_empty() {
                    info!("old chain len: {}", old_chain.len());
                    let res = self
                        .wind_chain(old_chain, new_chain, old_chain.len() - 1, true, storage)
                        .await;
                    res
                } else {
                    false
                }
            } else {
                let mut chain_to_unwind: Vec<[u8; 32]> = vec![];

                //
                // if we run into a problem winding our chain after we have
                // wound any blocks, we take the subset of the blocks we have
                // already pushed through on_chain_reorganization (i.e. not
                // including this block!) and put them onto a new vector we
                // will unwind in turn.
                //
                for i in current_wind_index + 1..new_chain.len() {
                    chain_to_unwind.push(new_chain[i]);
                }

                //
                // chain to unwind is now something like this...
                //
                //  [3] [2] [1]
                //
                // unwinding starts from the BEGINNING of the vector
                //
                let res = self
                    .unwind_chain(old_chain, &chain_to_unwind, 0, true, storage)
                    .await;
                res
            }
        }
    }

    //
    // when new_chain and old_chain are generated the block_hashes are pushed
    // to their vectors from tip-to-shared-ancestors. if the shared ancestors
    // is at position [0] for instance, we may receive:
    //
    // new_chain --> adds the hashes in this order
    //   [5] [4] [3] [2] [1]
    //
    // old_chain --> adds the hashes in this order
    //   [4] [3] [2] [1]
    //
    // unwinding requires starting from the BEGINNING of the vector, while
    // winding requires starting from the END of the vector. the first
    // block we have to remove in the old_chain is thus at position 0, and
    // walking up the vector from there until we reach the end.
    //
    #[async_recursion]
    #[tracing::instrument(level = "info", skip_all)]
    pub async fn unwind_chain(
        &mut self,
        new_chain: &[SaitoHash],
        old_chain: &[SaitoHash],
        current_unwind_index: usize,
        wind_failure: bool,
        storage: &Storage,
    ) -> bool {
        let block_id;
        {
            let block = self
                .blocks
                .get_mut(&old_chain[current_unwind_index])
                .unwrap();
            block
                .upgrade_block_to_block_type(BlockType::Full, storage)
                .await;
            block_id = block.id;

            // utxoset update
            block.on_chain_reorganization(&mut self.utxoset, false);

            // blockring update
            self.blockring
                .on_chain_reorganization(block.id, block.hash, false);

            // wallet update
            {
                let (mut wallet, _wallet_) = lock_for_write!(self.wallet_lock, LOCK_ORDER_WALLET);

                wallet.on_chain_reorganization(&block, false);
            }
        }
        self.on_chain_reorganization(block_id, false, storage).await;
        if current_unwind_index == old_chain.len() - 1 {
            //
            // start winding new chain
            //
            // new_chain --> adds the hashes in this order
            //   [5] [4] [3] [2] [1]
            //
            // old_chain --> adds the hashes in this order
            //   [4] [3] [2] [1]
            //
            // winding requires starting at the END of the vector and rolling
            // backwards until we have added block #5, etc.
            //
            let res = self
                .wind_chain(
                    new_chain,
                    old_chain,
                    new_chain.len() - 1,
                    wind_failure,
                    storage,
                )
                .await;
            res
        } else {
            //
            // continue unwinding,, which means
            //
            // unwinding requires moving FORWARD in our vector (and backwards in
            // the blockchain). So we increment our unwind index.
            //
            let res = self
                .unwind_chain(
                    new_chain,
                    old_chain,
                    current_unwind_index + 1,
                    wind_failure,
                    storage,
                )
                .await;
            res
        }
    }

    /// keeps any blockchain variables like fork_id or genesis_period
    /// tracking variables updated as the chain gets new blocks. also
    /// pre-loads any blocks needed to improve performance.
    #[tracing::instrument(level = "info", skip_all)]
    async fn on_chain_reorganization(
        &mut self,
        block_id: u64,
        longest_chain: bool,
        storage: &Storage,
    ) {
        //
        // skip out if earlier than we need to be vis-a-vis last_block_id
        //
        if self.get_latest_block_id() >= block_id {
            return;
        }

        if longest_chain {
            //
            // update genesis period, purge old data
            //
            self.update_genesis_period(storage).await;

            //
            // generate fork_id
            //
            let fork_id = self.generate_fork_id(block_id);
            self.set_fork_id(fork_id);
        }

        self.downgrade_blockchain_data().await;
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub async fn update_genesis_period(&mut self, storage: &Storage) {
        //
        // we need to make sure this is not a random block that is disconnected
        // from our previous genesis_id. If there is no connection between it
        // and us, then we cannot delete anything as otherwise the provision of
        // the block may be an attack on us intended to force us to discard
        // actually useful data.
        //
        // so we check that our block is the head of the longest-chain and only
        // update the genesis period when that is the case.
        //
        let latest_block_id = self.get_latest_block_id();
        if latest_block_id >= ((GENESIS_PERIOD * 2) + 1) {
            //
            // prune blocks
            //
            let purge_bid = latest_block_id - (GENESIS_PERIOD * 2);
            self.genesis_block_id = latest_block_id - GENESIS_PERIOD;

            //
            // in either case, we are OK to throw out everything below the
            // lowest_block_id that we have found. we use the purge_id to
            // handle purges.
            if purge_bid > 0 {
                self.delete_blocks(purge_bid, storage).await;
            }
        }

        //TODO: we already had in update_genesis_period() in self method - maybe no need to call here?
        // self.downgrade_blockchain_data().await;
    }

    //
    // deletes all blocks at a single block_id
    //
    #[tracing::instrument(level = "info", skip_all)]
    pub async fn delete_blocks(&mut self, delete_block_id: u64, storage: &Storage) {
        trace!(
            "removing data including from disk at id {}",
            delete_block_id
        );

        let mut block_hashes_copy: Vec<SaitoHash> = vec![];

        {
            let block_hashes = self.blockring.get_block_hashes_at_block_id(delete_block_id);
            for hash in block_hashes {
                block_hashes_copy.push(hash);
            }
        }

        trace!("number of hashes to remove {}", block_hashes_copy.len());

        for hash in block_hashes_copy {
            self.delete_block(delete_block_id, hash, storage).await;
        }
    }

    //
    // deletes a single block
    //
    #[tracing::instrument(level = "info", skip_all)]
    pub async fn delete_block(
        &mut self,
        delete_block_id: u64,
        delete_block_hash: SaitoHash,
        storage: &Storage,
    ) {
        //
        // ask block to delete itself / utxo-wise
        //
        {
            let pblock = self.blocks.get(&delete_block_hash).unwrap();
            let pblock_filename = storage.generate_block_filename(pblock);

            //
            // remove slips from wallet
            //
            {
                let (mut wallet, _wallet_) = lock_for_write!(self.wallet_lock, LOCK_ORDER_WALLET);

                wallet.delete_block(pblock);
            }
            //
            // removes utxoset data
            //
            pblock.delete(&mut self.utxoset).await;

            //
            // deletes block from disk
            //
            storage.delete_block_from_disk(pblock_filename).await;
        }

        //
        // ask blockring to remove
        //
        self.blockring
            .delete_block(delete_block_id, delete_block_hash);

        //
        // remove from block index
        //
        if self.blocks.contains_key(&delete_block_hash) {
            self.blocks.remove_entry(&delete_block_hash);
        }
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub async fn downgrade_blockchain_data(&mut self) {
        trace!("downgrading blockchain data");
        //
        // downgrade blocks still on the chain
        //
        if PRUNE_AFTER_BLOCKS > self.get_latest_block_id() {
            return;
        }
        let prune_blocks_at_block_id = self.get_latest_block_id() - PRUNE_AFTER_BLOCKS;

        let mut block_hashes_copy: Vec<SaitoHash> = vec![];

        {
            let block_hashes = self
                .blockring
                .get_block_hashes_at_block_id(prune_blocks_at_block_id);
            for hash in block_hashes {
                block_hashes_copy.push(hash);
            }
        }

        for hash in block_hashes_copy {
            //
            // ask the block to remove its transactions
            //
            {
                let block = self.get_mut_block(&hash);
                if let Some(block) = block {
                    block.downgrade_block_to_block_type(BlockType::Pruned).await;
                } else {
                    warn!("block : {:?} not found to downgrade", hex::encode(hash));
                }
            }
        }
    }
    pub async fn add_blocks_from_mempool(
        &mut self,
        mempool: Arc<RwLock<Mempool>>,
        network: &Network,
        storage: &mut Storage,
        sender_to_miner: Sender<MiningEvent>,
    ) -> bool {
        debug!("adding blocks from mempool to blockchain");
        let mut blocks: VecDeque<Block>;
        let (mut mempool, _mempool_) = lock_for_write!(mempool, LOCK_ORDER_MEMPOOL);

        blocks = mempool.blocks_queue.drain(..).collect();
        blocks.make_contiguous().sort_by(|a, b| a.id.cmp(&b.id));

        debug!("blocks to add : {:?}", blocks.len());
        let mut blockchain_updated = false;
        while let Some(block) = blocks.pop_front() {
            let result = self
                .add_block(
                    block,
                    network,
                    storage,
                    sender_to_miner.clone(),
                    &mut mempool,
                )
                .await;
            if !blockchain_updated {
                if let AddBlockResult::BlockAdded = result {
                    blockchain_updated = true;
                }
            }
        }

        debug!(
            "added blocks to blockchain. added back : {:?}",
            mempool.blocks_queue.len()
        );
        blockchain_updated
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::RwLock;

    use crate::common::defs::{push_lock, LOCK_ORDER_BLOCKCHAIN, LOCK_ORDER_WALLET};
    use crate::common::test_manager::test;
    use crate::common::test_manager::test::TestManager;
    use crate::core::data::blockchain::{bit_pack, bit_unpack, Blockchain};
    use crate::core::data::wallet::Wallet;
    use crate::{lock_for_read, lock_for_write};

    #[tokio::test]
    async fn test_blockchain_init() {
        let wallet = Arc::new(RwLock::new(Wallet::new()));
        let blockchain = Blockchain::new(wallet);

        assert_eq!(blockchain.fork_id, [0; 32]);
        assert_eq!(blockchain.genesis_block_id, 0);
    }

    #[tokio::test]
    async fn test_add_block() {
        let wallet = Arc::new(RwLock::new(Wallet::new()));
        let blockchain = Blockchain::new(wallet);

        assert_eq!(blockchain.fork_id, [0; 32]);
        assert_eq!(blockchain.genesis_block_id, 0);
    }

    #[test]
    //
    // code that packs/unpacks two 32-bit values into one 64-bit variable
    //
    fn bit_pack_test() {
        let top = 157171715;
        let bottom = 11661612;
        let packed = bit_pack(top, bottom);
        assert_eq!(packed, 157171715 * (u64::pow(2, 32)) + 11661612);
        let (new_top, new_bottom) = bit_unpack(packed);
        assert_eq!(top, new_top);
        assert_eq!(bottom, new_bottom);

        let top = u32::MAX;
        let bottom = u32::MAX;
        let packed = bit_pack(top, bottom);
        let (new_top, new_bottom) = bit_unpack(packed);
        assert_eq!(top, new_top);
        assert_eq!(bottom, new_bottom);

        let top = 0;
        let bottom = 1;
        let packed = bit_pack(top, bottom);
        let (new_top, new_bottom) = bit_unpack(packed);
        assert_eq!(top, new_top);
        assert_eq!(bottom, new_bottom);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn initialize_blockchain_test() {
        let mut t = test::TestManager::new();

        // create first block, with 100 VIP txs with 1_000_000_000 NOLAN each
        t.initialize(100, 1_000_000_000).await;
        t.wait_for_mining_event().await;

        {
            let (blockchain, _blockchain_) =
                lock_for_read!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);
            assert_eq!(1, blockchain.get_latest_block_id());
        }
        t.check_blockchain().await;
        t.check_utxoset().await;
        t.check_token_supply().await;
    }

    #[tokio::test]
    #[serial_test::serial]
    //
    // test we can produce five blocks in a row
    //
    async fn add_five_good_blocks() {
        // let filter = tracing_subscriber::EnvFilter::from_default_env();
        // let fmt_layer = tracing_subscriber::fmt::Layer::default().with_filter(filter);
        //
        // tracing_subscriber::registry().with(fmt_layer).init();

        let mut t = TestManager::new();
        let block1;
        let block1_id;
        let block1_hash;
        let ts;

        //
        // block 1
        //
        t.initialize(100, 1_000_000_000).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);
            block1 = blockchain.get_latest_block().unwrap();
            block1_id = block1.id;
            block1_hash = block1.hash;
            ts = block1.timestamp;

            assert_eq!(blockchain.get_latest_block_hash(), block1_hash);
            assert_eq!(blockchain.get_latest_block_id(), block1_id);
            assert_eq!(blockchain.get_latest_block_id(), 1);
        }

        //
        // block 2
        //
        let mut block2 = t
            .create_block(
                block1_hash, // hash of parent block
                ts + 120000, // timestamp
                0,           // num transactions
                0,           // amount
                0,           // fee
                true,        // mine golden ticket
            )
            .await;
        block2.generate(); // generate hashes

        let block2_hash = block2.hash;
        let block2_id = block2.id;

        t.add_block(block2).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_eq!(blockchain.get_latest_block_hash(), block2_hash);
            assert_eq!(blockchain.get_latest_block_id(), block2_id);
            assert_eq!(blockchain.get_latest_block_id(), 2);
        }

        //
        // block 3
        //
        let mut block3 = t
            .create_block(
                block2_hash, // hash of parent block
                ts + 240000, // timestamp
                0,           // num transactions
                0,           // amount
                0,           // fee
                true,        // mine golden ticket
            )
            .await;
        block3.generate(); // generate hashes

        let block3_hash = block3.hash;
        let block3_id = block3.id;

        t.add_block(block3).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_ne!(blockchain.get_latest_block_hash(), block2_hash);
            assert_ne!(blockchain.get_latest_block_id(), block2_id);
            assert_eq!(blockchain.get_latest_block_hash(), block3_hash);
            assert_eq!(blockchain.get_latest_block_id(), block3_id);
            assert_eq!(blockchain.get_latest_block_id(), 3);
        }

        //
        // block 4
        //
        let mut block4 = t
            .create_block(
                block3_hash, // hash of parent block
                ts + 360000, // timestamp
                0,           // num transactions
                0,           // amount
                0,           // fee
                true,        // mine golden ticket
            )
            .await;
        block4.generate(); // generate hashes

        let block4_hash = block4.hash;
        let block4_id = block4.id;

        t.add_block(block4).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_ne!(blockchain.get_latest_block_hash(), block2_hash);
            assert_ne!(blockchain.get_latest_block_id(), block2_id);
            assert_ne!(blockchain.get_latest_block_hash(), block3_hash);
            assert_ne!(blockchain.get_latest_block_id(), block3_id);
            assert_eq!(blockchain.get_latest_block_hash(), block4_hash);
            assert_eq!(blockchain.get_latest_block_id(), block4_id);
            assert_eq!(blockchain.get_latest_block_id(), 4);
        }

        //
        // block 5
        //
        let mut block5 = t
            .create_block(
                block4_hash, // hash of parent block
                ts + 480000, // timestamp
                0,           // num transactions
                0,           // amount
                0,           // fee
                true,        // mine golden ticket
            )
            .await;
        block5.generate(); // generate hashes

        let block5_hash = block5.hash;
        let block5_id = block5.id;

        t.add_block(block5).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_ne!(blockchain.get_latest_block_hash(), block2_hash);
            assert_ne!(blockchain.get_latest_block_id(), block2_id);
            assert_ne!(blockchain.get_latest_block_hash(), block3_hash);
            assert_ne!(blockchain.get_latest_block_id(), block3_id);
            assert_ne!(blockchain.get_latest_block_hash(), block4_hash);
            assert_ne!(blockchain.get_latest_block_id(), block4_id);
            assert_eq!(blockchain.get_latest_block_hash(), block5_hash);
            assert_eq!(blockchain.get_latest_block_id(), block5_id);
            assert_eq!(blockchain.get_latest_block_id(), 5);
        }

        t.check_blockchain().await;
        t.check_utxoset().await;
        t.check_token_supply().await;

        {
            let (wallet, _wallet_) = lock_for_read!(t.wallet_lock, LOCK_ORDER_WALLET);
            let count = wallet.get_unspent_slip_count();
            assert_ne!(count, 0);
            let balance = wallet.get_available_balance();
            assert_ne!(balance, 0);
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    //
    // test we do not add blocks because of insufficient mining
    //
    async fn insufficient_golden_tickets_test() {
        // let filter = tracing_subscriber::EnvFilter::from_default_env();
        // let fmt_layer = tracing_subscriber::fmt::Layer::default().with_filter(filter);
        //
        // tracing_subscriber::registry().with(fmt_layer).init();

        let mut t = TestManager::new();
        let block1;
        let block1_id;
        let block1_hash;
        let ts;

        //
        // block 1
        //
        t.initialize(100, 1_000_000_000).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            block1 = blockchain.get_latest_block().unwrap();
            block1_id = block1.id;
            block1_hash = block1.hash;
            ts = block1.timestamp;

            assert_eq!(blockchain.get_latest_block_hash(), block1_hash);
            assert_eq!(blockchain.get_latest_block_id(), block1_id);
            assert_eq!(blockchain.get_latest_block_id(), 1);
        }

        //
        // block 2
        //
        let mut block2 = t
            .create_block(
                block1_hash, // hash of parent block
                ts + 120000, // timestamp
                1,           // num transactions
                0,           // amount
                0,           // fee
                false,       // mine golden ticket
            )
            .await;
        block2.generate(); // generate hashes

        let block2_hash = block2.hash;
        let block2_id = block2.id;

        t.add_block(block2).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_eq!(blockchain.get_latest_block_hash(), block2_hash);
            assert_eq!(blockchain.get_latest_block_id(), block2_id);
            assert_eq!(blockchain.get_latest_block_id(), 2);
        }

        //
        // block 3
        //
        let mut block3 = t
            .create_block(
                block2_hash, // hash of parent block
                ts + 240000, // timestamp
                1,           // num transactions
                0,           // amount
                0,           // fee
                false,       // mine golden ticket
            )
            .await;
        block3.generate(); // generate hashes

        let block3_hash = block3.hash;
        let block3_id = block3.id;

        t.add_block(block3).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_ne!(blockchain.get_latest_block_hash(), block2_hash);
            assert_ne!(blockchain.get_latest_block_id(), block2_id);
            assert_eq!(blockchain.get_latest_block_hash(), block3_hash);
            assert_eq!(blockchain.get_latest_block_id(), block3_id);
            assert_eq!(blockchain.get_latest_block_id(), 3);
        }

        //
        // block 4
        //
        let mut block4 = t
            .create_block(
                block3_hash, // hash of parent block
                ts + 360000, // timestamp
                1,           // num transactions
                0,           // amount
                0,           // fee
                false,       // mine golden ticket
            )
            .await;
        block4.generate(); // generate hashes

        let block4_hash = block4.hash;
        let block4_id = block4.id;

        t.add_block(block4).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_ne!(blockchain.get_latest_block_hash(), block2_hash);
            assert_ne!(blockchain.get_latest_block_id(), block2_id);
            assert_ne!(blockchain.get_latest_block_hash(), block3_hash);
            assert_ne!(blockchain.get_latest_block_id(), block3_id);
            assert_eq!(blockchain.get_latest_block_hash(), block4_hash);
            assert_eq!(blockchain.get_latest_block_id(), block4_id);
            assert_eq!(blockchain.get_latest_block_id(), 4);
        }

        //
        // block 5
        //
        let mut block5 = t
            .create_block(
                block4_hash, // hash of parent block
                ts + 480000, // timestamp
                1,           // num transactions
                0,           // amount
                0,           // fee
                false,       // mine golden ticket
            )
            .await;
        block5.generate(); // generate hashes

        let block5_hash = block5.hash;
        let block5_id = block5.id;

        t.add_block(block5).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_ne!(blockchain.get_latest_block_hash(), block2_hash);
            assert_ne!(blockchain.get_latest_block_id(), block2_id);
            assert_ne!(blockchain.get_latest_block_hash(), block3_hash);
            assert_ne!(blockchain.get_latest_block_id(), block3_id);
            assert_ne!(blockchain.get_latest_block_hash(), block4_hash);
            assert_ne!(blockchain.get_latest_block_id(), block4_id);
            assert_eq!(blockchain.get_latest_block_hash(), block5_hash);
            assert_eq!(blockchain.get_latest_block_id(), block5_id);
            assert_eq!(blockchain.get_latest_block_id(), 5);
        }

        //
        // block 6
        //
        let mut block6 = t
            .create_block(
                block5_hash, // hash of parent block
                ts + 600000, // timestamp
                1,           // num transactions
                0,           // amount
                0,           // fee
                false,       // mine golden ticket
            )
            .await;
        block6.generate(); // generate hashes

        let block6_hash = block6.hash;
        let block6_id = block6.id;

        t.add_block(block6).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_ne!(blockchain.get_latest_block_hash(), block2_hash);
            assert_ne!(blockchain.get_latest_block_id(), block2_id);
            assert_ne!(
                hex::encode(blockchain.get_latest_block_hash()),
                hex::encode(block3_hash)
            );
            assert_ne!(blockchain.get_latest_block_id(), block3_id);
            assert_ne!(
                hex::encode(blockchain.get_latest_block_hash()),
                hex::encode(block4_hash)
            );
            assert_ne!(blockchain.get_latest_block_id(), block4_id);
            assert_ne!(
                hex::encode(blockchain.get_latest_block_hash()),
                hex::encode(block5_hash)
            );
            assert_ne!(blockchain.get_latest_block_id(), block5_id);
            assert_eq!(blockchain.get_latest_block_hash(), block6_hash);
            assert_eq!(blockchain.get_latest_block_id(), block6_id);
            assert_eq!(blockchain.get_latest_block_id(), 6);
        }

        //
        // block 7
        //
        let mut block7 = t
            .create_block(
                block6_hash, // hash of parent block
                ts + 720000, // timestamp
                1,           // num transactions
                0,           // amount
                0,           // fee
                false,       // mine golden ticket
            )
            .await;
        block7.generate(); // generate hashes

        let block7_hash = block7.hash;
        let block7_id = block7.id;

        t.add_block(block7).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_ne!(blockchain.get_latest_block_hash(), block2_hash);
            assert_ne!(blockchain.get_latest_block_id(), block2_id);
            assert_ne!(blockchain.get_latest_block_hash(), block3_hash);
            assert_ne!(blockchain.get_latest_block_id(), block3_id);
            assert_ne!(blockchain.get_latest_block_hash(), block4_hash);
            assert_ne!(blockchain.get_latest_block_id(), block4_id);
            assert_ne!(blockchain.get_latest_block_hash(), block5_hash);
            assert_ne!(blockchain.get_latest_block_id(), block5_id);
            assert_eq!(blockchain.get_latest_block_hash(), block6_hash);
            assert_eq!(blockchain.get_latest_block_id(), block6_id);
            assert_ne!(blockchain.get_latest_block_hash(), block7_hash);
            assert_ne!(blockchain.get_latest_block_id(), block7_id);
            assert_eq!(blockchain.get_latest_block_id(), 6);
        }

        t.check_blockchain().await;
        t.check_utxoset().await;
        t.check_token_supply().await;
    }

    #[tokio::test]
    #[serial_test::serial]
    //
    // test we do not add blocks because of insufficient mining
    //
    async fn seven_blocks_with_sufficient_golden_tickets_test() {
        let mut t = TestManager::new();
        let block1;
        let block1_id;
        let block1_hash;
        let ts;

        //
        // block 1
        //
        t.initialize(100, 1_000_000_000).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            block1 = blockchain.get_latest_block().unwrap();
            block1_hash = block1.hash;
            block1_id = block1.id;
            ts = block1.timestamp;

            assert_eq!(blockchain.get_latest_block_hash(), block1_hash);
            assert_eq!(blockchain.get_latest_block_id(), block1_id);
            assert_eq!(blockchain.get_latest_block_id(), 1);
        }

        //
        // block 2
        //
        let mut block2 = t
            .create_block(
                block1_hash, // hash of parent block
                ts + 120000, // timestamp
                0,           // num transactions
                0,           // amount
                0,           // fee
                true,        // mine golden ticket
            )
            .await;
        block2.generate(); // generate hashes

        let block2_hash = block2.hash;
        let block2_id = block2.id;

        t.add_block(block2).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_eq!(blockchain.get_latest_block_hash(), block2_hash);
            assert_eq!(blockchain.get_latest_block_id(), block2_id);
            assert_eq!(blockchain.get_latest_block_id(), 2);
        }

        //
        // block 3
        //
        let mut block3 = t
            .create_block(
                block2_hash, // hash of parent block
                ts + 240000, // timestamp
                1,           // num transactions
                0,           // amount
                0,           // fee
                false,       // mine golden ticket
            )
            .await;
        block3.generate(); // generate hashes

        let block3_hash = block3.hash;
        let block3_id = block3.id;

        t.add_block(block3).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_ne!(blockchain.get_latest_block_hash(), block2_hash);
            assert_ne!(blockchain.get_latest_block_id(), block2_id);
            assert_eq!(blockchain.get_latest_block_hash(), block3_hash);
            assert_eq!(blockchain.get_latest_block_id(), block3_id);
            assert_eq!(blockchain.get_latest_block_id(), 3);
        }

        //
        // block 4
        //
        let mut block4 = t
            .create_block(
                block3_hash, // hash of parent block
                ts + 360000, // timestamp
                0,           // num transactions
                0,           // amount
                0,           // fee
                true,        // mine golden ticket
            )
            .await;
        block4.generate(); // generate hashes

        let block4_hash = block4.hash;
        let block4_id = block4.id;

        t.add_block(block4).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_ne!(blockchain.get_latest_block_hash(), block2_hash);
            assert_ne!(blockchain.get_latest_block_id(), block2_id);
            assert_ne!(blockchain.get_latest_block_hash(), block3_hash);
            assert_ne!(blockchain.get_latest_block_id(), block3_id);
            assert_eq!(blockchain.get_latest_block_hash(), block4_hash);
            assert_eq!(blockchain.get_latest_block_id(), block4_id);
            assert_eq!(blockchain.get_latest_block_id(), 4);
        }

        //
        // block 5
        //
        let mut block5 = t
            .create_block(
                block4_hash, // hash of parent block
                ts + 480000, // timestamp
                1,           // num transactions
                0,           // amount
                0,           // fee
                false,       // mine golden ticket
            )
            .await;
        block5.generate(); // generate hashes

        let block5_hash = block5.hash;
        let block5_id = block5.id;

        t.add_block(block5).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_ne!(blockchain.get_latest_block_hash(), block2_hash);
            assert_ne!(blockchain.get_latest_block_id(), block2_id);
            assert_ne!(blockchain.get_latest_block_hash(), block3_hash);
            assert_ne!(blockchain.get_latest_block_id(), block3_id);
            assert_ne!(blockchain.get_latest_block_hash(), block4_hash);
            assert_ne!(blockchain.get_latest_block_id(), block4_id);
            assert_eq!(blockchain.get_latest_block_hash(), block5_hash);
            assert_eq!(blockchain.get_latest_block_id(), block5_id);
            assert_eq!(blockchain.get_latest_block_id(), 5);
        }

        //
        // block 6
        //
        let mut block6 = t
            .create_block(
                block5_hash, // hash of parent block
                ts + 600000, // timestamp
                0,           // num transactions
                0,           // amount
                0,           // fee
                true,        // mine golden ticket
            )
            .await;
        block6.generate(); // generate hashes

        let block6_hash = block6.hash;
        let block6_id = block6.id;

        t.add_block(block6).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_ne!(blockchain.get_latest_block_hash(), block2_hash);
            assert_ne!(blockchain.get_latest_block_id(), block2_id);
            assert_ne!(blockchain.get_latest_block_hash(), block3_hash);
            assert_ne!(blockchain.get_latest_block_id(), block3_id);
            assert_ne!(blockchain.get_latest_block_hash(), block4_hash);
            assert_ne!(blockchain.get_latest_block_id(), block4_id);
            assert_ne!(blockchain.get_latest_block_hash(), block5_hash);
            assert_ne!(blockchain.get_latest_block_id(), block5_id);
            assert_eq!(blockchain.get_latest_block_hash(), block6_hash);
            assert_eq!(blockchain.get_latest_block_id(), block6_id);
            assert_eq!(blockchain.get_latest_block_id(), 6);
        }

        //
        // block 7
        //
        let mut block7 = t
            .create_block(
                block6_hash, // hash of parent block
                ts + 720000, // timestamp
                1,           // num transactions
                0,           // amount
                0,           // fee
                false,       // mine golden ticket
            )
            .await;
        block7.generate(); // generate hashes

        let block7_hash = block7.hash;
        let block7_id = block7.id;

        t.add_block(block7).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_ne!(blockchain.get_latest_block_hash(), block1_hash);
            assert_ne!(blockchain.get_latest_block_id(), block1_id);
            assert_ne!(blockchain.get_latest_block_hash(), block2_hash);
            assert_ne!(blockchain.get_latest_block_id(), block2_id);
            assert_ne!(blockchain.get_latest_block_hash(), block3_hash);
            assert_ne!(blockchain.get_latest_block_id(), block3_id);
            assert_ne!(blockchain.get_latest_block_hash(), block4_hash);
            assert_ne!(blockchain.get_latest_block_id(), block4_id);
            assert_ne!(blockchain.get_latest_block_hash(), block5_hash);
            assert_ne!(blockchain.get_latest_block_id(), block5_id);
            assert_ne!(blockchain.get_latest_block_hash(), block6_hash);
            assert_ne!(blockchain.get_latest_block_id(), block6_id);
            assert_eq!(blockchain.get_latest_block_hash(), block7_hash);
            assert_eq!(blockchain.get_latest_block_id(), block7_id);
            assert_eq!(blockchain.get_latest_block_id(), 7);
        }

        t.check_blockchain().await;
        t.check_utxoset().await;
        t.check_token_supply().await;
    }

    #[tokio::test]
    #[serial_test::serial]
    //
    // add 6 blocks including 4 block reorg
    //
    async fn basic_longest_chain_reorg_test() {
        let mut t = TestManager::new();
        let block1;
        let block1_id;
        let block1_hash;
        let ts;

        //
        // block 1
        //
        t.initialize(100, 1_000_000_000).await;

        {
            let (blockchain, _blockchain_) =
                lock_for_read!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            block1 = blockchain.get_latest_block().unwrap();
            block1_hash = block1.hash;
            block1_id = block1.id;
            ts = block1.timestamp;
        }

        //
        // block 2
        //
        let mut block2 = t
            .create_block(
                block1_hash, // hash of parent block
                ts + 120000, // timestamp
                0,           // num transactions
                0,           // amount
                0,           // fee
                true,        // mine golden ticket
            )
            .await;
        block2.generate(); // generate hashes

        let block2_hash = block2.hash;
        let block2_id = block2.id;

        t.add_block(block2).await;

        {
            let (blockchain, _blockchain_) =
                lock_for_read!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_eq!(blockchain.get_latest_block_hash(), block2_hash);
            assert_eq!(blockchain.get_latest_block_id(), block2_id);
        }

        //
        // block 3
        //
        let mut block3 = t
            .create_block(
                block2_hash, // hash of parent block
                ts + 240000, // timestamp
                1,           // num transactions
                0,           // amount
                0,           // fee
                false,       // mine golden ticket
            )
            .await;
        block3.generate(); // generate hashes
        let block3_hash = block3.hash;
        let _block3_id = block3.id;
        t.add_block(block3).await;

        //
        // block 4
        //
        let mut block4 = t
            .create_block(
                block3_hash, // hash of parent block
                ts + 360000, // timestamp
                0,           // num transactions
                0,           // amount
                0,           // fee
                true,        // mine golden ticket
            )
            .await;
        block4.generate(); // generate hashes
        let block4_hash = block4.hash;
        let _block4_id = block4.id;
        t.add_block(block4).await;

        //
        // block 5
        //
        let mut block5 = t
            .create_block(
                block4_hash, // hash of parent block
                ts + 480000, // timestamp
                1,           // num transactions
                0,           // amount
                0,           // fee
                false,       // mine golden ticket
            )
            .await;
        block5.generate(); // generate hashes
        let block5_hash = block5.hash;
        let block5_id = block5.id;
        t.add_block(block5).await;

        {
            let (blockchain, _blockchain_) =
                lock_for_read!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_eq!(blockchain.get_latest_block_hash(), block5_hash);
            assert_eq!(blockchain.get_latest_block_id(), block5_id);
        }

        //
        //  block3-2
        //
        let mut block3_2 = t
            .create_block(
                block2_hash, // hash of parent block
                ts + 240000, // timestamp
                0,           // num transactions
                0,           // amount
                0,           // fee
                true,        // mine golden ticket
            )
            .await;
        block3_2.generate(); // generate hashes
        let block3_2_hash = block3_2.hash;
        let _block3_2_id = block3_2.id;
        t.add_block(block3_2).await;

        {
            let (blockchain, _blockchain_) =
                lock_for_read!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_eq!(blockchain.get_latest_block_hash(), block5_hash);
            assert_eq!(blockchain.get_latest_block_id(), block5_id);
        }

        //
        //  block4-2
        //
        let mut block4_2 = t
            .create_block(
                block3_2_hash, // hash of parent block
                ts + 360000,   // timestamp
                0,             // num transactions
                0,             // amount
                0,             // fee
                true,          // mine golden ticket
            )
            .await;
        block4_2.generate(); // generate hashes
        let block4_2_hash = block4_2.hash;
        let _block4_2_id = block4_2.id;
        t.add_block(block4_2).await;

        {
            let (blockchain, _blockchain_) =
                lock_for_read!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_eq!(blockchain.get_latest_block_hash(), block5_hash);
            assert_eq!(blockchain.get_latest_block_id(), block5_id);
        }

        //
        //  block5-2
        //
        let mut block5_2 = t
            .create_block(
                block4_2_hash, // hash of parent block
                ts + 480000,   // timestamp
                1,             // num transactions
                0,             // amount
                0,             // fee
                false,         // mine golden ticket
            )
            .await;
        block5_2.generate(); // generate hashes
        let block5_2_hash = block5_2.hash;
        let _block5_2_id = block5_2.id;
        t.add_block(block5_2).await;

        {
            let (blockchain, _blockchain_) =
                lock_for_read!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_eq!(blockchain.get_latest_block_hash(), block5_hash);
            assert_eq!(blockchain.get_latest_block_id(), block5_id);
        }

        //
        //  block6_2
        //
        let mut block6_2 = t
            .create_block(
                block5_2_hash, // hash of parent block
                ts + 600000,   // timestamp
                0,             // num transactions
                0,             // amount
                0,             // fee
                true,          // mine golden ticket
            )
            .await;
        block6_2.generate(); // generate hashes
        let block6_2_hash = block6_2.hash;
        let block6_2_id = block6_2.id;
        t.add_block(block6_2).await;

        {
            let (blockchain, _blockchain_) =
                lock_for_read!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            assert_eq!(blockchain.get_latest_block_hash(), block6_2_hash);
            assert_eq!(blockchain.get_latest_block_id(), block6_2_id);
            assert_eq!(blockchain.get_latest_block_id(), 6);
        }

        t.check_blockchain().await;
        t.check_utxoset().await;
        t.check_token_supply().await;
    }

    /// Loading blocks into a blockchain which were created from another blockchain instance
    #[tokio::test]
    #[serial_test::serial]
    async fn load_blocks_from_another_blockchain_test() {
        // pretty_env_logger::init();
        let mut t = TestManager::new();
        let mut t2 = TestManager::new();
        let block1;
        let block1_id;
        let block1_hash;
        let ts;

        // block 1
        t.initialize(100, 1_000_000_000).await;

        {
            let (mut blockchain, _blockchain_) =
                lock_for_write!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            block1 = blockchain.get_latest_block().unwrap();
            block1_id = block1.id;
            block1_hash = block1.hash;
            ts = block1.timestamp;
        }

        // block 2
        let mut block2 = t
            .create_block(
                block1_hash, // hash of parent block
                ts + 120000, // timestamp
                0,           // num transactions
                0,           // amount
                0,           // fee
                true,        // mine golden ticket
            )
            .await;
        block2.generate(); // generate hashes

        let block2_hash = block2.hash;
        let _block2_id = block2.id;

        t.add_block(block2).await;

        t2.storage
            .load_blocks_from_disk(t2.mempool_lock.clone())
            .await;
        {
            let (mut blockchain2, _blockchain2_) =
                lock_for_write!(t2.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            blockchain2
                .add_blocks_from_mempool(
                    t2.mempool_lock.clone(),
                    &t2.network,
                    &mut t2.storage,
                    t2.sender_to_miner.clone(),
                )
                .await;
        }

        {
            let blockchain1 = t.blockchain_lock.read().await;
            let blockchain2 = t2.blockchain_lock.read().await;

            assert_eq!(blockchain1.blocks.len(), 2);
            assert_eq!(blockchain2.blocks.len(), 2);

            let block1_chain1 = blockchain1.get_block(&block1_hash).unwrap();
            let block1_chain2 = blockchain2.get_block(&block1_hash).unwrap();

            let block2_chain1 = blockchain1.get_block(&block2_hash).unwrap();
            let block2_chain2 = blockchain2.get_block(&block2_hash).unwrap();

            for (block_new, block_old) in [
                (block1_chain2, block1_chain1),
                (block2_chain2, block2_chain1),
            ] {
                assert_eq!(block_new.hash, block_old.hash);
                assert_eq!(block_new.has_golden_ticket, block_old.has_golden_ticket);
                assert_eq!(block_new.previous_block_hash, block_old.previous_block_hash);
                assert_eq!(block_new.block_type, block_old.block_type);
                assert_eq!(block_new.signature, block_old.signature);
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn fork_id_test() {
        // pretty_env_logger::init();

        let mut t = TestManager::new();
        let mut block1;
        let mut block1_id;
        let mut block1_hash;
        let mut ts;

        t.initialize_with_timestamp(100, 1_000_000_000, 10_000_000)
            .await;

        for _i in (0..20).step_by(1) {
            {
                let (blockchain, _blockchain_) =
                    lock_for_read!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

                block1 = blockchain.get_latest_block().unwrap();
                block1_hash = block1.hash;
                block1_id = block1.id;
                ts = block1.timestamp;
            }

            let mut block = t
                .create_block(
                    block1_hash, // hash of parent block
                    ts + 120000, // timestamp
                    0,           // num transactions
                    0,           // amount
                    0,           // fee
                    true,        // mine golden ticket
                )
                .await;
            block.generate(); // generate hashes

            let _block_hash = block.hash;
            let _block_id = block.id;

            t.add_block(block).await;

            let _result = t.receiver_in_miner.try_recv();
        }

        {
            let (blockchain, _blockchain_) =
                lock_for_read!(t.blockchain_lock, LOCK_ORDER_BLOCKCHAIN);

            let fork_id = blockchain.generate_fork_id(15);
            assert_ne!(fork_id, [0; 32]);
            assert_eq!(fork_id[2..], [0; 30]);

            let fork_id = blockchain.generate_fork_id(20);
            assert_ne!(fork_id, [0; 32]);
            assert_eq!(fork_id[4..], [0; 28]);
        }
    }
}
