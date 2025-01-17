use std::collections::VecDeque;
use std::time::Duration;

use ahash::AHashMap;
use rayon::prelude::*;
use tracing::{debug, info, trace, warn};

use crate::common::defs::{Currency, SaitoHash, SaitoPrivateKey, SaitoPublicKey, SaitoSignature};
use crate::core::data::block::Block;
use crate::core::data::blockchain::Blockchain;
use crate::core::data::burnfee::BurnFee;
use crate::core::data::crypto::hash;
use crate::core::data::golden_ticket::GoldenTicket;
use crate::core::data::transaction::{Transaction, TransactionType};

//
// In addition to responding to global broadcast messages, the
// mempool has a local broadcast channel it uses to coordinate
// attempts to bundle blocks and notify itself when a block has
// been produced.
//
#[derive(Clone, Debug)]
pub enum MempoolMessage {
    LocalTryBundleBlock,
    LocalNewBlock,
}

/// The `Mempool` holds unprocessed blocks and transactions and is in control of
/// discerning when the node is allowed to create a block. It bundles the block and
/// sends it to the `Blockchain` to be added to the longest-chain. New `Block`s
/// received over the network are queued in the `Mempool` before being added to
/// the `Blockchain`
#[derive(Debug)]
pub struct Mempool {
    pub blocks_queue: VecDeque<Block>,
    pub transactions: AHashMap<SaitoSignature, Transaction>,
    pub golden_tickets: AHashMap<SaitoHash, (Transaction, bool)>,
    // vector so we just copy it over
    routing_work_in_mempool: Currency,
    pub new_tx_added: bool,
    pub(crate) public_key: SaitoPublicKey,
    private_key: SaitoPrivateKey,
}

