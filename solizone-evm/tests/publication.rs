//! Publication pipeline against a real node and RPC server, with a fake sink
//! standing in for Logos.

use std::sync::{Arc, Mutex};

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_signer::SignerSync;
use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};
use revm::primitives::{Address, Bytes, TxKind, U256, address};
use serde_json::Value;
use solizone_evm::{
    node::{
        MemoryNodeStorage, NodeConfig, NodeCore, NodeHandle, NodeStorage,
        dev_accounts::{DEV_ACCOUNTS, dev_account_balance},
    },
    publication::{
        InscriptionSink, PublicationHandle, PublicationState, PublicationStatus, PublicationStore,
        SharedState, SubmitError, Submitted,
        feeder::{PumpOutcome, check_cursor, pump},
    },
    rpc,
};

const NOW: u64 = 1_800_000_000;

struct RecordingSink {
    payloads: Mutex<Vec<Vec<u8>>>,
}

impl InscriptionSink for RecordingSink {
    async fn submit(&self, payload: Vec<u8>) -> Result<Submitted, SubmitError> {
        let mut payloads = self.payloads.lock().unwrap();
        payloads.push(payload);

        Ok(Submitted {
            tx_hash: format!("0xtx{}", payloads.len() - 1),
            checkpoint: None,
        })
    }
}

fn transfer(nonce: u64) -> Vec<u8> {
    let tx = TxEip1559 {
        chain_id: 9001,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: 0,
        max_priority_fee_per_gas: 0,
        to: TxKind::Call(address!("2222222222222222222222222222222222222222")),
        value: U256::from(1),
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let signer: alloy_signer_local::PrivateKeySigner = DEV_ACCOUNTS[0].private_key.parse().unwrap();
    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    let envelope: TxEnvelope = tx.into_signed(signature).into();
    envelope.encoded_2718()
}

fn new_node() -> NodeHandle {
    let genesis: Vec<(Address, U256)> = DEV_ACCOUNTS
        .iter()
        .map(|account| (account.address, dev_account_balance()))
        .collect();

    let (core, commit) = NodeCore::new(NodeConfig::default(), &genesis, NOW);

    let mut storage = MemoryNodeStorage::default();
    storage
        .persist(commit.block, &commit.receipts, &commit.checkpoint)
        .unwrap();

    NodeHandle::new(core, Box::new(storage))
}

#[tokio::test(flavor = "multi_thread")]
async fn node_blocks_flow_to_the_sink_and_show_in_rpc_status() {
    let node = new_node();

    for nonce in 0..3 {
        node.core()
            .submit_raw_transaction(&transfer(nonce), NOW + nonce)
            .unwrap();
        node.produce_and_persist(NOW + nonce).unwrap();
    }

    let directory = std::env::temp_dir().join(format!("solizone-pub-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let store = PublicationStore::new(directory.join("publication.json"));

    let state: SharedState = Arc::new(Mutex::new(PublicationState::default()));
    let handle = PublicationHandle::new(state.clone());

    check_cursor(&state.lock().unwrap(), &node).unwrap();

    let sink = RecordingSink {
        payloads: Mutex::new(Vec::new()),
    };

    // Genesis plus three produced blocks, limited to two in flight at a time.
    let first = pump(&sink, &node, &state, &store, 2).await;
    assert_eq!(first.submitted, 2);
    assert_eq!(first.outcome, PumpOutcome::Backpressure);

    let (address, server) = rpc::start_server_with_publication(
        node.clone(),
        "127.0.0.1:0".parse().unwrap(),
        Some(handle),
    )
    .await
    .unwrap();

    let client = HttpClientBuilder::default()
        .build(format!("http://{address}"))
        .unwrap();

    let status: Value = client
        .request("solizone_publicationStatus", rpc_params![])
        .await
        .unwrap();

    assert_eq!(status["enabled"], true);
    assert_eq!(status["headHeight"], 3);
    assert_eq!(status["nextHeight"], 2);
    assert_eq!(status["unsubmitted"], 2);
    assert_eq!(status["inFlight"], 2);
    assert_eq!(status["recent"][0]["height"], 1);
    assert_eq!(status["recent"][0]["status"], "submitted");

    // Logos finalizes both; the rest goes out.
    for tx in ["0xtx0", "0xtx1"] {
        state
            .lock()
            .unwrap()
            .set_status(tx, PublicationStatus::Finalized);
    }

    let second = pump(&sink, &node, &state, &store, 2).await;
    assert_eq!(second.submitted, 2);

    // Every payload is exactly the node's encoded block, in height order.
    let payloads = sink.payloads.lock().unwrap().clone();
    assert_eq!(payloads.len(), 4);

    for (height, payload) in payloads.iter().enumerate() {
        let block = node.block_by_height(height as u64).unwrap().unwrap();
        assert_eq!(payload, &block.encode());
    }

    let status: Value = client
        .request("solizone_publicationStatus", rpc_params![])
        .await
        .unwrap();
    assert_eq!(status["nextHeight"], 4);
    assert_eq!(status["unsubmitted"], 0);

    server.stop().unwrap();
    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_disabled_without_publication() {
    let node = new_node();

    let (address, server) = rpc::start_server(node, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();

    let client = HttpClientBuilder::default()
        .build(format!("http://{address}"))
        .unwrap();

    let status: Value = client
        .request("solizone_publicationStatus", rpc_params![])
        .await
        .unwrap();

    assert_eq!(status["enabled"], false);
    assert_eq!(status["headHeight"], 0);
    assert_eq!(status["recent"], serde_json::json!([]));

    server.stop().unwrap();
}
