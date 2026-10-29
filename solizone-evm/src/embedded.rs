//! Running a Solizone node inside another program.
//!
//! [`EmbeddedNode`] is the whole node (execution, mempool, block production,
//! storage and optional Logos publication) behind a small synchronous API
//! that a host such as a Basecamp module can call from any thread:
//!
//! * [`EmbeddedNode::request`] takes one JSON-RPC request as text and returns
//!   the JSON-RPC response as text. It is the same set of methods the HTTP
//!   server serves (`eth_*`, `net_*`, `web3_*`, `solizone_*`), with no socket.
//! * [`EmbeddedNode::stop`] ends block production and publication.
//!
//! The node owns a Tokio runtime and a block-production thread, so the host
//! needs neither. An HTTP endpoint is optional (for Foundry, Zoner or a
//! wallet): set [`EmbeddedConfig::rpc`].

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use jsonrpsee::{RpcModule, server::ServerHandle};

use crate::rpc::EthRpc;

use crate::{
    node::{NodeConfig, NodeHandle},
    publication::PublicationHandle,
    rpc,
    service::{self, NodePaths, Start, StorageMode},
};

#[cfg(feature = "logos")]
use crate::publication::{
    FeederConfig, PublicationStore,
    logos::{self, LogosPublicationConfig},
};

/// Largest JSON-RPC response the in-process path will produce.
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Where and how to publish blocks to Logos.
#[cfg(feature = "logos")]
#[derive(Debug, Clone)]
pub struct PublishSettings {
    /// The Logos node's HTTP API, e.g. `http://127.0.0.1:8080`.
    pub logos_node: String,
    /// File holding the channel id (64 hex characters).
    pub channel_id_file: PathBuf,
    /// File holding the sequencer's Ed25519 key (64 hex characters).
    pub sequencer_key_file: PathBuf,
    /// Public address of the Logos wallet that pays inscription fees.
    pub funding_address: String,
}

#[derive(Debug, Clone)]
pub struct EmbeddedConfig {
    /// Directory the node keeps its data in.
    pub data_dir: PathBuf,
    /// The chain's id (EIP-155). Solizone's canonical dev chain is 9001; a
    /// fork or a collaboration room uses its own so transactions signed for
    /// one chain are never valid on another.
    pub chain_id: u64,
    /// A follower never produces blocks: it applies the host's, verified
    /// block by block (see [`EmbeddedNode::apply_block`]).
    pub follower: bool,
    pub storage: StorageMode,
    /// Recent blocks kept on disk before archiving (Logos storage mode).
    pub retention: usize,
    pub block_time: Duration,
    /// Also serve JSON-RPC over HTTP at this address.
    pub rpc: Option<SocketAddr>,
    /// Publish produced blocks to Logos.
    #[cfg(feature = "logos")]
    pub publish: Option<PublishSettings>,
}

impl EmbeddedConfig {
    /// A local-disk node in `data_dir`: 2 s blocks, no HTTP, no publishing.
    pub fn local(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            chain_id: NodeConfig::default().chain_id,
            follower: false,
            storage: StorageMode::Local,
            retention: 64,
            block_time: Duration::from_secs(2),
            rpc: None,
            #[cfg(feature = "logos")]
            publish: None,
        }
    }
}

/// What a running node can say about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    pub chain_id: u64,
    /// True when this start created a new chain.
    pub fresh: bool,
    /// Address of the HTTP endpoint, when one was requested.
    pub rpc_address: Option<SocketAddr>,
    pub publishing: bool,
}

pub struct EmbeddedNode {
    // Dropped last: stops the publisher tasks and the HTTP server.
    runtime: Option<tokio::runtime::Runtime>,
    node: NodeHandle,
    module: RpcModule<EthRpc>,
    info: NodeInfo,
    stop: Arc<AtomicBool>,
    producer: Option<JoinHandle<()>>,
    server: Option<ServerHandle>,
    _publication: Option<PublicationHandle>,
    publication_path: PathBuf,
    /// A publisher started (and stoppable) after the node began running.
    #[cfg(feature = "logos")]
    late_publisher: Mutex<Option<logos::Publisher>>,
}

impl EmbeddedNode {
    /// Opens (or creates) the chain in `config.data_dir` and starts running.
    pub fn start(config: EmbeddedConfig) -> Result<Self, String> {
        let paths = NodePaths::new(&config.data_dir, config.storage);

        let node_config = NodeConfig {
            chain_id: config.chain_id,
            ..NodeConfig::default()
        };
        let chain_id = node_config.chain_id;

        let mut storage = service::open_storage(&paths, config.retention)?;
        let booted = service::bootstrap_as(node_config, storage.as_mut(), config.follower)?;

        let fresh = matches!(booted.start, Start::Fresh);

        let node = NodeHandle::new(booted.core, storage);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("failed to start runtime: {error}"))?;

        #[cfg(feature = "logos")]
        let publication: Option<PublicationHandle> = {
            let publication_config = config
                .publish
                .as_ref()
                .map(|settings| -> Result<LogosPublicationConfig, String> {
                    Ok(LogosPublicationConfig {
                        node_url: settings.logos_node.clone(),
                        channel_id: logos::read_hex_32(&settings.channel_id_file)?,
                        sequencer_key: logos::read_hex_32(&settings.sequencer_key_file)?,
                        funding_address: logos::parse_hex_32(&settings.funding_address)
                            .map_err(|error| format!("invalid funding address: {error}"))?,
                        max_tx_fee: 1_000_000,
                        feeder: FeederConfig::default(),
                    })
                })
                .transpose()?;

            // The publisher spawns background tasks, so it needs a runtime context.
            match publication_config {
                Some(publication_config) => {
                    let _guard = runtime.enter();

                    Some(logos::spawn(
                        publication_config,
                        node.clone(),
                        PublicationStore::new(paths.publication()),
                    )?)
                }
                None => None,
            }
        };

