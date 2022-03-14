use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use log::{debug, info};

use crate::common::defs::{PrivateKey, PublicKey};
use crate::common::run_task::RunTask;
use crate::core::data::block::Block;
use crate::core::data::transaction::Transaction;
use crate::core::wallet::Wallet;

pub struct Mempool {
    blocks_queue: VecDeque<Block>,
    pub transactions: Vec<Transaction>,
    // vector so we just copy it over
    routing_work_in_mempool: u64,
    wallet: Arc<RwLock<Wallet>>,
    currently_bundling_block: bool,
    public_key: PublicKey,
    private_key: PrivateKey,
}

impl Mempool {
    pub fn new(wallet: Arc<RwLock<Wallet>>) -> Mempool {
        Mempool {
            blocks_queue: Default::default(),
            transactions: vec![],
            routing_work_in_mempool: 0,
            wallet,
            currently_bundling_block: false,
            public_key: [0; 33],
            private_key: [0; 32],
        }
    }
    pub fn init(&mut self, task_runner: &dyn RunTask) {
        debug!("mempool.init");

        debug!("main thread id = {:?}", std::thread::current().id());
        task_runner.run(Box::pin(move || {
            let mut last_time = Instant::now();
            let mut counter = 0;
            debug!("new thread id = {:?}", std::thread::current().id());
            loop {
                let current_time = Instant::now();
                let duration = current_time.duration_since(last_time);

                if duration.as_micros() > 1_000_000 {
                    info!("counter : {:?}", counter);
                    last_time = current_time;
                    counter = counter + 1;
                }
                if counter < 5 {
                    continue;
                }
                info!("block created");

                counter = 0;
            }
        }));
    }

    pub fn add_block(&mut self, block: Block) {
        todo!()
    }

    pub fn on_timer(&mut self, duration: Duration) {}
}
