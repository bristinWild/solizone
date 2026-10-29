//! Long-running Solizone node with an Ethereum JSON-RPC endpoint.
//!
//! ```text
//! cargo run --bin solizone_node -- [--storage logos|local] [--rpc 127.0.0.1:8545]
//!                                  [--block-time 2] [--retention 64] [--data-dir .]
//!                                  [--publish [--logos-node URL] [--channel-id-file F]
//!                                   [--sequencer-key-file F] [--funding-address HEX]]
//!                                  [--sync-from-logos]
//! cargo run --bin solizone_node -- reset [--storage logos|local] [--data-dir .]
//! ```
//!
//! `--storage logos` (default) keeps recent blocks locally, archives older
//! blocks and every checkpoint to Logos Storage through `logosctl`.
//! `--storage local` keeps everything on local disk: no Basecamp needed,
//! which is what local development and Zoner use.
//!
//! `--publish` also publishes every produced block to Logos as a channel
//! inscription, through the Logos node's HTTP API (see the README).
//!
//! `--sync-from-logos` rebuilds the chain from the channel's finalized
//! inscriptions into an empty data directory, verifying every block by
//! replaying it, then starts the node on top of it.

use std::{env, net::SocketAddr, path::PathBuf, time::Duration};

use solizone_evm::{
    node::{NodeConfig, NodeCore, NodeHandle, NodeStorage, dev_accounts::DEV_ACCOUNTS},
    publication::{
        FeederConfig, PublicationState, PublicationStore,
        logos::{self, LogosPublicationConfig},
    },
    rpc,
    service::{self, NodePaths, Start, StorageMode, dev_genesis, open_storage, reset},
    sync::sync_from_logos,
};

/// Same credential locations and funding wallet as the `publish` harness.
const DEFAULT_CHANNEL_ID_FILE: &str =
    "../experiments/experiment-003-canonical-block/.secrets/channel_id";
const DEFAULT_SEQUENCER_KEY_FILE: &str =
    "../experiments/experiment-003-canonical-block/.secrets/sequencer_key";
/// Public Logos wallet address (not a key) configured in the Basecamp node.
const DEFAULT_FUNDING_ADDRESS: &str =
    "ae5a6c8a01b95f2cc40f596882b3f7b03e02b6a37d4376bc43bda47369ccbe2a";

struct Options {
    reset: bool,
    storage: StorageMode,
    rpc: SocketAddr,
    block_time: Duration,
    retention: usize,
    data_dir: PathBuf,
    publish: bool,
    logos_node: String,
    channel_id_file: PathBuf,
    sequencer_key_file: PathBuf,
    funding_address: String,
    sync_from_logos: bool,
}

fn usage() -> String {
    "usage: solizone_node [reset] [--storage logos|local] [--rpc ADDR] \
     [--block-time SECONDS] [--retention BLOCKS] [--data-dir DIR] \
     [--publish] [--logos-node URL] [--channel-id-file FILE] \
     [--sequencer-key-file FILE] [--funding-address HEX] [--sync-from-logos]"
        .to_string()
}

fn parse_options() -> Result<Options, String> {
    let mut options = Options {
        reset: false,
        storage: StorageMode::Logos,
        rpc: "127.0.0.1:8545".parse().expect("valid default address"),
        block_time: Duration::from_secs(2),
        retention: 64,
        data_dir: env::current_dir().map_err(|error| error.to_string())?,
        publish: false,
        logos_node: "http://127.0.0.1:8080".to_string(),
        channel_id_file: PathBuf::from(DEFAULT_CHANNEL_ID_FILE),
        sequencer_key_file: PathBuf::from(DEFAULT_SEQUENCER_KEY_FILE),
        funding_address: DEFAULT_FUNDING_ADDRESS.to_string(),
        sync_from_logos: false,
    };

    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .ok_or_else(|| format!("{name} needs a value\n{}", usage()))
        };

        match arg.as_str() {
            "reset" => options.reset = true,
            "--storage" => {
                options.storage = match value("--storage")?.as_str() {
                    "logos" => StorageMode::Logos,
                    "local" => StorageMode::Local,
                    other => return Err(format!("unknown storage mode {other:?}\n{}", usage())),
                }
            }
            "--rpc" => {
                options.rpc = value("--rpc")?
                    .parse()
                    .map_err(|error| format!("invalid --rpc address: {error}"))?
            }
            "--block-time" => {
                let seconds: f64 = value("--block-time")?
                    .parse()
                    .map_err(|error| format!("invalid --block-time: {error}"))?;
                if !seconds.is_finite() || seconds <= 0.0 {
                    return Err("--block-time must be positive".to_string());
                }
                options.block_time = Duration::from_secs_f64(seconds);
            }
            "--retention" => {
                options.retention = value("--retention")?
                    .parse()
                    .map_err(|error| format!("invalid --retention: {error}"))?
            }
            "--data-dir" => options.data_dir = PathBuf::from(value("--data-dir")?),
            "--publish" => options.publish = true,
            "--logos-node" => options.logos_node = value("--logos-node")?,
            "--channel-id-file" => {
                options.channel_id_file = PathBuf::from(value("--channel-id-file")?)
            }
            "--sequencer-key-file" => {
                options.sequencer_key_file = PathBuf::from(value("--sequencer-key-file")?)
            }
            "--funding-address" => options.funding_address = value("--funding-address")?,
            "--sync-from-logos" => options.sync_from_logos = true,
            "-h" | "--help" => return Err(usage()),
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
    }

    Ok(options)
}