impl Mempool {
    #[allow(clippy::new_without_default)]
    pub fn new(public_key: SaitoPublicKey, private_key: SaitoPrivateKey) -> Self {
        Mempool {
            blocks_queue: VecDeque::new(),
            transactions: Default::default(),
            golden_tickets: Default::default(),
            routing_work_in_mempool: 0,
            new_tx_added: false,
            public_key,
            private_key,
        }
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub fn add_block(&mut self, block: Block) {
        debug!("mempool add block : {:?}", hex::encode(block.hash));
        let hash_to_insert = block.hash;
        if !self
            .blocks_queue
            .par_iter()
            .any(|block| block.hash == hash_to_insert)
        {
            self.blocks_queue.push_back(block);
        } else {
            debug!("block not added to mempool as it was already there");
        }
    }
    #[tracing::instrument(level = "info", skip_all)]
    pub async fn add_golden_ticket(&mut self, golden_ticket: Transaction) {
        let gt = GoldenTicket::deserialize_from_net(&golden_ticket.message);
        info!(
            "adding golden ticket : {:?} target : {:?} public_key : {:?}",
            hex::encode(hash(&golden_ticket.serialize_for_net())),
            hex::encode(gt.target),
            hex::encode(gt.public_key)
        );
        // TODO : should we replace others' GT with our GT if targets are similar ?
        if self.golden_tickets.contains_key(&gt.target) {
            debug!(
                "similar golden ticket already exists : {:?}",
                hex::encode(gt.target)
            );
            return;
        }
        self.golden_tickets
            .insert(gt.target, (golden_ticket, false));

        info!("golden ticket added to mempool");
    }
    #[tracing::instrument(level = "info", skip_all)]
    pub async fn add_transaction_if_validates(
        &mut self,
        mut transaction: Transaction,
        blockchain: &Blockchain,
    ) {
        trace!(
            "add transaction if validates : {:?}",
            hex::encode(transaction.hash_for_signature.unwrap())
        );
        transaction.generate(&self.public_key, 0, 0);
        // validate
        if transaction.validate(&blockchain.utxoset) {
            self.add_transaction(transaction).await;
        } else {
            debug!(
                "transaction not valid : {:?}",
                transaction.hash_for_signature.unwrap()
            );
        }
    }
    #[tracing::instrument(level = "info", skip_all)]
    pub async fn add_transaction(&mut self, transaction: Transaction) {
        trace!(
            "add_transaction {:?} : type = {:?}",
            hex::encode(transaction.hash_for_signature.unwrap()),
            transaction.transaction_type
        );

        debug_assert!(transaction.hash_for_signature.is_some());
        //
        // this assigns the amount of routing work that this transaction
        // contains to us, which is why we need to provide our public_key
        // so that we can calculate routing work.
        //

        //
        // generates hashes, total fees, routing work for me, etc.
        //
        // transaction.generate(&self.public_key, 0, 0);

        if !self.transactions.contains_key(&transaction.signature) {
            self.routing_work_in_mempool += transaction.total_work_for_me;
            debug!(
                "routing work available in mempool : {:?} after adding work : {:?} from tx with fees : {:?}",
                self.routing_work_in_mempool, transaction.total_work_for_me, transaction.total_fees
            );
            if let TransactionType::GoldenTicket = transaction.transaction_type {
                panic!("golden tickets should be in gt collection");
            } else {
                self.transactions.insert(transaction.signature, transaction);
                self.new_tx_added = true;
            }
        }
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub async fn bundle_block(
        &mut self,
        blockchain: &mut Blockchain,
        current_timestamp: u64,
        gt_tx: Option<Transaction>,
    ) -> Option<Block> {
        let mempool_work = self
            .can_bundle_block(blockchain, current_timestamp, &gt_tx)
            .await?;
        info!(
            "bundling block with {:?} txs with work : {:?}",
            self.transactions.len(),
            mempool_work
        );

        let previous_block_hash: SaitoHash;
        {
            previous_block_hash = blockchain.get_latest_block_hash();
        }

        let mut block = Block::create(
            &mut self.transactions,
            previous_block_hash,
            blockchain,
            current_timestamp,
            &self.public_key,
            &self.private_key,
            gt_tx,
        )
        .await;
        block.generate();
        info!(
            "block generated with work : {:?} and burnfee : {:?}",
            block.total_work, block.burnfee
        );
        // assert_eq!(block.total_work, mempool_work);
        self.new_tx_added = false;
        self.routing_work_in_mempool = 0;

        Some(block)
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub async fn bundle_genesis_block(
        &mut self,
        blockchain: &mut Blockchain,
        current_timestamp: u64,
    ) -> Block {
        debug!("bundling genesis block...");

        let mut block = Block::create(
            &mut self.transactions,
            [0; 32],
            blockchain,
            current_timestamp,
            &self.public_key,
            &self.private_key,
            None,
        )
        .await;
        block.generate();
        self.new_tx_added = false;
        self.routing_work_in_mempool = 0;

        block
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub async fn can_bundle_block(
        &self,
        blockchain: &Blockchain,
        current_timestamp: u64,
        gt_tx: &Option<Transaction>,
    ) -> Option<Currency> {
        // if self.transactions.is_empty() {
        //     return false;
        // }
        // trace!("can bundle block : timestamp = {:?}", current_timestamp);

        // TODO : add checks [downloading_active,etc...] from SLR code here

        if blockchain.blocks.is_empty() {
            warn!("Not generating #1 block. Waiting for blocks from peers");
            tokio::time::sleep(Duration::from_secs(1)).await;
            return None;
        }
        if !self.blocks_queue.is_empty() {
            return None;
        }
        if self.transactions.is_empty() || !self.new_tx_added {
            return None;
        }
        if !blockchain
            .is_golden_ticket_count_valid(blockchain.get_latest_block_hash(), gt_tx.is_some())
        {
            trace!("waiting till more golden tickets come in");
            return None;
        }

        if let Some(previous_block) = blockchain.get_latest_block() {
            let work_available = self.get_routing_work_available();
            let work_needed = BurnFee::return_routing_work_needed_to_produce_block_in_nolan(
                previous_block.burnfee,
                current_timestamp,
                previous_block.timestamp,
            );
            let time_elapsed = current_timestamp - previous_block.timestamp;

            let result = work_available >= work_needed;
            if result {
                info!(
                "last ts: {:?}, this ts: {:?}, work available: {:?}, work needed: {:?}, time_elapsed : {:?} can_bundle : {:?}",
                previous_block.timestamp, current_timestamp, work_available, work_needed, time_elapsed, true
                );
            } else {
                debug!(
                "last ts: {:?}, this ts: {:?}, work available: {:?}, work needed: {:?}, time_elapsed : {:?} can_bundle : {:?}",
                previous_block.timestamp, current_timestamp, work_available, work_needed, time_elapsed, false
                );
            }
            if result {
                return Some(work_available);
            }
            None
        } else {
            Some(0)
        }
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub fn delete_block(&mut self, block_hash: &SaitoHash) {
        debug!(
            "deleting block from mempool : {:?}",
            hex::encode(block_hash)
        );

        self.golden_tickets.remove(block_hash);
        // self.blocks_queue.retain(|block| !block.hash.eq(block_hash));
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub fn delete_transactions(&mut self, transactions: &Vec<Transaction>) {
        for transaction in transactions {
            if let TransactionType::GoldenTicket = transaction.transaction_type {
                let gt = GoldenTicket::deserialize_from_net(&transaction.message);
                self.golden_tickets.remove(&gt.target);
            } else {
                self.transactions.remove(&transaction.signature);
            }
        }

        self.routing_work_in_mempool = 0;

        // add routing work from remaining tx
        for (_, transaction) in &self.transactions {
            self.routing_work_in_mempool += transaction.total_work_for_me;
        }
    }

    ///
    /// Calculates the work available in mempool to produce a block
    ///
    pub fn get_routing_work_available(&self) -> Currency {
        self.routing_work_in_mempool
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::RwLock;

    use crate::common::defs::{
        push_lock, LOCK_ORDER_BLOCKCHAIN, LOCK_ORDER_MEMPOOL, LOCK_ORDER_WALLET,
    };
    use crate::common::test_manager::test::{create_timestamp, TestManager};
    use crate::core::data::burnfee::HEARTBEAT;
    use crate::core::data::wallet::Wallet;
    use crate::{lock_for_read, lock_for_write};

    use super::*;

    #[test]
    fn mempool_new_test() {
        let mempool = Mempool::new([0; 33], [0; 32]);
        assert_eq!(mempool.blocks_queue, VecDeque::new());
    }

    #[test]
    fn mempool_add_block_test() {
        let mut mempool = Mempool::new([0; 33], [0; 32]);
        let block = Block::new();
        mempool.add_block(block.clone());
        assert_eq!(Some(block), mempool.blocks_queue.pop_front())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mempool_bundle_blocks_test() {
        let mempool_lock: Arc<RwLock<Mempool>>;
        let wallet_lock: Arc<RwLock<Wallet>>;
        let blockchain_lock: Arc<RwLock<Blockchain>>;
        let public_key: SaitoPublicKey;
        let private_key: SaitoPrivateKey;

        {
            let mut t = TestManager::new();
            t.initialize(100, 720_000).await;
            t.wait_for_mining_event().await;

            wallet_lock = t.get_wallet_lock();
            mempool_lock = t.get_mempool_lock();
            blockchain_lock = t.get_blockchain_lock();
        }

        {
            let (wallet, _wallet_) = lock_for_read!(wallet_lock, LOCK_ORDER_WALLET);

            public_key = wallet.public_key;
            private_key = wallet.private_key;
        }

        let ts = create_timestamp();
        let _next_block_timestamp = ts + (HEARTBEAT * 2);

        let (blockchain, _blockchain_) = lock_for_read!(blockchain_lock, LOCK_ORDER_BLOCKCHAIN);
        let (mut mempool, _mempool_) = lock_for_write!(mempool_lock, LOCK_ORDER_MEMPOOL);

        let _txs = Vec::<Transaction>::new();

        assert_eq!(mempool.get_routing_work_available(), 0);

        for _i in 0..5 {
            let mut tx = Transaction::default();

            {
                let (mut wallet, _wallet_) = lock_for_write!(wallet_lock, LOCK_ORDER_WALLET);

                let (inputs, outputs) = wallet.generate_slips(720_000);
                tx.inputs = inputs;
                tx.outputs = outputs;
                // _i prevents sig from being identical during test
                // and thus from being auto-rejected from mempool
                tx.timestamp = ts + 120000 + _i;
                tx.generate(&public_key, 0, 0);
                tx.sign(&private_key);
            }
            let (wallet, _wallet_) = lock_for_read!(wallet_lock, LOCK_ORDER_WALLET);
            tx.add_hop(&wallet.private_key, &wallet.public_key, &[1; 33]);
            tx.generate(&public_key, 0, 0);
            mempool.add_transaction(tx).await;
        }

        assert_eq!(mempool.transactions.len(), 5);
        assert_eq!(mempool.get_routing_work_available(), 0);

        // TODO : FIX THIS TEST
        // assert_eq!(
        //     mempool.can_bundle_block(blockchain_lock.clone(), ts).await,
        //     false
        // );

        assert!(mempool
            .can_bundle_block(&blockchain, ts + 120000, &None)
            .await
            .is_some());
    }
}
