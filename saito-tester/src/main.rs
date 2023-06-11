use std::collections::VecDeque;
use std::panic;
use std::process;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::info;
use log::{debug, error, trace};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing_subscriber;
use tracing_subscriber::filter::Directive;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use saito_core::common::command::NetworkEvent;
use saito_core::common::defs::{push_lock, StatVariable, LOCK_ORDER_CONFIGS, STAT_BIN_COUNT};
use saito_core::common::keep_time::KeepTime;
use saito_core::common::process_event::ProcessEvent;
use saito_core::core::consensus_thread::{ConsensusEvent, ConsensusStats, ConsensusThread};
use saito_core::core::data::blockchain::Blockchain;
use saito_core::core::data::blockchain_sync_state::BlockchainSyncState;
use saito_core::core::data::configuration::Configuration;
use saito_core::core::data::context::Context;
use saito_core::core::data::crypto::generate_keys;
use saito_core::core::data::network::Network;
use saito_core::core::data::peer_collection::PeerCollection;
use saito_core::core::data::storage::Storage;
use saito_core::core::data::wallet::Wallet;
use saito_core::core::mining_thread::{MiningEvent, MiningThread};
use saito_core::core::routing_thread::{
    PeerState, RoutingEvent, RoutingStats, RoutingThread, StaticPeer,
};
use saito_core::core::verification_thread::{VerificationThread, VerifyRequest};
use saito_core::lock_for_read;
use saito_rust::saito::config_handler::ConfigHandler;
use saito_rust::saito::io_event::IoEvent;
use saito_rust::saito::network_controller::run_network_controller;
use saito_rust::saito::rust_io_handler::RustIOHandler;
use saito_rust::saito::stat_thread::StatThread;
use saito_rust::saito::time_keeper::TimeKeeper;
use saito::config_handler::SpammerConfigs;

const ROUTING_EVENT_PROCESSOR_ID: u8 = 1;
const CONSENSUS_EVENT_PROCESSOR_ID: u8 = 2;
const MINING_EVENT_PROCESSOR_ID: u8 = 3;

mod saito;
mod thread_util;
mod thread_run;

use thread_util::run_thread;
use thread_util::run_loop_thread;
use thread_run::run_consensus_event_processor;
use thread_run::run_mining_event_processor;
use thread_run::run_routing_event_processor;
use thread_run::run_verification_thread;
use thread_run::run_verification_threads;

fn setup_ctrl_c_handler() -> Result<(), Box<dyn std::error::Error>> {
    ctrlc::set_handler(move || {
        info!("shutting down the node");
        process::exit(0);
    })
    .map_err(|e| e.into())
}

fn setup_panic_hook() {
    let orig_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        if let Some(location) = panic_info.location() {
            error!(
                "panic occurred in file '{}' at line {}, exiting ..",
                location.file(),
                location.line()
            );
        } else {
            error!("panic occurred but can't get location information, exiting ..");
        }

        // invoke the default handler and exit the process
        orig_hook(panic_info);
        process::exit(99);
    }));
}

fn setup_logging_environment() {
    let mut filter = tracing_subscriber::EnvFilter::from_default_env();
    let directives = vec![
        "tokio_tungstenite=info",
        "tungstenite=info",
        "mio::poll=info",
        "hyper::proto=info",
        "hyper::client=info",
        "want=info",
        "reqwest::async_impl=info",
        "reqwest::connect=info",
        "warp::filters=info",
        // "saito_stats=info",
    ];

    for directive in directives {
        filter = filter.add_directive(Directive::from_str(directive).unwrap());
    }

    let fmt_layer = tracing_subscriber::fmt::Layer::default().with_filter(filter);

    tracing_subscriber::registry().with(fmt_layer).init();
    
}

async fn load_configs() -> Arc<RwLock<dyn Configuration + Send + Sync>> {
    Arc::new(RwLock::new(
        ConfigHandler::load_configs("configs/config.json".to_string())
            .expect("loading configs failed"),
    ))
}


