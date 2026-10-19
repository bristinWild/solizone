//! Durable storage behind a running node: canonical blocks plus the
//! execution checkpoint, behind one object-safe interface so the node can run
//! on local files (development) or Logos Storage (HybridBlockStore +
//! LogosStorageCheckpointBackend).

use std::fmt::Display;

use revm::primitives::B256;

use crate::{
    block::SolizoneBlock,
    block_store::BlockStore,
    state::{CheckpointBackend, SolizoneCheckpoint},
};

pub trait NodeStorage: Send {
    /// Persists a produced block and then the checkpoint taken after it.
    fn persist(
        &mut self,
        block: SolizoneBlock,
        checkpoint: &SolizoneCheckpoint,
    ) -> Result<(), String>;

    fn load_checkpoint(&self) -> Result<Option<SolizoneCheckpoint>, String>;

    fn block_by_height(&self, height: u64) -> Result<Option<SolizoneBlock>, String>;

    fn block_by_hash(&self, hash: B256) -> Result<Option<SolizoneBlock>, String>;
}

pub struct ChainStorage<B, C> {
    blocks: B,
    checkpoints: C,
}

impl<B, C> ChainStorage<B, C> {
    pub fn new(blocks: B, checkpoints: C) -> Self {
        Self {
            blocks,
            checkpoints,
        }
    }
}

impl<B, C> NodeStorage for ChainStorage<B, C>
where
    B: BlockStore + Send,
    B::Error: Display,
    C: CheckpointBackend + Send,
    C::Error: Display,
{
    fn persist(
        &mut self,
        block: SolizoneBlock,
        checkpoint: &SolizoneCheckpoint,
    ) -> Result<(), String> {
        let height = block.header.height;

        self.blocks
            .insert(block)
            .map_err(|error| format!("failed to persist block #{height}: {error}"))?;

        self.checkpoints
            .save(checkpoint)
            .map_err(|error| format!("failed to persist checkpoint after block #{height}: {error}"))
    }

    fn load_checkpoint(&self) -> Result<Option<SolizoneCheckpoint>, String> {
        self.checkpoints.load().map_err(|error| error.to_string())
    }

    fn block_by_height(&self, height: u64) -> Result<Option<SolizoneBlock>, String> {
        self.blocks
            .get_by_height(height)
            .map_err(|error| error.to_string())
    }

    fn block_by_hash(&self, hash: B256) -> Result<Option<SolizoneBlock>, String> {
        self.blocks
            .get_by_hash(hash)
            .map_err(|error| error.to_string())
    }
}

/// Purely in-memory storage for tests and throwaway dev nodes.
#[derive(Default)]
pub struct MemoryNodeStorage {
    blocks: crate::memory_block_store::MemoryBlockStore,
    checkpoint: Option<SolizoneCheckpoint>,
}

impl NodeStorage for MemoryNodeStorage {
    fn persist(
        &mut self,
        block: SolizoneBlock,
        checkpoint: &SolizoneCheckpoint,
    ) -> Result<(), String> {
        let _ = self.blocks.insert(block);
        self.checkpoint = Some(checkpoint.clone());
        Ok(())
    }

    fn load_checkpoint(&self) -> Result<Option<SolizoneCheckpoint>, String> {
        Ok(self.checkpoint.clone())
    }

    fn block_by_height(&self, height: u64) -> Result<Option<SolizoneBlock>, String> {
        Ok(self.blocks.get_by_height(height).unwrap_or(None))
    }

    fn block_by_hash(&self, hash: B256) -> Result<Option<SolizoneBlock>, String> {
        Ok(self.blocks.get_by_hash(hash).unwrap_or(None))
    }
}