        #[cfg(not(feature = "logos"))]
        let publication: Option<PublicationHandle> = None;

        let module = rpc::build_module(node.clone(), publication.clone())?;

        let (rpc_address, server) = match config.rpc {
            Some(address) => {
                let (bound, handle) = runtime.block_on(rpc::start_server_with_publication(
                    node.clone(),
                    address,
                    publication.clone(),
                ))?;

                (Some(bound), Some(handle))
            }
            None => (None, None),
        };

        let stop = Arc::new(AtomicBool::new(false));
        let producer = (!config.follower)
            .then(|| node.spawn_stoppable_block_producer(config.block_time, stop.clone()));

        Ok(Self {
            runtime: Some(runtime),
            node,
            module,
            info: NodeInfo {
                chain_id,
                fresh,
                rpc_address,
                publishing: publication.is_some(),
            },
            stop,
            producer,
            server,
            _publication: publication,
            publication_path: paths.publication(),
            #[cfg(feature = "logos")]
            late_publisher: Mutex::new(None),
        })
    }

    /// Starts publishing this chain's blocks through `adapter` (the HTTP
    /// client or the in-process Basecamp blockchain module). Replaces a
    /// publisher started earlier; progress is kept on disk, so stopping and
    /// starting again resumes where it left off. Only a block-producing node
    /// can publish.
    #[cfg(feature = "logos")]
    pub fn start_publishing<N>(
        &self,
        settings: logos::PublisherSettings,
        adapter: N,
    ) -> Result<(), String>
    where
        N: logos::LogosNode + Clone + Send + Sync + 'static,
    {
        let runtime = self.runtime.as_ref().ok_or("node is stopped")?;

        let mut slot = self
            .late_publisher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Stop the old one first: two writers must never share the store.
        drop(slot.take());

        let _guard = runtime.enter();

        *slot = Some(logos::spawn_with(
            settings,
            adapter,
            self.node.clone(),
            PublicationStore::new(self.publication_path.clone()),
        )?);

        Ok(())
    }

    /// Stops publishing. Progress stays on disk.
    #[cfg(feature = "logos")]
    pub fn stop_publishing(&self) {
        drop(
            self.late_publisher
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take(),
        );
    }

    /// Progress of the publisher started with [`Self::start_publishing`].
    #[cfg(feature = "logos")]
    pub fn publication_state(&self) -> Option<crate::publication::PublicationState> {
        self.late_publisher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|publisher| publisher.handle().snapshot())
    }

    /// Publication progress saved on disk, whether or not publishing is
    /// running now.
    pub fn saved_publication_state(
        &self,
    ) -> Result<crate::publication::PublicationState, String> {
        crate::publication::PublicationStore::new(self.publication_path.clone()).load()
    }

    pub fn info(&self) -> &NodeInfo {
        &self.info
    }

    /// Height of the newest block, if any.
    pub fn head_height(&self) -> Option<u64> {
        self.node.core().head_height()
    }

    /// Height the next block must have: the chain length, 0 for a follower
    /// still waiting for the host's genesis block.
    pub fn next_height(&self) -> u64 {
        self.node.core().next_height()
    }

    /// The encoded block at `height` (what a host sends to followers).
    pub fn block_bytes(&self, height: u64) -> Result<Option<Vec<u8>>, String> {
        Ok(self
            .node
            .block_by_height(height)?
            .map(|block| block.encode()))
    }

    /// Verifies and applies an encoded block from the host. The block must
    /// be exactly the next one; it is executed and checked against its own
    /// hash, so a forged or out-of-order block is refused. Returns its
    /// height. Not meant to be called concurrently.
    pub fn apply_block(&self, bytes: &[u8]) -> Result<u64, String> {
        let block = crate::block::SolizoneBlock::decode_and_validate(bytes)
            .map_err(|error| format!("invalid block: {error:?}"))?;

        self.node
            .apply_and_persist(&block, crate::node::runtime::unix_now())?;

        Ok(block.header.height)
    }

    /// Why block production stopped, if it did (for example a storage error).
    pub fn halted(&self) -> Option<String> {
        self.node.core().halted().map(str::to_string)
    }

    /// Handles one JSON-RPC request given as text and returns the response
    /// as text. A malformed request gets a JSON-RPC error response, so the
    /// result is always valid JSON.
    pub fn request(&self, request: &str) -> String {
        let Some(runtime) = &self.runtime else {
            return error_response("node is stopped");
        };

        match runtime.block_on(self.module.raw_json_request(request, MAX_RESPONSE_BYTES)) {
            Ok((response, _subscription)) => response.get().to_string(),
            Err(error) => error_response(&format!("invalid JSON-RPC request: {error}")),
        }
    }

    /// Stops block production, the HTTP server and publishing, and waits for
    /// the block-production thread. Data already written stays on disk.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        if let Some(server) = self.server.take() {
            let _ = server.stop();
        }

        if let Some(producer) = self.producer.take() {
            let _ = producer.join();
        }

        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

impl Drop for EmbeddedNode {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn error_response(message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": serde_json::Value::Null,
        "error": { "code": -32600, "message": message },
    })
    .to_string()
}
