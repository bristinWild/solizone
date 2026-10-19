use crate::execution::transaction::SolizoneTransaction;
use crate::execution::transactions_root::compute_transactions_root;
use revm::primitives::{B256, keccak256};

pub const HEADER_SIZE: usize = 170;

/// Block containing only internal (SZT1) transactions.
const BLOCK_MAGIC: &[u8; 4] = b"SZB1";

/// Block containing at least one signed Ethereum (SZT2) transaction.
/// Same layout as SZB1; the magic tells older decoders the body has
/// entries they cannot read. Internal-only blocks keep encoding as SZB1,
/// so existing history stays byte-identical.
const SIGNED_BLOCK_MAGIC: &[u8; 4] = b"SZB2";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolizoneBlock {
    pub header: SolizoneBlockHeader,
    pub transactions: Vec<SolizoneTransaction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockValidationError {
    TransactionsRootMismatch,
    /// A signed transaction was signed for a different chain than the block.
    TransactionChainIdMismatch {
        index: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockDecodeError {
    UnexpectedEof,
    InvalidMagic,
    /// An SZB1 block contained a signed (SZT2) transaction.
    SignedTransactionInLegacyBlock,
    InvalidHeaderLength,
    TransactionDecode,
    TrailingBytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockImportError {
    Decode(BlockDecodeError),
    Validation(BlockValidationError),
}

impl SolizoneBlock {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();

        if self.has_signed_transactions() {
            bytes.extend_from_slice(SIGNED_BLOCK_MAGIC);
        } else {
            bytes.extend_from_slice(BLOCK_MAGIC);
        }

        bytes.extend_from_slice(&self.header.encode());

        let tx_count = u32::try_from(self.transactions.len()).expect("too many transactions");

        bytes.extend_from_slice(&tx_count.to_be_bytes());

        for tx in &self.transactions {
            let encoded = tx.encode_canonical();

            let tx_len = u32::try_from(encoded.len()).expect("transaction too large");

            bytes.extend_from_slice(&tx_len.to_be_bytes());

            bytes.extend_from_slice(&encoded);
        }

        bytes
    }

    pub fn hash(&self) -> B256 {
        self.header.hash()
    }

    pub fn validate_transactions_root(&self) -> bool {
        compute_transactions_root(&self.transactions) == self.header.transactions_root
    }

    pub fn has_signed_transactions(&self) -> bool {
        self.transactions.iter().any(|tx| tx.is_signed())
    }

    pub fn validate(&self) -> Result<(), BlockValidationError> {
        if !self.validate_transactions_root() {
            return Err(BlockValidationError::TransactionsRootMismatch);
        }

        for (index, tx) in self.transactions.iter().enumerate() {
            if let Some(raw) = &tx.signed {
                let chain_id = crate::eth::SignedEthTransaction::decode(raw)
                    .map(|signed| signed.chain_id())
                    .ok();

                if chain_id != Some(self.header.chain_id) {
                    return Err(BlockValidationError::TransactionChainIdMismatch { index });
                }
            }
        }

        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, BlockDecodeError> {
        let mut input = bytes;

        /*
         * Magic
         */
        let magic = take(&mut input, 4)?;

        let signed_block = if magic == SIGNED_BLOCK_MAGIC {
            true
        } else if magic == BLOCK_MAGIC {
            false
        } else {
            return Err(BlockDecodeError::InvalidMagic);
        };

        /*
         * Header
         */
        let header_bytes = take(&mut input, HEADER_SIZE)?;

        let header = SolizoneBlockHeader::decode(header_bytes)?;

        /*
         * Transaction count
         */
        let tx_count = u32::from_be_bytes(take(&mut input, 4)?.try_into().unwrap()) as usize;

        let mut transactions = Vec::with_capacity(tx_count);

        /*
         * Transactions
         */
        for _ in 0..tx_count {
            let tx_len = u32::from_be_bytes(take(&mut input, 4)?.try_into().unwrap()) as usize;

            let tx_bytes = take(&mut input, tx_len)?;

            let tx = SolizoneTransaction::decode_canonical(tx_bytes)
                .map_err(|_| BlockDecodeError::TransactionDecode)?;

            if tx.is_signed() && !signed_block {
                return Err(BlockDecodeError::SignedTransactionInLegacyBlock);
            }

            transactions.push(tx);
        }

        /*
         * No extra bytes allowed
         */
        if !input.is_empty() {
            return Err(BlockDecodeError::TrailingBytes);
        }

        Ok(Self {
            header,
            transactions,
        })
    }

    pub fn decode_and_validate(bytes: &[u8]) -> Result<Self, BlockImportError> {
        let block = Self::decode(bytes).map_err(BlockImportError::Decode)?;

        block.validate().map_err(BlockImportError::Validation)?;

        Ok(block)
    }
}

fn take<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8], BlockDecodeError> {
    if input.len() < len {
        return Err(BlockDecodeError::UnexpectedEof);
    }

    let (head, tail) = input.split_at(len);
    *input = tail;

    Ok(head)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolizoneBlockHeader {
    pub version: u16,
    pub chain_id: u64,
    pub height: u64,
    pub parent_hash: B256,
    pub timestamp: u64,
    pub state_root: B256,
    pub transactions_root: B256,
    pub receipts_root: B256,
    pub gas_limit: u64,
    pub gas_used: u64,
}

impl SolizoneBlockHeader {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER_SIZE);

        bytes.extend_from_slice(&self.version.to_be_bytes());

        bytes.extend_from_slice(&self.chain_id.to_be_bytes());

        bytes.extend_from_slice(&self.height.to_be_bytes());

        bytes.extend_from_slice(self.parent_hash.as_slice());

        bytes.extend_from_slice(&self.timestamp.to_be_bytes());

        bytes.extend_from_slice(self.state_root.as_slice());

        bytes.extend_from_slice(self.transactions_root.as_slice());

        bytes.extend_from_slice(self.receipts_root.as_slice());

        bytes.extend_from_slice(&self.gas_limit.to_be_bytes());

        bytes.extend_from_slice(&self.gas_used.to_be_bytes());

        bytes
    }

    pub fn hash(&self) -> B256 {
        keccak256(self.encode())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, BlockDecodeError> {
        if bytes.len() != HEADER_SIZE {
            return Err(BlockDecodeError::InvalidHeaderLength);
        }

        let mut input = bytes;

        let version = u16::from_be_bytes(take(&mut input, 2)?.try_into().unwrap());

        let chain_id = u64::from_be_bytes(take(&mut input, 8)?.try_into().unwrap());

        let height = u64::from_be_bytes(take(&mut input, 8)?.try_into().unwrap());

        let parent_hash = B256::from_slice(take(&mut input, 32)?);

        let timestamp = u64::from_be_bytes(take(&mut input, 8)?.try_into().unwrap());

        let state_root = B256::from_slice(take(&mut input, 32)?);

        let transactions_root = B256::from_slice(take(&mut input, 32)?);

        let receipts_root = B256::from_slice(take(&mut input, 32)?);

        let gas_limit = u64::from_be_bytes(take(&mut input, 8)?.try_into().unwrap());

        let gas_used = u64::from_be_bytes(take(&mut input, 8)?.try_into().unwrap());

        Ok(Self {
            version,
            chain_id,
            height,
            parent_hash,
            timestamp,
            state_root,
            transactions_root,
            receipts_root,
            gas_limit,
            gas_used,
        })
    }
}

#[cfg(test)]
mod signed_block_tests {
    use super::*;
    use crate::block_builder::build_block;
    use crate::eth::SignedEthTransaction;
    use crate::eth::test_utils::{TxParams, dev_signer, sign_eip1559};
    use crate::execution::transaction::TransactionKind;
    use revm::primitives::{Bytes, U256, address};

    fn internal_tx() -> SolizoneTransaction {
        SolizoneTransaction {
            sender: address!("1111111111111111111111111111111111111111"),
            nonce: 0,
            kind: TransactionKind::Call(address!("2222222222222222222222222222222222222222")),
            value: U256::from(1),
            data: Bytes::new(),
            gas_limit: 21_000,
            signed: None,
        }
    }

    fn signed_tx(chain_id: u64) -> SolizoneTransaction {
        let raw = sign_eip1559(
            &dev_signer(0),
            TxParams::transfer(
                chain_id,
                0,
                address!("2222222222222222222222222222222222222222"),
                5,
            ),
        );
        SignedEthTransaction::decode(&raw).unwrap().to_solizone()
    }

    fn block(chain_id: u64, txs: Vec<SolizoneTransaction>) -> SolizoneBlock {
        build_block(
            1,
            chain_id,
            3,
            B256::ZERO,
            1_800_000_000,
            B256::ZERO,
            &txs,
            &[],
            30_000_000,
        )
    }

    #[test]
    fn internal_only_blocks_stay_szb1() {
        let block = block(9001, vec![internal_tx()]);
        let bytes = block.encode();

        assert_eq!(&bytes[..4], b"SZB1");
        assert_eq!(SolizoneBlock::decode_and_validate(&bytes).unwrap(), block);
    }

    #[test]
    fn signed_blocks_round_trip_as_szb2() {
        let block = block(9001, vec![internal_tx(), signed_tx(9001)]);
        let bytes = block.encode();

        assert_eq!(&bytes[..4], b"SZB2");

        let decoded = SolizoneBlock::decode_and_validate(&bytes).unwrap();
        assert_eq!(decoded, block);
        assert!(decoded.transactions[1].is_signed());
    }

    #[test]
    fn szb1_block_cannot_smuggle_signed_transactions() {
        let mut bytes = block(9001, vec![signed_tx(9001)]).encode();
        bytes[..4].copy_from_slice(b"SZB1");

        assert_eq!(
            SolizoneBlock::decode(&bytes).unwrap_err(),
            BlockDecodeError::SignedTransactionInLegacyBlock
        );
    }

    #[test]
    fn rejects_signed_transaction_for_another_chain() {
        let block = block(9001, vec![signed_tx(1)]);

        assert_eq!(
            block.validate(),
            Err(BlockValidationError::TransactionChainIdMismatch { index: 0 })
        );
    }
}
