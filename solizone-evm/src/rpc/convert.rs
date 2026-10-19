//! Conversions from Solizone data into Ethereum JSON-RPC (alloy) types.

use alloy_consensus::{
    Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom, TxEnvelope, transaction::Recovered,
};
use alloy_primitives::{B64, Bloom, logs_bloom};
use alloy_rpc_types_eth::{Block, BlockTransactions, Header, Log, Transaction, TransactionReceipt};
use revm::primitives::{Address, B256, Bytes, U256};

use crate::{
    block::SolizoneBlock,
    eth::SignedEthTransaction,
    execution::{
        receipt::{ExecutionLog, ExecutionStatus},
        transaction::TransactionKind,
    },
    node::{PendingTransaction, TxRecord},
};

/// keccak256(rlp([])) — Ethereum's empty ommers hash.
const EMPTY_OMMERS_HASH: B256 =
    alloy_primitives::b256!("1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347");

fn primitive_log(log: &ExecutionLog) -> alloy_primitives::Log {
    alloy_primitives::Log::new_unchecked(log.address, log.topics.clone(), log.data.clone())
}

/// Ethereum-shaped logs of an executed transaction.
pub fn rpc_logs(record: &TxRecord) -> Vec<Log> {
    record
        .receipt
        .logs
        .iter()
        .enumerate()
        .map(|(offset, log)| Log {
            inner: primitive_log(log),
            block_hash: Some(record.block_hash),
            block_number: Some(record.block_number),
            block_timestamp: Some(record.block_timestamp),
            transaction_hash: Some(record.hash),
            transaction_index: Some(record.index),
            log_index: Some(record.first_log_index + offset as u64),
            removed: false,
        })
        .collect()
}

fn record_bloom(record: &TxRecord) -> Bloom {
    let logs: Vec<alloy_primitives::Log> = record.receipt.logs.iter().map(primitive_log).collect();
    logs_bloom(logs.iter())
}

pub fn rpc_receipt(record: &TxRecord) -> Option<TransactionReceipt> {
    let signed = SignedEthTransaction::decode(record.tx.signed.as_ref()?).ok()?;

    let receipt = ReceiptWithBloom {
        receipt: Receipt {
            status: Eip658Value::Eip658(record.receipt.status == ExecutionStatus::Success),
            cumulative_gas_used: record.cumulative_gas_used,
            logs: rpc_logs(record),
        },
        logs_bloom: record_bloom(record),
    };

    let inner = match signed.envelope {
        TxEnvelope::Legacy(_) => ReceiptEnvelope::Legacy(receipt),
        TxEnvelope::Eip2930(_) => ReceiptEnvelope::Eip2930(receipt),
        _ => ReceiptEnvelope::Eip1559(receipt),
    };

    let to = match record.tx.kind {
        TransactionKind::Call(to) => Some(to),
        TransactionKind::Create => None,
    };

    Some(TransactionReceipt {
        inner,
        transaction_hash: record.hash,
        transaction_index: Some(record.index),
        block_hash: Some(record.block_hash),
        block_number: Some(record.block_number),
        gas_used: record.receipt.gas_used,
        effective_gas_price: 0,
        blob_gas_used: None,
        blob_gas_price: None,
        from: record.tx.sender,
        to,
        contract_address: record.receipt.contract_address,
    })
}

fn rpc_transaction_from_raw(
    raw: &Bytes,
    block: Option<(B256, u64, u64, u64)>,
) -> Option<Transaction> {
    let signed = SignedEthTransaction::decode(raw).ok()?;

    Some(Transaction {
        inner: Recovered::new_unchecked(signed.envelope, signed.sender),
        block_hash: block.map(|b| b.0),
        block_number: block.map(|b| b.1),
        transaction_index: block.map(|b| b.2),
        block_timestamp: block.map(|b| b.3),
        effective_gas_price: block.map(|_| 0),
    })
}

pub fn rpc_transaction(record: &TxRecord) -> Option<Transaction> {
    rpc_transaction_from_raw(
        record.tx.signed.as_ref()?,
        Some((
            record.block_hash,
            record.block_number,
            record.index,
            record.block_timestamp,
        )),
    )
}

pub fn rpc_pending_transaction(pending: &PendingTransaction) -> Option<Transaction> {
    rpc_transaction_from_raw(pending.tx.signed.as_ref()?, None)
}

/// Builds an Ethereum block view. `records` are the indexed transactions of
/// this block when available (needed for the logs bloom); signed
/// transactions are listed either by hash or in full.
pub fn rpc_block(block: &SolizoneBlock, records: Option<&[&TxRecord]>, full: bool) -> Block {
    let header = &block.header;
    let hash = block.hash();

    let logs_bloom = records.map_or(Bloom::ZERO, |records| {
        records.iter().fold(Bloom::ZERO, |mut bloom, record| {
            bloom.accrue_bloom(&record_bloom(record));
            bloom
        })
    });

    let signed: Vec<(u64, &Bytes)> = block
        .transactions
        .iter()
        .enumerate()
        .filter_map(|(index, tx)| tx.signed.as_ref().map(|raw| (index as u64, raw)))
        .collect();

    let transactions = if full {
        BlockTransactions::Full(
            signed
                .iter()
                .filter_map(|(index, raw)| {
                    rpc_transaction_from_raw(
                        raw,
                        Some((hash, header.height, *index, header.timestamp)),
                    )
                })
                .collect(),
        )
    } else {
        BlockTransactions::Hashes(
            signed
                .iter()
                .map(|(_, raw)| revm::primitives::keccak256(raw))
                .collect(),
        )
    };

    Block {
        header: Header {
            hash,
            inner: alloy_consensus::Header {
                parent_hash: header.parent_hash,
                ommers_hash: EMPTY_OMMERS_HASH,
                beneficiary: Address::ZERO,
                state_root: header.state_root,
                transactions_root: header.transactions_root,
                receipts_root: header.receipts_root,
                logs_bloom,
                difficulty: U256::ZERO,
                number: header.height,
                gas_limit: header.gas_limit,
                gas_used: header.gas_used,
                timestamp: header.timestamp,
                extra_data: Bytes::new(),
                mix_hash: B256::ZERO,
                nonce: B64::ZERO,
                base_fee_per_gas: Some(0),
                ..Default::default()
            },
            total_difficulty: Some(U256::ZERO),
            size: Some(U256::from(block.encode().len())),
        },
        uncles: Vec::new(),
        transactions,
        withdrawals: None,
    }
}

/// Human-readable revert reason for `Error(string)` reverts.
pub fn revert_reason(data: &[u8]) -> Option<String> {
    const ERROR_SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];

    if data.len() < 68 || data[..4] != ERROR_SELECTOR {
        return None;
    }

    let length = U256::from_be_slice(&data[36..68]);
    let length = usize::try_from(length).ok()?;

    let bytes = data.get(68..68usize.checked_add(length)?)?;

    String::from_utf8(bytes.to_vec()).ok()
}
