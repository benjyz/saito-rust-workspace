use std::io::Error;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::common::run_task::RunTask;
use crate::core::data::blockchain::Blockchain;
use crate::core::data::configuration::Configuration;
use crate::core::data::mempool::Mempool;
use crate::core::data::wallet::Wallet;

#[derive(Clone)]
pub struct Context {
    pub blockchain: Arc<RwLock<Blockchain>>,
    pub mempool: Arc<RwLock<Mempool>>,
    pub wallet: Arc<RwLock<Wallet>>,
    pub configuration: Arc<RwLock<Box<dyn Configuration + Send + Sync>>>,
}

impl Context {
    pub fn new(configs: Arc<RwLock<Box<dyn Configuration + Send + Sync>>>) -> Context {
        let wallet = Wallet::new();
        let public_key = wallet.public_key.clone();
        let private_key = wallet.private_key.clone();
        let wallet = Arc::new(RwLock::new(wallet));
        Context {
            blockchain: Arc::new(RwLock::new(Blockchain::new(
                wallet.clone(),
                // global_sender.clone(),
            ))),
            mempool: Arc::new(RwLock::new(Mempool::new(public_key, private_key))),
            wallet: wallet.clone(),
            configuration: configs,
        }
    }
    pub async fn init(&self, _task_runner: &dyn RunTask) -> Result<(), Error> {
        Ok(())
    }
}
