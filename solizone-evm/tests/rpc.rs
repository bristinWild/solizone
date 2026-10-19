//! End-to-end JSON-RPC test: real server, real signed transactions.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};
use revm::primitives::{Address, Bytes, TxKind, U256, address};
use serde_json::Value;
use solizone_evm::{
    node::{
        MemoryNodeStorage, NodeConfig, NodeCore, NodeHandle,
        dev_accounts::{DEV_ACCOUNTS, dev_account_balance},
        runtime::unix_now,
    },
    rpc,
};

const CHAIN_ID: u64 = 9001;

fn signer() -> PrivateKeySigner {
    DEV_ACCOUNTS[0].private_key.parse().unwrap()
}

fn sign(nonce: u64, to: Option<Address>, input: Bytes, gas_limit: u64) -> String {
    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce,
        gas_limit,
        max_fee_per_gas: 0,
        max_priority_fee_per_gas: 0,
        to: to.map_or(TxKind::Create, TxKind::Call),
        value: U256::ZERO,
        access_list: Default::default(),
        input,
    };
    let signature = signer().sign_hash_sync(&tx.signature_hash()).unwrap();
    let envelope: TxEnvelope = tx.into_signed(signature).into();
    format!("0x{}", hex::encode(envelope.encoded_2718()))
}

fn counter_initcode() -> Bytes {
    let hex = include_str!("../contracts/Counter.creation.hex").trim();
    Bytes::from(hex::decode(hex.trim_start_matches("0x")).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn deploy_call_and_query_over_json_rpc() {
    let genesis: Vec<(Address, U256)> = DEV_ACCOUNTS
        .iter()
        .map(|a| (a.address, dev_account_balance()))
        .collect();
    let (core, genesis_commit) = NodeCore::new(NodeConfig::default(), &genesis, unix_now());

    let mut storage = MemoryNodeStorage::default();
    use solizone_evm::node::NodeStorage;
    storage
        .persist(genesis_commit.block, &genesis_commit.checkpoint)
        .unwrap();

    let node = NodeHandle::new(core, Box::new(storage));
    let (address, server) = rpc::start_server(node.clone(), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let client = HttpClientBuilder::default()
        .build(format!("http://{address}"))
        .unwrap();

    let chain_id: String = client.request("eth_chainId", rpc_params![]).await.unwrap();
    assert_eq!(chain_id, "0x2329");

    let latest: Value = client
        .request("eth_getBlockByNumber", rpc_params!["latest", false])
        .await
        .unwrap();
    assert_eq!(latest["number"], "0x0");
    assert_eq!(latest["baseFeePerGas"], "0x0");

    // Deploy Counter.
    let deploy_hash: String = client
        .request(
            "eth_sendRawTransaction",
            rpc_params![sign(0, None, counter_initcode(), 1_000_000)],
        )
        .await
        .unwrap();

    let pending: Value = client
        .request("eth_getTransactionByHash", rpc_params![&deploy_hash])
        .await
        .unwrap();
    assert_eq!(pending["blockNumber"], Value::Null);

    let none: Value = client
        .request("eth_getTransactionReceipt", rpc_params![&deploy_hash])
        .await
        .unwrap();
    assert_eq!(none, Value::Null);

    assert_eq!(node.produce_and_persist(unix_now()), Some(1));

    let receipt: Value = client
        .request("eth_getTransactionReceipt", rpc_params![&deploy_hash])
        .await
        .unwrap();
    assert_eq!(receipt["status"], "0x1");
    assert_eq!(receipt["blockNumber"], "0x1");
    let counter: Address = receipt["contractAddress"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // increment() then read count() through eth_call.
    let increment = Bytes::from(vec![0xd0, 0x9d, 0xe0, 0x8a]);
    let _: String = client
        .request(
            "eth_sendRawTransaction",
            rpc_params![sign(1, Some(counter), increment, 100_000)],
        )
        .await
        .unwrap();
    node.produce_and_persist(unix_now());

    let count: String = client
        .request(
            "eth_call",
            rpc_params![
                serde_json::json!({"to": counter, "data": "0x06661abd"}),
                "latest"
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        U256::from_str_radix(count.trim_start_matches("0x"), 16).unwrap(),
        U256::from(1)
    );

    // Revert surfaces as code 3 with the reason.
    let revert = client
        .request::<String, _>(
            "eth_call",
            rpc_params![serde_json::json!({"to": counter, "data": "0xa9cc4718"})],
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(revert.contains("intentional failure"), "{revert}");

    // Logs and nonce.
    let logs: Vec<Value> = client
        .request(
            "eth_getLogs",
            rpc_params![serde_json::json!({"fromBlock": "0x0", "address": counter})],
        )
        .await
        .unwrap();
    assert_eq!(logs.len(), 1);

    let nonce: String = client
        .request(
            "eth_getTransactionCount",
            rpc_params![DEV_ACCOUNTS[0].address, "pending"],
        )
        .await
        .unwrap();
    assert_eq!(nonce, "0x2");

    // Wrong-chain transaction is refused.
    let wrong_chain = {
        let tx = TxEip1559 {
            chain_id: 1,
            to: TxKind::Call(address!("2222222222222222222222222222222222222222")),
            gas_limit: 21_000,
            nonce: 2,
            ..Default::default()
        };
        let signature = signer().sign_hash_sync(&tx.signature_hash()).unwrap();
        let envelope: TxEnvelope = tx.into_signed(signature).into();
        format!("0x{}", hex::encode(envelope.encoded_2718()))
    };
    let error = client
        .request::<String, _>("eth_sendRawTransaction", rpc_params![wrong_chain])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("invalid chain id"), "{error}");

    server.stop().unwrap();
}
