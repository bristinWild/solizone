//! Setting up a node: where its data lives, opening storage, recovering from
//! a checkpoint or starting a fresh dev chain.
//!
//! Shared by the `solizone_node` binary and by hosts that embed the node
//! (see [`crate::embedded`]).

use std::{fs, path::PathBuf};

use revm::primitives::{Address, B256, U256};

use crate::{
    file_block_store::FileBlockStore,
    hybrid_block_store::HybridBlockStore,
    node::{
        ChainStorage, FileReceiptStore, NodeConfig, NodeCore, NodeStorage,
        dev_accounts::{DEV_ACCOUNTS, dev_account_balance},
        runtime::unix_now,
    },
    state::{FileCheckpointBackend, LogosStorageCheckpointBackend},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMode {
    /// Recent blocks on local disk, older blocks and checkpoints archived to
    /// Logos Storage through `logosctl`.
    Logos,
    /// Everything on local disk. No Basecamp needed.
    Local,
}

/// On-disk layout. Logos mode keeps the original `solizone-node-*` paths so
/// existing node data keeps working; local mode lives in its own directory.
#[derive(Debug, Clone)]
pub struct NodePaths {
    pub root: PathBuf,
    pub mode: StorageMode,
}

impl NodePaths {
    pub fn new(root: impl Into<PathBuf>, mode: StorageMode) -> Self {
        Self {
            root: root.into(),
            mode,
        }
    }

    fn logos(&self, name: &str) -> PathBuf {
        self.root.join(format!("solizone-node-{name}"))
    }

    fn local_root(&self) -> PathBuf {
        self.root.join("solizone-node-local")
    }

    /// Publication cursor and Logos transaction status.
    pub fn publication(&self) -> PathBuf {
        match self.mode {
            StorageMode::Logos => self.logos("publication.json"),
            StorageMode::Local => self.local_root().join("publication.json"),
        }
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
                "publication.json",
            ]
            .iter()
            .map(|name| self.logos(name))
            .collect(),
            StorageMode::Local => vec![self.local_root()],
        }
    }
}

pub fn open_storage(paths: &NodePaths, retention: usize) -> Result<Box<dyn NodeStorage>, String> {
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

/// Deletes the node's own data (and nothing else) under the data directory.
pub fn reset(paths: &NodePaths) -> Result<(), String> {
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

/// Genesis balances of every Solizone dev chain. A chain read back from Logos
/// is replayed from exactly this state.
pub fn dev_genesis() -> Vec<(Address, U256)> {
    DEV_ACCOUNTS
        .iter()
        .map(|account| (account.address, dev_account_balance()))
        .collect()
}

/// How a node came up.
#[derive(Debug)]
pub enum Start {
    /// Genesis block #0 was created and persisted.
    Fresh,
    /// Recovered from the latest checkpoint.
    Recovered {
        head_height: u64,
        head_hash: B256,
        /// Blocks whose transaction index was restored.
        restored_blocks: usize,
        /// Blocks with no stored receipts (produced before receipts were
        /// persisted); their transactions will not resolve over JSON-RPC.
        missing_receipts: u64,
    },
}

pub struct Bootstrapped {
    pub core: NodeCore,
    pub start: Start,
}

/// Recovers from the latest checkpoint, or starts a fresh dev chain.
pub fn bootstrap(
    config: NodeConfig,
    storage: &mut dyn NodeStorage,
) -> Result<Bootstrapped, String> {
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

        let head_hash = head.hash();

        let mut core = NodeCore::resume(config, checkpoint, Some(head));

        let mut records = Vec::new();
        let mut missing_receipts = 0u64;

        for height in 0..=head_height {
            match storage.block_receipts(height)? {
                Some(record) => records.push(record),
                None => missing_receipts += 1,
            }
        }

        let restored_blocks = core.restore_index(records);

        return Ok(Bootstrapped {
            core,
            start: Start::Recovered {
                head_height,
                head_hash,
                restored_blocks,
                missing_receipts,
            },
        });
    }

    let (core, genesis_commit) = NodeCore::new(config, &dev_genesis(), unix_now());

    storage.persist(
        genesis_commit.block,
        &genesis_commit.receipts,
        &genesis_commit.checkpoint,
    )?;

    Ok(Bootstrapped {
        core,
        start: Start::Fresh,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::MemoryNodeStorage;

    #[test]
    fn a_fresh_start_persists_genesis_and_a_second_start_recovers_it() {
        let mut storage = MemoryNodeStorage::default();

        let first = bootstrap(NodeConfig::default(), &mut storage).unwrap();
        assert!(matches!(first.start, Start::Fresh));
        assert_eq!(first.core.head_height(), Some(0));

        let second = bootstrap(NodeConfig::default(), &mut storage).unwrap();

        match second.start {
            Start::Recovered {
                head_height,
                restored_blocks,
                missing_receipts,
                ..
            } => {
                assert_eq!(head_height, 0);
                assert_eq!(restored_blocks, 1);
                assert_eq!(missing_receipts, 0);
            }
            Start::Fresh => panic!("expected recovery"),
        }
    }

    #[test]
    fn reset_removes_only_the_nodes_own_data() {
        let root = std::env::temp_dir().join(format!(
            "solizone-service-test-{}-{}",
            std::process::id(),
            unix_now()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("keep.txt"), "mine").unwrap();

        let paths = NodePaths::new(&root, StorageMode::Local);
        let mut storage = open_storage(&paths, 8).unwrap();
        bootstrap(NodeConfig::default(), storage.as_mut()).unwrap();
        drop(storage);

        assert!(root.join("solizone-node-local").exists());

        reset(&paths).unwrap();

        assert!(!root.join("solizone-node-local").exists());
        assert!(root.join("keep.txt").exists());

        fs::remove_dir_all(&root).unwrap();
    }
}
