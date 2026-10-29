//! Shared node handle and the block-production loop.
//!
//! Two independent locks: `core` (state, mempool, indexes) and `storage`
//! (durable blocks + checkpoints). They are never held together, so a slow
//! Logos Storage upload never blocks JSON-RPC reads or transaction
//! submission.

use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use revm::primitives::B256;

use crate::{
    block::SolizoneBlock,
    node::{core::NodeCore, storage::NodeStorage},
};

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[derive(Clone)]
pub struct NodeHandle {
    core: Arc<Mutex<NodeCore>>,
    storage: Arc<Mutex<Box<dyn NodeStorage>>>,
}

impl NodeHandle {
    pub fn new(core: NodeCore, storage: Box<dyn NodeStorage>) -> Self {
        Self {
            core: Arc::new(Mutex::new(core)),
            storage: Arc::new(Mutex::new(storage)),
        }
    }

    pub fn core(&self) -> MutexGuard<'_, NodeCore> {
        self.core
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn storage(&self) -> MutexGuard<'_, Box<dyn NodeStorage>> {
        self.storage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Block by height: recent blocks from memory, older ones from storage.
    pub fn block_by_height(&self, height: u64) -> Result<Option<SolizoneBlock>, String> {
        if let Some(block) = self.core().cached_block_by_height(height).cloned() {
            return Ok(Some(block));
        }

        self.storage().block_by_height(height)
    }

    pub fn block_by_hash(&self, hash: B256) -> Result<Option<SolizoneBlock>, String> {
        if let Some(block) = self.core().cached_block_by_hash(&hash).cloned() {
            return Ok(Some(block));
        }

        self.storage().block_by_hash(hash)
    }

    /// Produces one block if transactions are pending, then persists it.
    /// Returns the produced height. On a persistence failure the node is
    /// halted so in-memory state cannot drift further from durable state.
    pub fn produce_and_persist(&self, now: u64) -> Option<u64> {
        let commit = self.core().produce_block(now)?;

        let height = commit.block.header.height;
        let hash = commit.block.hash();
        let tx_count = commit.block.transactions.len();

        let result = self
            .storage()
            .persist(commit.block, &commit.receipts, &commit.checkpoint);

        match result {
            Ok(()) => {
                println!(
                    "block #{height} {hash} txs={tx_count} rejected={}",
                    commit.rejected
                );
            }
            Err(error) => {
                eprintln!("persistence failed, halting block production: {error}");
                self.core().halt(error);
            }
        }

        Some(height)
    }

    /// Applies a block produced by another node (a follower), then persists
    /// it. Calls must be serialised by the caller: blocks are applied and
    /// stored in the order given.
    pub fn apply_and_persist(&self, block: &SolizoneBlock, now: u64) -> Result<(), String> {
        let commit = self
            .core()
            .apply_block(block, now)
            .map_err(|error| error.to_string())?;

        let height = commit.block.header.height;

        self.storage()
            .persist(commit.block, &commit.receipts, &commit.checkpoint)
            .map_err(|error| {
                self.core().halt(error.clone());
                format!("persistence failed at block #{height}: {error}")
            })
    }

    /// Runs block production every `block_time` on a background thread.
    pub fn spawn_block_producer(&self, block_time: Duration) -> thread::JoinHandle<()> {
        self.spawn_stoppable_block_producer(block_time, Arc::new(AtomicBool::new(false)))
    }

    /// Like [`NodeHandle::spawn_block_producer`], but the thread also ends
    /// when `stop` is set (checked at least every 100 ms), so an embedding
    /// host can shut the node down cleanly.
    pub fn spawn_stoppable_block_producer(
        &self,
        block_time: Duration,
        stop: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        let handle = self.clone();

        thread::spawn(move || {
            let tick = Duration::from_millis(100).min(block_time);

            'run: loop {
                let mut waited = Duration::ZERO;

                while waited < block_time {
                    if stop.load(Ordering::Relaxed) {
                        break 'run;
                    }

                    thread::sleep(tick);
                    waited += tick;
                }

                handle.produce_and_persist(unix_now());

                if handle.core().halted().is_some() {
                    break;
                }
            }
        })
    }
}
