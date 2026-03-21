use std::path::PathBuf;

/// Operating mode for the node.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeMode {
    /// Connect to peers, log messages — no state storage.
    Observer,
    /// Sync ledger state, serve RPC queries — no consensus.
    #[default]
    Follower,
    /// Full consensus participant — propose, validate, vote.
    Validator,
}

/// Configuration for the XRPL validator node.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct NodeConfig {
    /// Operating mode.
    #[serde(default)]
    pub mode: NodeMode,

    /// Port for peer-to-peer connections (default: 51235).
    #[serde(default = "default_peer_port")]
    pub listen_port: u16,

    /// Port for JSON-RPC server (default: 51234).
    #[serde(default = "default_rpc_port")]
    pub rpc_port: u16,

    /// Directory for persistent data (sled database, keys).
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    /// Log level filter (e.g., "info", "debug", "peer=trace,consensus=debug").
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Seed peers to connect to on startup.
    #[serde(default)]
    pub seed_peers: Vec<String>,

    /// Maximum outbound peer connections.
    #[serde(default = "default_max_peers")]
    pub max_peers: usize,

    /// Trusted validator public keys (hex-encoded, 33 bytes each).
    #[serde(default)]
    pub unl_keys: Vec<String>,

    /// Validator token (base64). If present, node operates as validator.
    #[serde(default)]
    pub validator_token: Option<String>,

    /// Fee voting preferences.
    #[serde(default)]
    pub fee_config: FeeConfig,
}

/// Fee and reserve voting preferences.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct FeeConfig {
    /// Base transaction cost in drops (default: 10).
    #[serde(default = "default_reference_fee")]
    pub reference_fee: u64,

    /// Base account reserve in drops (default: 10 XRP).
    #[serde(default = "default_account_reserve")]
    pub account_reserve: u64,

    /// Per-object owner reserve in drops (default: 2 XRP).
    #[serde(default = "default_owner_reserve")]
    pub owner_reserve: u64,
}

impl Default for FeeConfig {
    fn default() -> Self {
        Self {
            reference_fee: default_reference_fee(),
            account_reserve: default_account_reserve(),
            owner_reserve: default_owner_reserve(),
        }
    }
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            mode: NodeMode::default(),
            listen_port: default_peer_port(),
            rpc_port: default_rpc_port(),
            data_dir: default_data_dir(),
            log_level: default_log_level(),
            seed_peers: Vec::new(),
            max_peers: default_max_peers(),
            unl_keys: Vec::new(),
            validator_token: None,
            fee_config: FeeConfig::default(),
        }
    }
}

fn default_peer_port() -> u16 {
    51235
}
fn default_rpc_port() -> u16 {
    51234
}
fn default_data_dir() -> PathBuf {
    dirs_next().unwrap_or_else(|| PathBuf::from(".xrpl-node"))
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_max_peers() -> usize {
    21
}
fn default_reference_fee() -> u64 {
    10
}
fn default_account_reserve() -> u64 {
    10_000_000
}
fn default_owner_reserve() -> u64 {
    2_000_000
}

fn dirs_next() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".xrpl-node"))
}