#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {

    // --------------
    // SETUP
    // --------------

    setup_ctrl_c_handler()?;
    setup_panic_hook();
    setup_logging_environment();

    println!("Running saito");

    let configs = load_configs().await;

    let (channel_size, thread_sleep_time_in_ms, stat_timer_in_ms, verification_thread_count, fetch_batch_size) = {
        let (configs, _configs_) = lock_for_read!(configs, LOCK_ORDER_CONFIGS);
        let server_configs = configs.get_server_configs().unwrap();
    
        (
            server_configs.channel_size as usize,
            server_configs.thread_sleep_time_in_ms,
            server_configs.stat_timer_in_ms,
            server_configs.verification_threads,
            server_configs.block_fetch_batch_size as usize,
        )
    };
    

    let (event_sender_to_loop, event_receiver_in_loop) =
        tokio::sync::mpsc::channel::<IoEvent>(channel_size);

    let (sender_to_network_controller, receiver_in_network_controller) =
        tokio::sync::mpsc::channel::<IoEvent>(channel_size);

    info!("running saito controllers");

    let keys = generate_keys();
    let wallet = Arc::new(RwLock::new(Wallet::new(keys.1, keys.0)));
    {
        let mut wallet = wallet.write().await;
        Wallet::load(
            &mut wallet,
            Box::new(RustIOHandler::new(
                sender_to_network_controller.clone(),
                ROUTING_EVENT_PROCESSOR_ID,
            )),
        )
        .await;
    }
    let context = Context::new(configs.clone(), wallet);

    let peers = Arc::new(RwLock::new(PeerCollection::new()));

    let (sender_to_consensus, receiver_for_consensus) =
        tokio::sync::mpsc::channel::<ConsensusEvent>(channel_size);

    let (sender_to_routing, receiver_for_routing) =
        tokio::sync::mpsc::channel::<RoutingEvent>(channel_size);

    let (sender_to_miner, receiver_for_miner) =
        tokio::sync::mpsc::channel::<MiningEvent>(channel_size);
    let (sender_to_stat, receiver_for_stat) = tokio::sync::mpsc::channel::<String>(channel_size);

    //TODO simplify
    let (senders, verification_handles) = run_verification_threads(
        sender_to_consensus.clone(),
        context.blockchain.clone(),
        peers.clone(),
        context.wallet.clone(),
        stat_timer_in_ms,
        thread_sleep_time_in_ms,
        verification_thread_count,
        sender_to_stat.clone(),
    )
    .await;

    let (network_event_sender_to_routing, routing_handle) = run_routing_event_processor(
        sender_to_network_controller.clone(),
        configs.clone(),
        &context,
        peers.clone(),
        &sender_to_consensus,
        receiver_for_routing,
        &sender_to_miner,
        senders,
        stat_timer_in_ms,
        thread_sleep_time_in_ms,
        channel_size,
        sender_to_stat.clone(),
        fetch_batch_size,
    )
    .await;

    let (_network_event_sender_to_consensus, blockchain_handle) = run_consensus_event_processor(
        &context,
        peers.clone(),
        receiver_for_consensus,
        &sender_to_routing,
        sender_to_miner,
        sender_to_network_controller.clone(),
        stat_timer_in_ms,
        thread_sleep_time_in_ms,
        channel_size,
        sender_to_stat.clone(),
    )
    .await;

    // let (_network_event_sender_to_mining, miner_handle) = run_mining_event_processor(
    //     &context,
    //     &sender_to_consensus,
    //     receiver_for_miner,
    //     stat_timer_in_ms,
    //     thread_sleep_time_in_ms,
    //     channel_size,
    //     sender_to_stat.clone(),
    // )
    // .await;
    
    let loop_handle = run_loop_thread(
        event_receiver_in_loop,
        network_event_sender_to_routing,
        stat_timer_in_ms,
        thread_sleep_time_in_ms,
        sender_to_stat.clone(),
    );

    let network_handle = tokio::spawn(run_network_controller(
        receiver_in_network_controller,
        event_sender_to_loop.clone(),
        configs.clone(),
        context.blockchain.clone(),
        sender_to_stat.clone(),
        peers.clone(),
    ));

    let _result = tokio::join!(
        routing_handle,
        //blockchain_handle,
        //miner_handle,
        loop_handle,
        network_handle,
        //stat_handle,
        futures::future::join_all(verification_handles)
    );
    Ok(())
}
