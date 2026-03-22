//! Top-level Node orchestrator — ties all subsystems together.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Instant;

use tracing::info;
use tracing_subscriber::{fmt, EnvFilter};

use crate::config::{NodeConfig, NodeMode};
use crate::consensus::{ConsensusState, UNL};
use crate::mempool::{RelayFilter, TransactionQueue};
use crate::overlay::OverlayRouter;
use crate::peer::identity::NodeIdentity;
use crate::peer::manager::PeerManager;
use crate::rpc::AppState;
use crate::NodeError;

/// The XRPL validator node — coordinates all subsystems.
pub struct Node {
    pub config: Arc<NodeConfig>,
    pub identity: Arc<NodeIdentity>,
    pub peer_manager: Arc<PeerManager>,
    pub mempool: Arc<TransactionQueue>,
    pub relay_filter: Arc<RelayFilter>,
    pub unl: Arc<parking_lot::RwLock<UNL>>,
    pub consensus: Option<parking_lot::RwLock<ConsensusState>>,
    pub start_time: Instant,
    pub latest_ledger: Arc<parking_lot::RwLock<Option<u32>>>,
    pub peer_count: Arc<AtomicUsize>,
}

impl Node {
    /// Create a new node from configuration.
    pub fn new(config: NodeConfig) -> Result<Self, NodeError> {
        let config = Arc::new(config);

        let identity = Arc::new(NodeIdentity::generate()?);
        info!(pubkey = %identity.public_key_hex(), "node identity ready");

        let unl = UNL::from_keys(&config.unl_keys);
        info!(trusted = unl.trusted_count(), "UNL loaded");

        let mempool = Arc::new(TransactionQueue::new(10_000));
        let relay_filter = Arc::new(RelayFilter::new(std::time::Duration::from_secs(300)));

        let (_, inbound_tx, _channels) = OverlayRouter::new(1024);

        let peer_manager = Arc::new(PeerManager::new(
            identity.clone(),
            config.clone(),
            inbound_tx,
        ));

        let consensus = if config.mode == NodeMode::Validator {
            Some(parking_lot::RwLock::new(ConsensusState::new(
                xrpl_core::types::Hash256([0; 32]),
            )))
        } else {
            None
        };

        Ok(Self {
            config,
            identity,
            peer_manager,
            mempool,
            relay_filter,
            unl: Arc::new(parking_lot::RwLock::new(unl)),
            consensus,
            start_time: Instant::now(),
            latest_ledger: Arc::new(parking_lot::RwLock::new(None)),
            peer_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Start the node — connect to peers.
    pub async fn start(&self) {
        info!(mode = ?self.config.mode, "starting node");
        self.peer_manager.connect_to_seeds().await;
        self.peer_count.store(
            self.peer_manager.peer_count(),
            std::sync::atomic::Ordering::Relaxed,
        );
        info!(peers = self.peer_manager.peer_count(), "connected to peers");
    }

    /// Create the RPC app state.
    pub fn rpc_state(&self) -> Arc<AppState> {
        Arc::new(AppState {
            mempool: self.mempool.clone(),
            start_time: self.start_time,
            peer_count: self.peer_count.clone(),
            latest_ledger: self.latest_ledger.clone(),
        })
    }

    pub fn mode(&self) -> &NodeMode { &self.config.mode }
    pub fn uptime(&self) -> u64 { self.start_time.elapsed().as_secs() }

    pub fn shutdown(&self) {
        info!("shutting down");
        self.peer_manager.shutdown();
    }
}

/// Initialize the tracing subscriber for structured logging.
pub fn init_tracing(config: &NodeConfig) {
    let filter = EnvFilter::try_new(&config.log_level)
        .unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_names(true)
        .with_timer(fmt::time::uptime())
        .init();
}
