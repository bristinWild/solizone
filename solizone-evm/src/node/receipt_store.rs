//! Durable receipts and transaction index.
//!
//! Receipts cannot be recomputed after the fact (that would need the state of
//! every historical block), so each block's transactions and receipts are
//! stored next to the block when it is produced. A stored record is
//! self-contained: it carries the block hash, timestamp, every transaction and
//! its receipt, which is everything needed to rebuild the node's transaction
//! index on restart without reading the blocks themselves (they may already
//! be archived to Logos Storage).
//!
//! File format (`<height>.szrb`):
//!
//! ```text
//! "SZRB" | height u64 | block_hash 32 | timestamp u64 | count u32
//!        | count x ( tx_len u32 | tx canonical | receipt_len u32 | receipt canonical )
//! ```

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use revm::primitives::B256;

use crate::{
    block::SolizoneBlock,
    execution::{receipt::ExecutionReceipt, transaction::SolizoneTransaction},
};

const MAGIC: &[u8; 4] = b"SZRB";

/// Transactions and receipts of one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockReceipts {
    pub height: u64,
    pub block_hash: B256,
    pub timestamp: u64,
    pub entries: Vec<(SolizoneTransaction, ExecutionReceipt)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockReceiptsError {
    ReceiptCountMismatch {
        transactions: usize,
        receipts: usize,
    },
    Decode(String),
}

impl std::fmt::Display for BlockReceiptsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReceiptCountMismatch {
                transactions,
                receipts,
            } => write!(
                f,
                "block has {transactions} transactions but {receipts} receipts"
            ),
            Self::Decode(reason) => write!(f, "invalid receipts record: {reason}"),
        }
    }
}

impl std::error::Error for BlockReceiptsError {}

impl BlockReceipts {
    pub fn from_block(
        block: &SolizoneBlock,
        receipts: &[ExecutionReceipt],
    ) -> Result<Self, BlockReceiptsError> {
        if block.transactions.len() != receipts.len() {
            return Err(BlockReceiptsError::ReceiptCountMismatch {
                transactions: block.transactions.len(),
                receipts: receipts.len(),
            });
        }

        Ok(Self {
            height: block.header.height,
            block_hash: block.hash(),
            timestamp: block.header.timestamp,
            entries: block
                .transactions
                .iter()
                .cloned()
                .zip(receipts.iter().cloned())
                .collect(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();

        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&self.height.to_be_bytes());
        bytes.extend_from_slice(self.block_hash.as_slice());
        bytes.extend_from_slice(&self.timestamp.to_be_bytes());

        let count = u32::try_from(self.entries.len()).expect("too many transactions");
        bytes.extend_from_slice(&count.to_be_bytes());

        for (tx, receipt) in &self.entries {
            for part in [tx.encode_canonical(), receipt.encode_canonical()] {
                let len = u32::try_from(part.len()).expect("record part too large");
                bytes.extend_from_slice(&len.to_be_bytes());
                bytes.extend_from_slice(&part);
            }
        }

        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, BlockReceiptsError> {
        let mut input = bytes;

        if take(&mut input, 4)? != MAGIC {
            return Err(decode_error("bad magic"));
        }

        let height = u64::from_be_bytes(take(&mut input, 8)?.try_into().expect("8 bytes"));
        let block_hash = B256::from_slice(take(&mut input, 32)?);
        let timestamp = u64::from_be_bytes(take(&mut input, 8)?.try_into().expect("8 bytes"));
        let count = take_u32(&mut input)?;

        let mut entries = Vec::new();

        for _ in 0..count {
            let tx_len = take_u32(&mut input)?;
            let tx = SolizoneTransaction::decode_canonical(take(&mut input, tx_len)?)
                .map_err(|error| decode_error(format!("transaction: {error:?}")))?;

            let receipt_len = take_u32(&mut input)?;
            let receipt = ExecutionReceipt::decode_canonical(take(&mut input, receipt_len)?)
                .map_err(|error| decode_error(format!("receipt: {error:?}")))?;

            entries.push((tx, receipt));
        }

        if !input.is_empty() {
            return Err(decode_error("trailing bytes"));
        }

        Ok(Self {
            height,
            block_hash,
            timestamp,
            entries,
        })
    }
}

fn decode_error(reason: impl Into<String>) -> BlockReceiptsError {
    BlockReceiptsError::Decode(reason.into())
}

fn take<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8], BlockReceiptsError> {
    if input.len() < len {
        return Err(decode_error("unexpected end of data"));
    }

    let (head, tail) = input.split_at(len);
    *input = tail;

    Ok(head)
}

fn take_u32(input: &mut &[u8]) -> Result<usize, BlockReceiptsError> {
    let bytes: [u8; 4] = take(input, 4)?.try_into().expect("4 bytes");

    Ok(u32::from_be_bytes(bytes) as usize)
}

/// Where block receipts live. Implementations must make `put` durable before
/// returning (the node persists receipts first, then the block, then the
/// checkpoint).
pub trait ReceiptStore {
    fn put(&mut self, record: &BlockReceipts) -> Result<(), String>;

    fn get(&self, height: u64) -> Result<Option<BlockReceipts>, String>;
}

/// One file per block in a local directory.
pub struct FileReceiptStore {
    directory: PathBuf,
}

impl FileReceiptStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn path(&self, height: u64) -> PathBuf {
        self.directory.join(format!("{height}.szrb"))
    }
}

impl ReceiptStore for FileReceiptStore {
    fn put(&mut self, record: &BlockReceipts) -> Result<(), String> {
        let write = || -> io::Result<()> {
            fs::create_dir_all(&self.directory)?;

            // Write to a temporary file and rename so a crash never leaves a
            // half-written record under the real name.
            let path = self.path(record.height);
            let temp = path.with_extension("szrb.tmp");

            fs::write(&temp, record.encode())?;
            fs::rename(&temp, &path)
        };

        write().map_err(|error| format!("failed to write receipts for #{}: {error}", record.height))
    }

