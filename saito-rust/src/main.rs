use std::panic;
use std::process;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::info;
use tracing::{debug, error, trace};
use tracing_subscriber;
use tracing_subscriber::filter::Directive;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use saito_core::common::command::NetworkEvent;
use saito_core::common::defs::{
    StatVariable, CHANNEL_SIZE, STAT_BIN_COUNT, STAT_TIMER, THREAD_SLEEP_TIME,
};

use saito_core::common::process_event::ProcessEvent;
use saito_core::core::consensus_event_processor::{ConsensusEvent, ConsensusEventProcessor};
use saito_core::core::data::configuration::Configuration;
use saito_core::core::data::context::Context;
use saito_core::core::data::network::Network;
use saito_core::core::data::peer_collection::PeerCollection;
use saito_core::core::data::storage::Storage;
use saito_core::core::mining_event_processor::{MiningEvent, MiningEventProcessor};
use saito_core::core::routing_event_processor::{
    PeerState, RoutingEvent, RoutingEventProcessor, StaticPeer,
};
use saito_core::{log_read_lock_receive, log_read_lock_request};

use crate::saito::config_handler::ConfigHandler;
use crate::saito::io_event::IoEvent;
use crate::saito::network_controller::run_network_controller;
use crate::saito::rust_io_handler::RustIOHandler;
use crate::saito::time_keeper::TimeKeeper;

mod saito;
mod test;

const ROUTING_EVENT_PROCESSOR_ID: u8 = 1;
const CONSENSUS_EVENT_PROCESSOR_ID: u8 = 2;
const MINING_EVENT_PROCESSOR_ID: u8 = 3;

