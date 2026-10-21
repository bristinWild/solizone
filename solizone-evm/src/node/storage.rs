//! Durable storage behind a running node: canonical blocks plus the
//! execution checkpoint, behind one object-safe interface so the node can run
//! on local files (development) or Logos Storage (HybridBlockStore +
//! LogosStorageCheckpointBackend).

use std::fmt::Display;

use revm::primitives::B256;

use crate::{
    block::SolizoneBlock,
    block_store::BlockStore,
    execution::receipt::ExecutionReceipt,
    node::receipt_store::{BlockReceipts, MemoryReceiptStore, ReceiptStore},
    state::{CheckpointBackend, SolizoneCheckpoint},
};

pub trait NodeStorage: Send {
    /// Persists a produced block: its receipts first, then the block, then
    /// the checkpoint taken after it. A crash between steps leaves the
    /// checkpoint pointing at the previous block, which is then re-produced.
    fn persist(
        &mut self,
        block: SolizoneBlock,
        receipts: &[ExecutionReceipt],
        checkpoint: &SolizoneCheckpoint,
    ) -> Result<(), String>;

    fn load_checkpoint(&self) -> Result<Option<SolizoneCheckpoint>, String>;

    /// Stored receipts of one block, `None` if none were recorded (e.g. the
    /// block predates receipt persistence).
    fn block_receipts(&self, height: u64) -> Result<Option<BlockReceipts>, String>;

    fn block_by_height(&self, height: u64) -> Result<Option<SolizoneBlock>, String>;

    fn block_by_hash(&self, hash: B256) -> Result<Option<SolizoneBlock>, String>;
}

pub struct ChainStorage<B, C, R> {
    blocks: B,
    checkpoints: C,
    receipts: R,
}

impl<B, C, R> ChainStorage<B, C, R> {
    pub fn new(blocks: B, checkpoints: C, receipts: R) -> Self {
        Self {
            blocks,
            checkpoints,
            receipts,
        }
    }
}

impl<B, C, R> NodeStorage for ChainStorage<B, C, R>
where
    B: BlockStore + Send,
    B::Error: Display,
    C: CheckpointBackend + Send,
    C::Error: Display,
    R: ReceiptStore + Send,
{
    fn persist(
        &mut self,
        block: SolizoneBlock,
        receipts: &[ExecutionReceipt],
        checkpoint: &SolizoneCheckpoint,
    ) -> Result<(), String> {
        let height = block.header.height;

        let record = BlockReceipts::from_block(&block, receipts)
            .map_err(|error| format!("block #{height}: {error}"))?;

        self.receipts.put(&record)?;

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

    fn block_receipts(&self, height: u64) -> Result<Option<BlockReceipts>, String> {
        self.receipts.get(height)
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
    receipts: MemoryReceiptStore,
    checkpoint: Option<SolizoneCheckpoint>,
}

impl NodeStorage for MemoryNodeStorage {
    fn persist(
        &mut self,
        block: SolizoneBlock,
        receipts: &[ExecutionReceipt],
        checkpoint: &SolizoneCheckpoint,
    ) -> Result<(), String> {
        let record = BlockReceipts::from_block(&block, receipts).map_err(|e| e.to_string())?;
        self.receipts.put(&record)?;
        let _ = self.blocks.insert(block);
        self.checkpoint = Some(checkpoint.clone());
        Ok(())
    }

    fn block_receipts(&self, height: u64) -> Result<Option<BlockReceipts>, String> {
        self.receipts.get(height)
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
