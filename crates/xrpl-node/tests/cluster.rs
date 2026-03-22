//! Test: verify Node can be constructed with various configurations.
//!
//! A full multi-node consensus cluster test requires running multiple
//! peer listeners and connecting them — this validates the Node
//! orchestrator wiring and configuration parsing.

use xrpl_node::config::{NodeConfig, NodeMode};
use xrpl_node::node::Node;

fn init_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[test]
fn create_observer_node() {
    init_crypto();
    let config = NodeConfig {
        mode: NodeMode::Observer,
        ..NodeConfig::default()
    };
    let node = Node::new(config).expect("should create observer node");
    assert_eq!(*node.mode(), NodeMode::Observer);
    assert!(node.consensus.is_none());
    assert_eq!(node.mempool.len(), 0);
}

#[test]
fn create_follower_node() {
    init_crypto();
    let config = NodeConfig {
        mode: NodeMode::Follower,
        ..NodeConfig::default()
    };
    let node = Node::new(config).expect("should create follower node");
    assert_eq!(*node.mode(), NodeMode::Follower);
    assert!(node.consensus.is_none());
}

#[test]
fn create_validator_node() {
    init_crypto();
    let config = NodeConfig {
        mode: NodeMode::Validator,
        unl_keys: vec!["AABBCCDD".into()],
        ..NodeConfig::default()
    };
    let node = Node::new(config).expect("should create validator node");
    assert_eq!(*node.mode(), NodeMode::Validator);
    assert!(node.consensus.is_some()); // validators get consensus engine
    assert_eq!(node.unl.read().trusted_count(), 1);
}

#[test]
fn node_has_unique_identity() {
    init_crypto();
    let n1 = Node::new(NodeConfig::default()).unwrap();
    let n2 = Node::new(NodeConfig::default()).unwrap();
    assert_ne!(n1.identity.public_key_hex(), n2.identity.public_key_hex());
}

#[test]
fn rpc_state_created() {
    init_crypto();
    let node = Node::new(NodeConfig::default()).unwrap();
    let state = node.rpc_state();
    assert_eq!(state.mempool.len(), 0);
}