/// Rebuilds an empty data directory from the channel's finalized history.
fn sync_storage(
    runtime: &tokio::runtime::Runtime,
    options: &Options,
    paths: &NodePaths,
    config: &NodeConfig,
    storage: &mut dyn NodeStorage,
) -> Result<(), String> {
    let channel_id = logos::read_hex_32(&options.channel_id_file)?;

    println!(
        "Syncing from Logos node {} (finalized blocks only; this can take a few minutes)",
        options.logos_node
    );

    let report = runtime.block_on(sync_from_logos(
        storage,
        config,
        &dev_genesis(),
        &options.logos_node,
        channel_id,
    ))?;

    println!(
        "Channel: {} inscription(s), {} Solizone block(s) verified, {} other, {} malformed, \
         {} duplicate",
        report.inscriptions, report.imported, report.foreign, report.malformed, report.duplicates
    );

    let Some((height, hash)) = report.head else {
        return Err(
            "no Solizone blocks found on the channel (blocks become readable about 12 hours \
             after they are published, once Logos finalizes them)"
                .to_string(),
        );
    };

    if report.buffered > 0 {
        eprintln!(
            "warning: the channel is missing block #{}; {} later block(s) were not imported",
            report.next_height, report.buffered
        );
    }

    println!("Imported chain up to block #{height} ({hash})");

    // The imported blocks are already on Logos: publication must not start
    // over from block #0.
    PublicationStore::new(paths.publication()).save(&PublicationState {
        next_height: height + 1,
        ..PublicationState::default()
    })?;

    Ok(())
}

/// Recovers from the latest checkpoint, or starts a fresh dev chain, and
/// reports how it started.
fn bootstrap(config: NodeConfig, storage: &mut dyn NodeStorage) -> Result<NodeCore, String> {
    let booted = service::bootstrap(config, storage)?;

    match booted.start {
        Start::Recovered {
            head_height,
            head_hash,
            restored_blocks,
            missing_receipts,
        } => {
            println!("Recovered node at block #{head_height} ({head_hash})");
            println!("Restored transaction index for {restored_blocks} block(s)");

            if missing_receipts > 0 {
                eprintln!(
                    "warning: {missing_receipts} block(s) have no stored receipts (produced \
                     before receipts were persisted); their transactions will not resolve over \
                     JSON-RPC. Run `solizone_node reset` to start a fresh chain."
                );
            }
        }
        Start::Fresh => {
            println!("Started fresh chain; genesis block #0 persisted");
            println!();
            println!("Dev accounts (10,000 ETH each; PUBLIC test keys, local use only):");

            for (index, account) in DEV_ACCOUNTS.iter().enumerate() {
                println!("  ({index}) {}  {}", account.address, account.private_key);
            }
        }
    }

    Ok(booted.core)
}

/// Shows warnings (and `RUST_LOG` overrides) from the Logos Zone SDK, which
/// otherwise stays silent when it cannot reach or authenticate with the node.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));

    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn main() -> Result<(), String> {
    init_logging();

    let options = parse_options()?;

    let paths = NodePaths::new(options.data_dir.clone(), options.storage);

    if options.reset {
        reset(&paths)?;
        println!("Solizone node state reset ({:?} storage)", options.storage);
        return Ok(());
    }

    let config = NodeConfig::default();
    let chain_id = config.chain_id;

    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;

    let mut storage = open_storage(&paths, options.retention)?;

    if options.sync_from_logos {
        sync_storage(&runtime, &options, &paths, &config, storage.as_mut())?;
    }

    let core = bootstrap(config, storage.as_mut())?;
    let head = core.head_height().unwrap_or(0);

    let node = NodeHandle::new(core, storage);

    let publication_config = if options.publish {
        Some(LogosPublicationConfig {
            node_url: options.logos_node.clone(),
            channel_id: logos::read_hex_32(&options.channel_id_file)?,
            sequencer_key: logos::read_hex_32(&options.sequencer_key_file)?,
            funding_address: logos::parse_hex_32(&options.funding_address)
                .map_err(|error| format!("invalid --funding-address: {error}"))?,
            max_tx_fee: 1_000_000,
            feeder: FeederConfig::default(),
        })
    } else {
        None
    };

    let publication_store = PublicationStore::new(paths.publication());

    runtime.block_on(async move {
        let publication = match publication_config {
            Some(config) => Some(logos::spawn(config, node.clone(), publication_store)?),
            None => None,
        };

        let (address, server) =
            rpc::start_server_with_publication(node.clone(), options.rpc, publication.clone())
                .await?;

        println!();
        println!("Solizone node running");
        println!("  chain id:   {chain_id}");
        println!("  head block: #{head}");
        println!(
            "  storage:    {:?} ({})",
            options.storage,
            options.data_dir.display()
        );
        println!(
            "  block time: {:?} (blocks are produced only when transactions are pending)",
            options.block_time
        );
        println!("  JSON-RPC:   http://{address}");
        if options.publish {
            println!("  publishing: Logos node {}", options.logos_node);
        } else {
            println!("  publishing: off (start with --publish to publish blocks to Logos)");
        }
        println!();

        node.spawn_block_producer(options.block_time);

        tokio::signal::ctrl_c()
            .await
            .map_err(|error| format!("failed to listen for Ctrl+C: {error}"))?;

        println!("\nShutting down");
        let _ = server.stop();

        Ok::<(), String>(())
    })
}