async fn run_thread<T>(
    mut event_processor: Box<(dyn ProcessEvent<T> + Send + 'static)>,
    mut network_event_receiver: Receiver<NetworkEvent>,
    mut event_receiver: Receiver<T>,
) -> JoinHandle<()>
where
    T: Send + 'static,
{
    tokio::spawn(async move {
        info!("new thread started");
        let mut work_done = false;
        let mut last_timestamp = Instant::now();
        let mut stat_timer = Instant::now();

        event_processor.on_init().await;

        loop {
            work_done = false;
            let result = network_event_receiver.try_recv();
            if result.is_ok() {
                let event = result.unwrap();
                if event_processor.process_network_event(event).await.is_some() {
                    work_done = true;
                }
            }

            let result = event_receiver.try_recv();
            if result.is_ok() {
                let event = result.unwrap();
                if event_processor.process_event(event).await.is_some() {
                    work_done = true;
                }
            }

            let current_instant = Instant::now();
            let duration = current_instant.duration_since(last_timestamp);
            last_timestamp = current_instant;

            if event_processor
                .process_timer_event(duration)
                .await
                .is_some()
            {
                work_done = true;
            }

            #[cfg(feature = "with-stats")]
            {
                let duration = current_instant.duration_since(stat_timer);
                if duration > STAT_TIMER {
                    stat_timer = current_instant;
                    event_processor.on_stat_interval().await;
                }
            }

            if !work_done {
                tokio::time::sleep(THREAD_SLEEP_TIME).await;
            }
        }
    })
}

async fn run_mining_event_processor(
    context: &Context,
    sender_to_mempool: &Sender<ConsensusEvent>,
    sender_to_blockchain: &Sender<RoutingEvent>,
    receiver_for_miner: Receiver<MiningEvent>,
) -> (Sender<NetworkEvent>, JoinHandle<()>) {
    let mining_event_processor = MiningEventProcessor {
        wallet: context.wallet.clone(),
        sender_to_blockchain: sender_to_blockchain.clone(),
        sender_to_mempool: sender_to_mempool.clone(),
        time_keeper: Box::new(TimeKeeper {}),
        miner_active: false,
        target: [0; 32],
        difficulty: 0,
        public_key: [0; 33],
        mined_golden_tickets: 0,
    };
    let (interface_sender_to_miner, interface_receiver_for_miner) =
        tokio::sync::mpsc::channel::<NetworkEvent>(CHANNEL_SIZE);

    debug!("running miner thread");
    let _miner_handle = run_thread(
        Box::new(mining_event_processor),
        interface_receiver_for_miner,
        receiver_for_miner,
    )
    .await;
    (interface_sender_to_miner, _miner_handle)
}

async fn run_consensus_event_processor(
    context: &Context,
    peers: Arc<RwLock<PeerCollection>>,
    receiver_for_blockchain: Receiver<ConsensusEvent>,
    sender_to_routing: &Sender<RoutingEvent>,
    sender_to_miner: Sender<MiningEvent>,
    sender_to_network_controller: Sender<IoEvent>,
) -> (Sender<NetworkEvent>, JoinHandle<()>) {
    let result = std::env::var("GEN_TX");
    let mut create_test_tx = false;
    if result.is_ok() {
        create_test_tx = result.unwrap().eq("1");
    }
    let generate_genesis_block: bool;
    {
        let configs = context.configuration.read().await;

        // if we have peers defined in configs, there's already an existing network. so we don't need to generate the first block.
        generate_genesis_block = configs.get_peer_configs().is_empty();
    }
    let consensus_event_processor = ConsensusEventProcessor {
        mempool: context.mempool.clone(),
        blockchain: context.blockchain.clone(),
        wallet: context.wallet.clone(),
        generate_genesis_block,
        sender_to_router: sender_to_routing.clone(),
        sender_to_miner: sender_to_miner.clone(),
        // sender_global: global_sender.clone(),
        time_keeper: Box::new(TimeKeeper {}),
        network: Network::new(
            Box::new(RustIOHandler::new(
                sender_to_network_controller.clone(),
                CONSENSUS_EVENT_PROCESSOR_ID,
            )),
            peers.clone(),
            context.wallet.clone(),
        ),
        block_producing_timer: 0,
        tx_producing_timer: 0,
        create_test_tx,
        storage: Storage::new(Box::new(RustIOHandler::new(
            sender_to_network_controller.clone(),
            CONSENSUS_EVENT_PROCESSOR_ID,
        ))),
        stats: Default::default(),
    };
    let (interface_sender_to_blockchain, interface_receiver_for_mempool) =
        tokio::sync::mpsc::channel::<NetworkEvent>(CHANNEL_SIZE);
    debug!("running mempool thread");
    let blockchain_handle = run_thread(
        Box::new(consensus_event_processor),
        interface_receiver_for_mempool,
        receiver_for_blockchain,
    )
    .await;

    (interface_sender_to_blockchain, blockchain_handle)
}

async fn run_routing_event_processor(
    sender_to_io_controller: Sender<IoEvent>,
    configs: Arc<RwLock<Box<dyn Configuration + Send + Sync>>>,
    context: &Context,
    peers: Arc<RwLock<PeerCollection>>,
    sender_to_mempool: &Sender<ConsensusEvent>,
    receiver_for_routing: Receiver<RoutingEvent>,
    sender_to_miner: &Sender<MiningEvent>,
) -> (Sender<NetworkEvent>, JoinHandle<()>) {
    let mut routing_event_processor = RoutingEventProcessor {
        blockchain: context.blockchain.clone(),
        sender_to_mempool: sender_to_mempool.clone(),
        sender_to_miner: sender_to_miner.clone(),
        time_keeper: Box::new(TimeKeeper {}),
        static_peers: vec![],
        configs: configs.clone(),
        wallet: context.wallet.clone(),
        network: Network::new(
            Box::new(RustIOHandler::new(
                sender_to_io_controller.clone(),
                ROUTING_EVENT_PROCESSOR_ID,
            )),
            peers.clone(),
            context.wallet.clone(),
        ),
        reconnection_timer: 0,
        stats: Default::default(),
    };
    {
        log_read_lock_request!("configs");
        let configs = configs.read().await;
        log_read_lock_receive!("configs");
        let peers = configs.get_peer_configs();
        for peer in peers {
            routing_event_processor.static_peers.push(StaticPeer {
                peer_details: (*peer).clone(),
                peer_state: PeerState::Disconnected,
                peer_index: 0,
            });
        }
    }

    let (interface_sender_to_routing, interface_receiver_for_routing) =
        tokio::sync::mpsc::channel::<NetworkEvent>(CHANNEL_SIZE);

    debug!("running blockchain thread");
    let routing_handle = run_thread(
        Box::new(routing_event_processor),
        interface_receiver_for_routing,
        receiver_for_routing,
    )
    .await;

    (interface_sender_to_routing, routing_handle)
}

// TODO : to be moved to routing event processor
fn run_loop_thread(
    mut receiver: Receiver<IoEvent>,
    network_event_sender_to_routing_ep: Sender<NetworkEvent>,
    network_event_sender_to_consensus_ep: Sender<NetworkEvent>,
    network_event_sender_to_mining_ep: Sender<NetworkEvent>,
) -> JoinHandle<()> {
    let loop_handle = tokio::spawn(async move {
        let mut work_done: bool;
        let mut incoming_msgs =
            StatVariable::new("network::incoming_msgs".to_string(), STAT_BIN_COUNT);
        let _last_stat_on: Instant = Instant::now();
        loop {
            work_done = false;

            let result = receiver.try_recv();
            if result.is_ok() {
                let command = result.unwrap();
                incoming_msgs.increment();
                work_done = true;
                // TODO : remove hard coded values
                match command.event_processor_id {
                    ROUTING_EVENT_PROCESSOR_ID => {
                        trace!("routing event to routing event processor  ",);
                        network_event_sender_to_routing_ep
                            .send(command.event)
                            .await
                            .unwrap();
                    }
                    CONSENSUS_EVENT_PROCESSOR_ID => {
                        trace!(
                            "routing event to consensus event processor : {:?}",
                            command.event
                        );
                        network_event_sender_to_consensus_ep
                            .send(command.event)
                            .await
                            .unwrap();
                    }
                    MINING_EVENT_PROCESSOR_ID => {
                        trace!(
                            "routing event to mining event processor : {:?}",
                            command.event
                        );
                        network_event_sender_to_mining_ep
                            .send(command.event)
                            .await
                            .unwrap();
                    }

                    _ => {}
                }
            }
            #[cfg(feature = "with-stats")]
            {
                if Instant::now().duration_since(last_stat_on) > STAT_TIMER {
                    last_stat_on = Instant::now();
                    incoming_msgs.calculate_stats(TimeKeeper {}.get_timestamp());
                    incoming_msgs.print();
                }
            }
            if !work_done {
                tokio::time::sleep(THREAD_SLEEP_TIME).await;
            }
        }
    });

    loop_handle
}

#[tokio::main(flavor = "multi_thread", worker_threads = 20)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ctrlc::set_handler(move || {
        info!("shutting down the node");
        process::exit(0);
    })
    .expect("Error setting Ctrl-C handler");

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

    println!("Running saito");

    // pretty_env_logger::init();
    // let mut builder = pretty_env_logger::formatted_builder();
    // builder
    //     .format(|buf, record| {
    //         let mut style = buf.style();
    //
    //         // TODO : set colored output
    //         style.set_bold(true);
    //         writeln!(
    //             buf,
    //             "{:6} {:2?} - {:45}- {:?}",
    //             style.value(record.level()),
    //             // record.level(),
    //             std::thread::current().id(),
    //             record.module_path().unwrap_or_default(),
    //             record.args(),
    //         )
    //     })
    //     .parse_filters(&env::var("RUST_LOG").unwrap_or_default())
    //     .init();

    // install global subscriber configured based on RUST_LOG envvar.

    let filter = tracing_subscriber::EnvFilter::from_default_env();
    let filter = filter.add_directive(Directive::from_str("tokio_tungstenite=info").unwrap());
    let filter = filter.add_directive(Directive::from_str("tungstenite=info").unwrap());
    let filter = filter.add_directive(Directive::from_str("mio::poll=info").unwrap());
    let filter = filter.add_directive(Directive::from_str("hyper::proto=info").unwrap());
    let filter = filter.add_directive(Directive::from_str("hyper::client=info").unwrap());
    let filter = filter.add_directive(Directive::from_str("want=info").unwrap());
    let filter = filter.add_directive(Directive::from_str("reqwest::async_impl=info").unwrap());
    let filter = filter.add_directive(Directive::from_str("reqwest::connect=info").unwrap());
    let filter = filter.add_directive(Directive::from_str("warp::filters=info").unwrap());
    // let filter = filter.add_directive(Directive::from_str("saito_stats=info").unwrap());

    // #[cfg(feature = "with-stats")]
    // {
    //     let current_env = std::env::var("RUST_LOG").unwrap();
    //     let new_env = current_env + ",saito_stat=info";
    //     std::env::set_var("RUST_LOG", new_env);
    // }
    // use tracing_flame::FlameLayer;

    // let (flame_layer, _guard) = FlameLayer::with_file("./tracing.folded").unwrap();

    let fmt_layer = tracing_subscriber::fmt::Layer::default().with_filter(filter);

    // filter.with_filter(flame_layer);
    // tracing_subscriber::fmt::fmt()
    tracing_subscriber::registry()
        .with(fmt_layer)
        // .with_env_filter(filter)
        // .with(flame_layer)
        // .pretty()
        .init();

    let configs: Arc<RwLock<Box<dyn Configuration + Send + Sync>>> =
        Arc::new(RwLock::new(Box::new(
            ConfigHandler::load_configs("configs/saito.config.json".to_string())
                .expect("loading configs failed"),
        )));

    let (event_sender_to_loop, event_receiver_in_loop) =
        tokio::sync::mpsc::channel::<IoEvent>(CHANNEL_SIZE);

    let (sender_to_network_controller, receiver_in_network_controller) =
        tokio::sync::mpsc::channel::<IoEvent>(CHANNEL_SIZE);

    info!("running saito controllers");

    let context = Context::new(configs.clone());
    let peers = Arc::new(RwLock::new(PeerCollection::new()));

    let (sender_to_consensus, receiver_for_consensus) =
        tokio::sync::mpsc::channel::<ConsensusEvent>(CHANNEL_SIZE);

    let (sender_to_routing, receiver_for_routing) =
        tokio::sync::mpsc::channel::<RoutingEvent>(CHANNEL_SIZE);

    let (sender_to_miner, receiver_for_miner) =
        tokio::sync::mpsc::channel::<MiningEvent>(CHANNEL_SIZE);

    let (network_event_sender_to_routing, routing_handle) = run_routing_event_processor(
        sender_to_network_controller.clone(),
        configs.clone(),
        &context,
        peers.clone(),
        &sender_to_consensus,
        receiver_for_routing,
        &sender_to_miner,
    )
    .await;

    let (network_event_sender_to_consensus, blockchain_handle) = run_consensus_event_processor(
        &context,
        peers.clone(),
        receiver_for_consensus,
        &sender_to_routing,
        sender_to_miner,
        sender_to_network_controller.clone(),
    )
    .await;

    let (network_event_sender_to_mining, miner_handle) = run_mining_event_processor(
        &context,
        &sender_to_consensus,
        &sender_to_routing,
        receiver_for_miner,
    )
    .await;

    let loop_handle = run_loop_thread(
        event_receiver_in_loop,
        network_event_sender_to_routing,
        network_event_sender_to_consensus,
        network_event_sender_to_mining,
    );

    let network_handle = tokio::spawn(run_network_controller(
        receiver_in_network_controller,
        event_sender_to_loop.clone(),
        configs.clone(),
        context.blockchain.clone(),
    ));

    let _result = tokio::join!(
        routing_handle,
        blockchain_handle,
        miner_handle,
        loop_handle,
        network_handle
    );
    Ok(())
}