    fn get(&self, height: u64) -> Result<Option<BlockReceipts>, String> {
        let path = self.path(height);

        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
        };

        BlockReceipts::decode(&bytes)
            .map(Some)
            .map_err(|error| format!("{}: {error}", path.display()))
    }
}

/// In-memory receipts for tests and throwaway dev nodes.
#[derive(Default)]
pub struct MemoryReceiptStore {
    records: BTreeMap<u64, BlockReceipts>,
}

impl ReceiptStore for MemoryReceiptStore {
    fn put(&mut self, record: &BlockReceipts) -> Result<(), String> {
        self.records.insert(record.height, record.clone());
        Ok(())
    }

    fn get(&self, height: u64) -> Result<Option<BlockReceipts>, String> {
        Ok(self.records.get(&height).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        block::SolizoneBlockHeader,
        eth::SignedEthTransaction,
        eth::test_utils::{TxParams, dev_signer, sign_eip1559},
        execution::receipt::{ExecutionLog, ExecutionStatus},
    };
    use revm::primitives::{Bytes, address};

    fn sample(height: u64) -> BlockReceipts {
        let raw = sign_eip1559(
            &dev_signer(0),
            TxParams::transfer(
                9001,
                0,
                address!("2222222222222222222222222222222222222222"),
                5,
            ),
        );
        let tx = SignedEthTransaction::decode(&raw).unwrap().to_solizone();

        let receipt = ExecutionReceipt {
            status: ExecutionStatus::Success,
            gas_used: 21_000,
            output: Bytes::new(),
            contract_address: None,
            logs: vec![ExecutionLog {
                address: address!("3333333333333333333333333333333333333333"),
                topics: vec![B256::repeat_byte(1)],
                data: Bytes::from_static(&[4, 5, 6]),
            }],
        };

        BlockReceipts {
            height,
            block_hash: B256::repeat_byte(height as u8),
            timestamp: 1_700_000_000 + height,
            entries: vec![(tx, receipt)],
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("solizone-receipts-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        directory
    }

    #[test]
    fn record_round_trips() {
        let record = sample(3);

        assert_eq!(BlockReceipts::decode(&record.encode()).unwrap(), record);
    }

    #[test]
    fn empty_block_round_trips() {
        let record = BlockReceipts {
            height: 0,
            block_hash: B256::repeat_byte(9),
            timestamp: 1,
            entries: vec![],
        };

        assert_eq!(BlockReceipts::decode(&record.encode()).unwrap(), record);
    }

    #[test]
    fn decode_rejects_corruption() {
        let bytes = sample(1).encode();

        assert!(BlockReceipts::decode(&bytes[..bytes.len() - 3]).is_err());

        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(BlockReceipts::decode(&trailing).is_err());

        let mut bad_magic = bytes;
        bad_magic[0] = b'X';
        assert!(BlockReceipts::decode(&bad_magic).is_err());
    }

    #[test]
    fn file_store_persists_and_reports_missing() {
        let directory = temp_dir("file");
        let mut store = FileReceiptStore::new(&directory);

        assert_eq!(store.get(5).unwrap(), None);

        let record = sample(5);
        store.put(&record).unwrap();

        // A new store over the same directory sees it (survives "restart").
        let reopened = FileReceiptStore::new(&directory);
        assert_eq!(reopened.get(5).unwrap(), Some(record));
        assert_eq!(reopened.get(6).unwrap(), None);

        // No temporary files are left behind.
        let leftovers = fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn file_store_reports_corrupt_record() {
        let directory = temp_dir("corrupt");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("2.szrb"), b"garbage").unwrap();

        let store = FileReceiptStore::new(&directory);
        assert!(store.get(2).is_err());

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn from_block_checks_receipt_count() {
        let block = SolizoneBlock {
            header: SolizoneBlockHeader {
                version: 1,
                chain_id: 9001,
                height: 1,
                parent_hash: B256::ZERO,
                timestamp: 1,
                state_root: B256::ZERO,
                transactions_root: B256::ZERO,
                receipts_root: B256::ZERO,
                gas_limit: 30_000_000,
                gas_used: 0,
            },
            transactions: vec![sample(1).entries[0].0.clone()],
        };

        assert_eq!(
            BlockReceipts::from_block(&block, &[]).unwrap_err(),
            BlockReceiptsError::ReceiptCountMismatch {
                transactions: 1,
                receipts: 0
            }
        );
    }
}
