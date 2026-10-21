//! The node running inside another program: no sockets, JSON-RPC as text.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use revm::primitives::{Bytes, TxKind, U256};
use serde_json::{Value, json};
use solizone_evm::{
    embedded::{EmbeddedConfig, EmbeddedNode},
    node::dev_accounts::DEV_ACCOUNTS,
};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    std::env::temp_dir().join(format!(
        "solizone-embedded-{name}-{}-{nanos}",
        std::process::id()
    ))
}

fn config(dir: &PathBuf) -> EmbeddedConfig {
    EmbeddedConfig {
        block_time: Duration::from_millis(50),
        ..EmbeddedConfig::local(dir)
    }
}

fn deploy_counter(nonce: u64) -> String {
    let signer: PrivateKeySigner = DEV_ACCOUNTS[0].private_key.parse().unwrap();

    let hex = include_str!("../contracts/Counter.creation.hex").trim();
    let init = Bytes::from(hex::decode(hex.trim_start_matches("0x")).unwrap());

    let tx = TxEip1559 {
        chain_id: 9001,
        nonce,
        gas_limit: 1_000_000,
        max_fee_per_gas: 0,
        max_priority_fee_per_gas: 0,
        to: TxKind::Create,
        value: U256::ZERO,
        access_list: Default::default(),
        input: init,
    };

    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    let envelope: TxEnvelope = tx.into_signed(signature).into();

    format!("0x{}", hex::encode(envelope.encoded_2718()))
}

fn call(node: &EmbeddedNode, method: &str, params: Value) -> Value {
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    serde_json::from_str(&node.request(&request.to_string())).unwrap()
}

fn wait_for_receipt(node: &EmbeddedNode, hash: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        let response = call(node, "eth_getTransactionReceipt", json!([hash]));

        if !response["result"].is_null() {
            return response["result"].clone();
        }

        assert!(Instant::now() < deadline, "no receipt for {hash}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn deploys_a_contract_through_in_process_json_rpc() {
    let dir = temp_dir("deploy");
    let node = EmbeddedNode::start(config(&dir)).unwrap();

    assert!(node.info().fresh);
    assert_eq!(node.info().chain_id, 9001);
    assert_eq!(node.info().rpc_address, None);
    assert!(!node.info().publishing);

    assert_eq!(call(&node, "eth_chainId", json!([]))["result"], "0x2329");
    assert_eq!(node.head_height(), Some(0));

    let sent = call(&node, "eth_sendRawTransaction", json!([deploy_counter(0)]));
    let hash = sent["result"]
        .as_str()
        .expect("transaction hash")
        .to_string();

    let receipt = wait_for_receipt(&node, &hash);

    assert_eq!(receipt["status"], "0x1");
    assert!(receipt["contractAddress"].is_string());
    assert_eq!(node.head_height(), Some(1));
    assert!(node.halted().is_none());

    node.stop();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_chain_survives_a_restart() {
    let dir = temp_dir("restart");

    let first = EmbeddedNode::start(config(&dir)).unwrap();
    let sent = call(&first, "eth_sendRawTransaction", json!([deploy_counter(0)]));
    let hash = sent["result"].as_str().unwrap().to_string();
    let receipt = wait_for_receipt(&first, &hash);
    let address = receipt["contractAddress"].clone();
    first.stop();

    let second = EmbeddedNode::start(config(&dir)).unwrap();

    assert!(!second.info().fresh);
    assert_eq!(second.head_height(), Some(1));

    let code = call(&second, "eth_getCode", json!([address, "latest"]));
    assert_ne!(code["result"], "0x");

    // The transaction index survived too.
    let again = call(&second, "eth_getTransactionReceipt", json!([hash]));
    assert_eq!(again["result"]["contractAddress"], address);

    second.stop();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn bad_requests_get_json_rpc_errors_not_panics() {
    let dir = temp_dir("bad");
    let node = EmbeddedNode::start(config(&dir)).unwrap();

    let garbage: Value = serde_json::from_str(&node.request("this is not json")).unwrap();
    assert!(garbage["error"].is_object());

    let unknown = call(&node, "eth_noSuchMethod", json!([]));
    assert_eq!(unknown["error"]["code"], -32601);

    let bad_tx = call(&node, "eth_sendRawTransaction", json!(["0x1234"]));
    assert!(bad_tx["error"].is_object());

    node.stop();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn can_also_serve_http_for_foundry_and_wallets() {
    let dir = temp_dir("http");

    let node = EmbeddedNode::start(EmbeddedConfig {
        rpc: Some("127.0.0.1:0".parse().unwrap()),
        ..config(&dir)
    })
    .unwrap();

    let address = node.info().rpc_address.expect("bound address");
    assert_ne!(address.port(), 0);

    // The same chain is visible in-process and over HTTP.
    assert_eq!(call(&node, "eth_blockNumber", json!([]))["result"], "0x0");

    node.stop();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn stopping_ends_block_production_quickly() {
    let dir = temp_dir("stop");
    let node = EmbeddedNode::start(EmbeddedConfig {
        block_time: Duration::from_secs(30),
        ..config(&dir)
    })
    .unwrap();

    let started = Instant::now();
    node.stop();

    assert!(started.elapsed() < Duration::from_secs(5));

    std::fs::remove_dir_all(&dir).unwrap();
}
