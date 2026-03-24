//! xrpl-node CLI — run the XRPL validator node.
//!
//! Usage:
//!   xrpl-node                    # Run with default config
//!   xrpl-node --config path.toml # Run with custom config
//!   xrpl-node --testnet          # Connect to testnet
//!   xrpl-node --generate-identity # Generate and print a new node keypair

use xrpl_node::config::NodeConfig;
use xrpl_node::node::{init_tracing, Node};
use xrpl_node::peer::identity::NodeIdentity;
use xrpl_node::rpc;

#[derive(clap::Parser)]
#[command(name = "xrpl-node", about = "XRPL Validator Node in Rust")]
struct Cli {
    /// Path to TOML config file
    #[arg(long, default_value = "~/.xrpl-node/config.toml")]
    config: String,

    /// Connect to testnet instead of mainnet
    #[arg(long)]
    testnet: bool,

    /// Generate a new node identity and exit
    #[arg(long)]
    generate_identity: bool,

    /// RPC port override
    #[arg(long)]
    rpc_port: Option<u16>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use clap::Parser;
    let cli = Cli::parse();

    // Generate identity mode
    if cli.generate_identity {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let identity = NodeIdentity::generate()?;
        println!("Node Public Key: {}", identity.public_key_hex());
        println!("Key Type: Secp256k1 (compressed)");
        println!("\nAdd this to your UNL or share with peers.");
        return Ok(());
    }

    // Load config
    let mut config = if let Ok(toml_str) = std::fs::read_to_string(
        cli.config.replace('~', &std::env::var("HOME").unwrap_or_default()),
    ) {
        toml::from_str::<NodeConfig>(&toml_str)?
    } else {
        eprintln!("No config file found, using defaults");
        NodeConfig::default()
    };

    // CLI overrides
    if cli.testnet {
        config.seed_peers = vec!["s.altnet.rippletest.net:51235".into()];
        config.network_id = 1;
    }
    if let Some(port) = cli.rpc_port {
        config.rpc_port = port;
    }

    // Init tracing
    init_tracing(&config);

    // Init crypto provider
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Create and start node
    let node = Node::new(config.clone())?;
    node.start().await;

    // Start RPC server
    let rpc_state = node.rpc_state();
    let rpc_router = rpc::create_router(rpc_state);
    let rpc_addr = format!("127.0.0.1:{}", config.rpc_port);

    tracing::info!(addr = %rpc_addr, "RPC server starting");
    let listener = tokio::net::TcpListener::bind(&rpc_addr).await?;

    // Run until ctrl-c
    tokio::select! {
        result = axum::serve(listener, rpc_router) => {
            if let Err(e) = result {
                tracing::error!("RPC server error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received ctrl-c, shutting down");
        }
    }

    node.shutdown();
    Ok(())
}
