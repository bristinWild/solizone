//! Long-running Solizone node with an Ethereum JSON-RPC endpoint.
//!
//! ```text
//! cargo run --bin solizone_node -- [--storage logos|local] [--rpc 127.0.0.1:8545]
//!                                  [--block-time 2] [--retention 64] [--data-dir .]
//! cargo run --bin solizone_node -- reset [--storage logos|local] [--data-dir .]
//! ```
//!
//! `--storage logos` (default) keeps recent blocks locally, archives older
//! blocks and every checkpoint to Logos Storage through `logosctl`.
//! `--storage local` keeps everything on local disk: no Basecamp needed,
//! which is what local development and Zoner use.

use std::{env, fs, net::SocketAddr, path::PathBuf, time::Duration};

use revm::primitives::{Address, U256};

use solizone_evm::{
    file_block_store::FileBlockStore,
    hybrid_block_store::HybridBlockStore,
    node::{
        ChainStorage, FileReceiptStore, NodeConfig, NodeCore, NodeHandle, NodeStorage,
        dev_accounts::{DEV_ACCOUNTS, dev_account_balance},
        runtime::unix_now,
    },
    rpc,
    state::{FileCheckpointBackend, LogosStorageCheckpointBackend},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageMode {
    Logos,
    Local,
}

struct Options {
    reset: bool,
    storage: StorageMode,
    rpc: SocketAddr,
    block_time: Duration,
    retention: usize,
    data_dir: PathBuf,
}

fn usage() -> String {
    "usage: solizone_node [reset] [--storage logos|local] [--rpc ADDR] \
     [--block-time SECONDS] [--retention BLOCKS] [--data-dir DIR]"
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
            "-h" | "--help" => return Err(usage()),
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
    }

    Ok(options)
}

/// On-disk layout. Logos mode keeps the original `solizone-node-*` paths so
/// existing node data keeps working; local mode lives in its own directory.
struct NodePaths {
    root: PathBuf,
    mode: StorageMode,
}

impl NodePaths {
    fn logos(&self, name: &str) -> PathBuf {
        self.root.join(format!("solizone-node-{name}"))
    }

    fn local_root(&self) -> PathBuf {
        self.root.join("solizone-node-local")
    }

    fn owned_paths(&self) -> Vec<PathBuf> {
        match self.mode {
            StorageMode::Logos => [
                "blocks",
                "archive-index.json",
                "recovery",
                "archive-work",
                "checkpoint-work",
                "latest-checkpoint.cid",
                "receipts",
            ]
            .iter()
            .map(|name| self.logos(name))
            .collect(),
            StorageMode::Local => vec![self.local_root()],
        }
    }
}

fn open_storage(paths: &NodePaths, retention: usize) -> Result<Box<dyn NodeStorage>, String> {
    match paths.mode {
        StorageMode::Logos => {
            for name in ["blocks", "recovery", "archive-work", "checkpoint-work"] {
                fs::create_dir_all(paths.logos(name)).map_err(|error| error.to_string())?;
            }

            Ok(Box::new(ChainStorage::new(
                HybridBlockStore::new(
                    paths.logos("blocks"),
                    paths.logos("archive-index.json"),
                    paths.logos("recovery"),
                    paths.logos("archive-work"),
                    retention,
                ),
                LogosStorageCheckpointBackend::new(
                    paths.logos("checkpoint-work"),
                    paths.logos("latest-checkpoint.cid"),
                ),
                // Receipts are derived data kept on local disk in both modes.
                FileReceiptStore::new(paths.logos("receipts")),
            )))
        }
        StorageMode::Local => {
            let root = paths.local_root();
            fs::create_dir_all(root.join("blocks")).map_err(|error| error.to_string())?;

            Ok(Box::new(ChainStorage::new(
                FileBlockStore::new(root.join("blocks")),
                FileCheckpointBackend::new(root.join("checkpoint.json")),
                FileReceiptStore::new(root.join("receipts")),
            )))
        }
    }
}

fn reset(paths: &NodePaths) -> Result<(), String> {
    for path in paths.owned_paths() {
        if path.is_dir() {
            fs::remove_dir_all(&path)
        } else if path.exists() {
            fs::remove_file(&path)
        } else {
            Ok(())
        }
        .map_err(|error| format!("failed to remove {}: {error}", path.display()))?;
    }

    Ok(())
}

/// Recovers from the latest checkpoint, or starts a fresh dev chain.
fn bootstrap(config: NodeConfig, storage: &mut dyn NodeStorage) -> Result<NodeCore, String> {
    if let Some(checkpoint) = storage.load_checkpoint()? {
        let head_height = checkpoint
            .next_height
            .checked_sub(1)
            .ok_or("checkpoint has no produced blocks")?;

        let head = storage
            .block_by_height(head_height)?
            .ok_or_else(|| format!("chain-head block #{head_height} missing from storage"))?;

        if head.hash() != checkpoint.parent_hash {
            return Err("checkpoint parent hash does not match stored chain head".to_string());
        }

        println!("Recovered node at block #{head_height} ({})", head.hash());

        let mut core = NodeCore::resume(config, checkpoint, Some(head));

        let mut records = Vec::new();
        let mut missing = 0u64;

        for height in 0..=head_height {
            match storage.block_receipts(height)? {
                Some(record) => records.push(record),
                None => missing += 1,
            }
        }

        let restored = core.restore_index(records);

        println!("Restored transaction index for {restored} block(s)");

        if missing > 0 {
            eprintln!(
                "warning: {missing} block(s) have no stored receipts (produced before receipts \
                 were persisted); their transactions will not resolve over JSON-RPC. \
                 Run `solizone_node reset` to start a fresh chain."
            );
        }

        return Ok(core);
    }

    let genesis: Vec<(Address, U256)> = DEV_ACCOUNTS
        .iter()
        .map(|account| (account.address, dev_account_balance()))
        .collect();

    let (core, genesis_commit) = NodeCore::new(config, &genesis, unix_now());

    storage.persist(
        genesis_commit.block,
        &genesis_commit.receipts,
        &genesis_commit.checkpoint,
    )?;

    println!("Started fresh chain; genesis block #0 persisted");
    println!();
    println!("Dev accounts (10,000 ETH each; PUBLIC test keys, local use only):");

    for (index, account) in DEV_ACCOUNTS.iter().enumerate() {
        println!("  ({index}) {}  {}", account.address, account.private_key);
    }

    Ok(core)
}

fn main() -> Result<(), String> {
    let options = parse_options()?;

    let paths = NodePaths {
        root: options.data_dir.clone(),
        mode: options.storage,
    };

    if options.reset {
        reset(&paths)?;
        println!("Solizone node state reset ({:?} storage)", options.storage);
        return Ok(());
    }

    let config = NodeConfig::default();
    let chain_id = config.chain_id;

    let mut storage = open_storage(&paths, options.retention)?;
    let core = bootstrap(config, storage.as_mut())?;
    let head = core.head_height().unwrap_or(0);

    let node = NodeHandle::new(core, storage);

    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;

    runtime.block_on(async move {
        let (address, server) = rpc::start_server(node.clone(), options.rpc).await?;

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
