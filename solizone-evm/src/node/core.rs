//! In-memory heart of a running Solizone node.
//!
//! `NodeCore` owns the canonical EVM state, the block producer, the
//! Solizone mempool and the indexes needed to answer Ethereum JSON-RPC
//! queries. It performs no I/O: persistence and publication are driven by
//! the runtime around it (`node::runtime`), which never holds the core lock
//! while talking to storage.

use std::collections::{BTreeMap, HashMap};

use revm::primitives::{Address, B256, Bytes, KECCAK_EMPTY, U256};
use revm::state::AccountInfo;
use revm_bytecode::Bytecode;

use crate::{
    block::SolizoneBlock,
    block_producer::BlockProducer,
    eth::{EthTransactionError, SignedEthTransaction},
    execution::{
        BlockContext, ExecutionError, RevmExecutionEngine,
        receipt::{ExecutionReceipt, ExecutionStatus},
        revm_engine::MAX_TX_GAS,
        transaction::{SolizoneTransaction, TransactionKind},
    },
    node::mempool::{Mempool, MempoolError, PendingTransaction},
    state::{MemoryState, SolizoneCheckpoint},
};

#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub version: u16,
    pub chain_id: u64,
    pub block_gas_limit: u64,
    pub mempool_capacity: usize,
    /// Most recent blocks kept in memory for RPC; older ones come from storage.
    pub recent_block_cache: usize,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            version: 1,
            chain_id: 9001,
            block_gas_limit: 30_000_000,
            mempool_capacity: 4_096,
            recent_block_cache: 1_024,
        }
    }
}

/// Why `eth_sendRawTransaction` refused a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    Decode(EthTransactionError),
    Pool(MempoolError),
    /// The transaction would not execute against the pending state
    /// (nonce, funds, intrinsic gas, ...).
    Execution(ExecutionError),
    GasLimitTooHigh {
        gas_limit: u64,
        max: u64,
    },
    Halted(String),
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(error) => write!(f, "{error}"),
            Self::Pool(error) => write!(f, "{error}"),
            Self::Execution(error) => write!(f, "{error}"),
            Self::GasLimitTooHigh { gas_limit, max } => {
                write!(f, "transaction gas limit {gas_limit} exceeds maximum {max}")
            }
            Self::Halted(reason) => write!(f, "node halted: {reason}"),
        }
    }
}

impl std::error::Error for SubmitError {}

/// Which state a query runs against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateView {
    /// State after the latest produced block.
    Latest,
    /// Latest state plus every transaction currently in the mempool.
    Pending,
}

/// Parameters of `eth_call` / `eth_estimateGas`.
#[derive(Debug, Clone, Default)]
pub struct CallRequest {
    pub from: Option<Address>,
    pub to: Option<Address>,
    pub value: U256,
    pub data: Bytes,
    pub gas: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    Execution(ExecutionError),
    /// Execution reverted; carries the revert data.
    Reverted(Bytes),
    /// Execution halted (out of gas, invalid opcode, ...).
    Halted,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Execution(error) => write!(f, "{error}"),
            Self::Reverted(_) => write!(f, "execution reverted"),
            Self::Halted => write!(f, "execution halted"),
        }
    }
}

/// An executed, indexed Ethereum transaction.
#[derive(Debug, Clone)]
pub struct TxRecord {
    pub hash: B256,
    pub tx: SolizoneTransaction,
    pub receipt: ExecutionReceipt,
    pub block_number: u64,
    pub block_hash: B256,
    pub block_timestamp: u64,
    pub index: u64,
    pub cumulative_gas_used: u64,
    /// Index of this transaction's first log within the block.
    pub first_log_index: u64,
}

/// A block plus the checkpoint that must be persisted with it.
#[derive(Debug, Clone)]
pub struct BlockCommit {
    pub block: SolizoneBlock,
    pub checkpoint: SolizoneCheckpoint,
    pub rejected: usize,
}

pub struct NodeCore {
    config: NodeConfig,
    engine: RevmExecutionEngine,
    state: MemoryState,
    pending_state: MemoryState,
    producer: BlockProducer,
    mempool: Mempool,
    last_timestamp: u64,
    recent_blocks: BTreeMap<u64, SolizoneBlock>,
    recent_heights: HashMap<B256, u64>,
    transactions: HashMap<B256, TxRecord>,
    block_transactions: HashMap<u64, Vec<B256>>,
    halted: Option<String>,
}

impl NodeCore {
    /// Fresh chain: applies the genesis balances and produces the empty
    /// genesis block #0 (Ethereum tooling expects a `latest` block to exist).
    /// The returned commit must be persisted like any other block.
    pub fn new(config: NodeConfig, genesis: &[(Address, U256)], now: u64) -> (Self, BlockCommit) {
        let mut state = MemoryState::new();

        for (address, balance) in genesis {
            state.insert_account_info(*address, AccountInfo::from_balance(*balance));
        }

        let producer = BlockProducer::new(config.version, config.chain_id, config.block_gas_limit);

        let mut core = Self::assemble(config, state, producer, None);

        let produced =
            core.producer
                .produce_block_with_report(&core.engine, &mut core.state, &[], now);

        core.last_timestamp = now;
        core.index_block(&produced.block, &produced.receipts);
        core.pending_state = core.state.clone();

        let commit = BlockCommit {
            block: produced.block.clone(),
            checkpoint: SolizoneCheckpoint::new(
                core.state.snapshot(),
                core.producer.next_height(),
                core.producer.parent_hash(),
            ),
            rejected: 0,
        };

        core.cache_block(produced.block);

        (core, commit)
    }

    /// Node restarted from a checkpoint whose head block is `head`.
    pub fn resume(
        config: NodeConfig,
        checkpoint: SolizoneCheckpoint,
        head: Option<SolizoneBlock>,
    ) -> Self {
        let producer = BlockProducer::resume(
            config.version,
            config.chain_id,
            config.block_gas_limit,
            checkpoint.next_height,
            checkpoint.parent_hash,
        );

        let state = MemoryState::from_snapshot(checkpoint.state);

        Self::assemble(config, state, producer, head)
    }

    fn assemble(
        config: NodeConfig,
        state: MemoryState,
        producer: BlockProducer,
        head: Option<SolizoneBlock>,
    ) -> Self {
        let mut core = Self {
            mempool: Mempool::new(config.mempool_capacity),
            pending_state: state.clone(),
            config,
            engine: RevmExecutionEngine::new(),
            state,
            producer,
            last_timestamp: 0,
            recent_blocks: BTreeMap::new(),
            recent_heights: HashMap::new(),
            transactions: HashMap::new(),
            block_transactions: HashMap::new(),
            halted: None,
        };

        if let Some(head) = head {
            core.last_timestamp = head.header.timestamp;
            core.cache_block(head);
        }

        core
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    pub fn chain_id(&self) -> u64 {
        self.config.chain_id
    }

    /// Height of the latest produced block, or `None` before the first block.
    pub fn head_height(&self) -> Option<u64> {
        self.producer.next_height().checked_sub(1)
    }

    pub fn mempool_len(&self) -> usize {
        self.mempool.len()
    }

    pub fn halt(&mut self, reason: impl Into<String>) {
        self.halted = Some(reason.into());
    }

    pub fn halted(&self) -> Option<&str> {
        self.halted.as_deref()
    }

    fn next_timestamp(&self, now: u64) -> u64 {
        now.max(self.last_timestamp)
    }

    fn next_context(&self, now: u64) -> BlockContext {
        self.producer.block_context(self.next_timestamp(now))
    }

    /// Context for read-only queries at the latest block.
    fn latest_context(&self, now: u64) -> BlockContext {
        match self.head_height() {
            Some(height) => BlockContext::new(
                self.config.chain_id,
                height,
                self.last_timestamp.max(1),
                self.config.block_gas_limit,
            ),
            None => self.next_context(now),
        }
    }

    // ---------------------------------------------------------------------
    // Transaction admission
    // ---------------------------------------------------------------------

    /// `eth_sendRawTransaction`: decode, verify, dry-run against the
    /// pending state, and queue. Returns the Ethereum transaction hash.
    pub fn submit_raw_transaction(&mut self, raw: &[u8], now: u64) -> Result<B256, SubmitError> {
        if let Some(reason) = &self.halted {
            return Err(SubmitError::Halted(reason.clone()));
        }

        let signed = SignedEthTransaction::decode_for_chain(raw, self.config.chain_id)
            .map_err(SubmitError::Decode)?;

        let hash = signed.hash();
        let tx = signed.to_solizone();

        let max = MAX_TX_GAS.min(self.config.block_gas_limit);

        if tx.gas_limit > max {
            return Err(SubmitError::GasLimitTooHigh {
                gas_limit: tx.gas_limit,
                max,
            });
        }

        let pending = PendingTransaction { hash, tx };

        self.mempool.check(&pending).map_err(SubmitError::Pool)?;

        let ctx = self.next_context(now);

        self.engine
            .try_execute_transaction(&ctx, &mut self.pending_state, &pending.tx)
            .map_err(SubmitError::Execution)?;

        self.mempool.insert(pending).map_err(SubmitError::Pool)?;

        Ok(hash)
    }

    pub fn pending_transaction(&self, hash: &B256) -> Option<&PendingTransaction> {
        self.mempool.get(hash)
    }

    // ---------------------------------------------------------------------
    // Block production
    // ---------------------------------------------------------------------

    /// Produces the next block from the mempool. Returns `None` when there
    /// is nothing to include or the node is halted.
    pub fn produce_block(&mut self, now: u64) -> Option<BlockCommit> {
        if self.halted.is_some() || self.mempool.is_empty() {
            return None;
        }

        let candidates = self.mempool.take_for_block(self.config.block_gas_limit);

        let txs: Vec<SolizoneTransaction> = candidates.iter().map(|p| p.tx.clone()).collect();

        let timestamp = self.next_timestamp(now);

        let produced =
            self.producer
                .produce_block_with_report(&self.engine, &mut self.state, &txs, timestamp);

        for rejected in &produced.rejected {
            eprintln!(
                "dropping transaction {:?} from {} nonce {}: {}",
                rejected.transaction.eth_hash(),
                rejected.transaction.sender,
                rejected.transaction.nonce,
                rejected.error
            );
        }

        self.last_timestamp = timestamp;

        self.index_block(&produced.block, &produced.receipts);

        self.rebuild_pending_state(now);

        let checkpoint = SolizoneCheckpoint::new(
            self.state.snapshot(),
            self.producer.next_height(),
            self.producer.parent_hash(),
        );

        let block = produced.block.clone();

        self.cache_block(produced.block);

        Some(BlockCommit {
            block,
            checkpoint,
            rejected: produced.rejected.len(),
        })
    }

    /// After a block, the pending state is the new canonical state plus any
    /// transactions left in the pool (e.g. ones that did not fit). Any that
    /// no longer execute are dropped.
    fn rebuild_pending_state(&mut self, now: u64) {
        self.pending_state = self.state.clone();

        let remaining = self.mempool.drain_all();

        let ctx = self.next_context(now);

        for pending in remaining {
            match self
                .engine
                .try_execute_transaction(&ctx, &mut self.pending_state, &pending.tx)
            {
                Ok(_) => {
                    self.mempool
                        .insert(pending)
                        .expect("re-inserting drained transaction");
                }
                Err(error) => eprintln!("dropping pending transaction {}: {error}", pending.hash),
            }
        }
    }

    fn index_block(&mut self, block: &SolizoneBlock, receipts: &[ExecutionReceipt]) {
        let block_hash = block.hash();
        let mut cumulative_gas_used = 0;
        let mut log_index = 0;
        let mut hashes = Vec::new();

        for (index, (tx, receipt)) in block.transactions.iter().zip(receipts).enumerate() {
            cumulative_gas_used += receipt.gas_used;

            if let Some(hash) = tx.eth_hash() {
                self.transactions.insert(
                    hash,
                    TxRecord {
                        hash,
                        tx: tx.clone(),
                        receipt: receipt.clone(),
                        block_number: block.header.height,
                        block_hash,
                        block_timestamp: block.header.timestamp,
                        index: index as u64,
                        cumulative_gas_used,
                        first_log_index: log_index,
                    },
                );

                hashes.push(hash);
            }

            log_index += receipt.logs.len() as u64;
        }

        self.block_transactions.insert(block.header.height, hashes);
    }

    fn cache_block(&mut self, block: SolizoneBlock) {
        self.recent_heights
            .insert(block.hash(), block.header.height);
        self.recent_blocks.insert(block.header.height, block);

        while self.recent_blocks.len() > self.config.recent_block_cache {
            let (_, evicted) = self.recent_blocks.pop_first().expect("non-empty");
            self.recent_heights.remove(&evicted.hash());
        }
    }

    // ---------------------------------------------------------------------
    // Queries
    // ---------------------------------------------------------------------

    pub fn cached_block_by_height(&self, height: u64) -> Option<&SolizoneBlock> {
        self.recent_blocks.get(&height)
    }

    pub fn cached_block_by_hash(&self, hash: &B256) -> Option<&SolizoneBlock> {
        self.recent_heights
            .get(hash)
            .and_then(|height| self.recent_blocks.get(height))
    }

    pub fn transaction(&self, hash: &B256) -> Option<&TxRecord> {
        self.transactions.get(hash)
    }

    /// Indexed transactions of a block, in block order. `None` if the block
    /// was produced before this process started (index not available).
    pub fn block_transaction_records(&self, height: u64) -> Option<Vec<&TxRecord>> {
        self.block_transactions.get(&height).map(|hashes| {
            hashes
                .iter()
                .filter_map(|h| self.transactions.get(h))
                .collect()
        })
    }

    fn view(&self, view: StateView) -> &MemoryState {
        match view {
            StateView::Latest => &self.state,
            StateView::Pending => &self.pending_state,
        }
    }

    pub fn balance(&self, address: &Address, view: StateView) -> U256 {
        self.view(view)
            .db()
            .cache
            .accounts
            .get(address)
            .map_or(U256::ZERO, |account| account.info.balance)
    }

    pub fn nonce(&self, address: &Address, view: StateView) -> u64 {
        self.view(view)
            .db()
            .cache
            .accounts
            .get(address)
            .map_or(0, |account| account.info.nonce)
    }

    pub fn code(&self, address: &Address, view: StateView) -> Bytes {
        let db = self.view(view).db();

        let Some(account) = db.cache.accounts.get(address) else {
            return Bytes::new();
        };

        let bytecode: Option<&Bytecode> = account.info.code.as_ref().or_else(|| {
            (account.info.code_hash != KECCAK_EMPTY)
                .then(|| db.cache.contracts.get(&account.info.code_hash))
                .flatten()
        });

        bytecode.map_or_else(Bytes::new, |code| code.original_bytes())
    }

    pub fn storage_at(&self, address: &Address, slot: U256, view: StateView) -> U256 {
        self.view(view)
            .db()
            .cache
            .accounts
            .get(address)
            .and_then(|account| account.storage.get(&slot).copied())
            .unwrap_or(U256::ZERO)
    }

    fn call_transaction(
        &self,
        request: &CallRequest,
        view: StateView,
        gas: u64,
    ) -> SolizoneTransaction {
        let sender = request.from.unwrap_or(Address::ZERO);

        SolizoneTransaction {
            sender,
            nonce: self.nonce(&sender, view),
            kind: request
                .to
                .map_or(TransactionKind::Create, TransactionKind::Call),
            value: request.value,
            data: request.data.clone(),
            gas_limit: gas,
            signed: None,
        }
    }

    fn max_call_gas(&self) -> u64 {
        MAX_TX_GAS.min(self.config.block_gas_limit)
    }

    fn query_context(&self, view: StateView, now: u64) -> BlockContext {
        match view {
            StateView::Latest => self.latest_context(now),
            StateView::Pending => self.next_context(now),
        }
    }

    fn run_call(
        &mut self,
        request: &CallRequest,
        view: StateView,
        gas: u64,
        now: u64,
    ) -> Result<ExecutionReceipt, ExecutionError> {
        let ctx = self.query_context(view, now);
        let tx = self.call_transaction(request, view, gas);

        let state = match view {
            StateView::Latest => &mut self.state,
            StateView::Pending => &mut self.pending_state,
        };

        self.engine.try_execute_call(&ctx, state, &tx)
    }

    /// `eth_call`: read-only execution; state is never modified.
    pub fn call(
        &mut self,
        request: &CallRequest,
        view: StateView,
        now: u64,
    ) -> Result<Bytes, CallError> {
        let gas = request.gas.unwrap_or(u64::MAX).min(self.max_call_gas());

        let receipt = self
            .run_call(request, view, gas, now)
            .map_err(CallError::Execution)?;

        match receipt.status {
            ExecutionStatus::Success => Ok(receipt.output),
            ExecutionStatus::Revert => Err(CallError::Reverted(receipt.output)),
            ExecutionStatus::Halt => Err(CallError::Halted),
        }
    }

    /// `eth_estimateGas`: smallest gas limit (within ~1.5%) at which the
    /// request succeeds, found by binary search like geth.
    pub fn estimate_gas(
        &mut self,
        request: &CallRequest,
        view: StateView,
        now: u64,
    ) -> Result<u64, CallError> {
        let cap = request.gas.unwrap_or(u64::MAX).min(self.max_call_gas());

        let receipt = self
            .run_call(request, view, cap, now)
            .map_err(CallError::Execution)?;

        match receipt.status {
            ExecutionStatus::Success => {}
            ExecutionStatus::Revert => return Err(CallError::Reverted(receipt.output)),
            ExecutionStatus::Halt => return Err(CallError::Halted),
        }

        let mut lo = receipt.gas_used.saturating_sub(1);
        let mut hi = cap;

        // Optimistic first probe just above the used gas.
        let optimistic = (receipt.gas_used + 2_300) * 64 / 63;

        if optimistic < hi && self.succeeds(request, view, optimistic, now) {
            hi = optimistic;
        } else if optimistic < hi {
            lo = optimistic;
        }

        while hi - lo > 1 && (hi - lo) * 1000 / hi > 15 {
            let mid = lo + (hi - lo) / 2;

            if self.succeeds(request, view, mid, now) {
                hi = mid;
            } else {
                lo = mid;
            }
        }

        Ok(hi)
    }

    fn succeeds(&mut self, request: &CallRequest, view: StateView, gas: u64, now: u64) -> bool {
        matches!(
            self.run_call(request, view, gas, now),
            Ok(ExecutionReceipt {
                status: ExecutionStatus::Success,
                ..
            })
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::test_utils::{TxParams, dev_signer, sign_eip1559};
    use crate::node::dev_accounts::{DEV_ACCOUNTS, dev_account_balance};
    use revm::primitives::address;

    const NOW: u64 = 1_800_000_000;
    const BOB: Address = address!("2222222222222222222222222222222222222222");

    fn node() -> NodeCore {
        let genesis: Vec<(Address, U256)> = DEV_ACCOUNTS
            .iter()
            .map(|account| (account.address, dev_account_balance()))
            .collect();

        NodeCore::new(NodeConfig::default(), &genesis, NOW).0
    }

    fn counter_initcode() -> Bytes {
        let hex = include_str!("../../contracts/Counter.creation.hex").trim();
        Bytes::from(hex::decode(hex.trim_start_matches("0x")).unwrap())
    }

    // keccak("increment()")[..4] and keccak("count()")[..4]
    const INCREMENT: [u8; 4] = [0xd0, 0x9d, 0xe0, 0x8a];
    const COUNT: [u8; 4] = [0x06, 0x66, 0x1a, 0xbd];

    #[test]
    fn fresh_chain_starts_with_genesis_block() {
        let (node, genesis) = NodeCore::new(
            NodeConfig::default(),
            &[(DEV_ACCOUNTS[0].address, dev_account_balance())],
            NOW,
        );

        assert_eq!(genesis.block.header.height, 0);
        assert!(genesis.block.transactions.is_empty());
        assert_eq!(genesis.block.header.parent_hash, B256::ZERO);
        assert_eq!(genesis.checkpoint.next_height, 1);
        assert_eq!(node.head_height(), Some(0));
        assert_eq!(
            node.balance(&DEV_ACCOUNTS[0].address, StateView::Latest),
            dev_account_balance()
        );
    }

    #[test]
    fn transfer_flows_from_submit_to_receipt() {
        let mut node = node();
        let alice = DEV_ACCOUNTS[0].address;

        let raw = sign_eip1559(&dev_signer(0), TxParams::transfer(9001, 0, BOB, 1_000));
        let hash = node.submit_raw_transaction(&raw, NOW).unwrap();

        // Visible as pending before the block.
        assert_eq!(node.nonce(&alice, StateView::Pending), 1);
        assert_eq!(node.nonce(&alice, StateView::Latest), 0);
        assert!(node.pending_transaction(&hash).is_some());
        assert!(node.transaction(&hash).is_none());

        let commit = node.produce_block(NOW).expect("block produced");
        assert_eq!(commit.block.header.height, 1);
        assert!(commit.block.has_signed_transactions());
        assert_eq!(commit.checkpoint.next_height, 2);

        let record = node.transaction(&hash).expect("indexed");
        assert_eq!(record.block_number, 1);
        assert_eq!(record.receipt.status, ExecutionStatus::Success);
        assert_eq!(node.balance(&BOB, StateView::Latest), U256::from(1_000));
        assert_eq!(node.head_height(), Some(1));

        // Nothing left: no empty blocks.
        assert!(node.produce_block(NOW + 1).is_none());
    }

    #[test]
    fn rejects_bad_transactions_at_admission() {
        let mut node = node();
        let signer = dev_signer(0);

        let gap = sign_eip1559(&signer, TxParams::transfer(9001, 5, BOB, 1));
        assert!(matches!(
            node.submit_raw_transaction(&gap, NOW),
            Err(SubmitError::Execution(_))
        ));

        let wrong_chain = sign_eip1559(&signer, TxParams::transfer(1, 0, BOB, 1));
        assert!(matches!(
            node.submit_raw_transaction(&wrong_chain, NOW),
            Err(SubmitError::Decode(
                EthTransactionError::WrongChainId { .. }
            ))
        ));

        let ok = sign_eip1559(&signer, TxParams::transfer(9001, 0, BOB, 1));
        node.submit_raw_transaction(&ok, NOW).unwrap();
        assert!(matches!(
            node.submit_raw_transaction(&ok, NOW),
            Err(SubmitError::Pool(MempoolError::AlreadyKnown))
        ));

        // Sequential nonces from the same sender queue up fine.
        let next = sign_eip1559(&signer, TxParams::transfer(9001, 1, BOB, 1));
        node.submit_raw_transaction(&next, NOW).unwrap();
        assert_eq!(node.mempool_len(), 2);

        let mut too_much_gas = TxParams::transfer(9001, 2, BOB, 1);
        too_much_gas.gas_limit = 20_000_000;
        assert!(matches!(
            node.submit_raw_transaction(&sign_eip1559(&signer, too_much_gas), NOW),
            Err(SubmitError::GasLimitTooHigh { .. })
        ));
    }

    #[test]
    fn deploys_and_calls_counter_via_signed_transactions() {
        let mut node = node();
        let signer = dev_signer(1);
        let deployer = signer.address();

        let deploy = TxParams {
            chain_id: 9001,
            nonce: 0,
            to: None,
            value: U256::ZERO,
            input: counter_initcode(),
            gas_limit: 0,
        };

        let estimate = node
            .estimate_gas(
                &CallRequest {
                    from: Some(deployer),
                    data: deploy.input.clone(),
                    ..Default::default()
                },
                StateView::Pending,
                NOW,
            )
            .unwrap();

        let deploy_hash = node
            .submit_raw_transaction(
                &sign_eip1559(
                    &signer,
                    TxParams {
                        gas_limit: estimate,
                        ..deploy
                    },
                ),
                NOW,
            )
            .unwrap();

        node.produce_block(NOW).unwrap();

        let counter = node
            .transaction(&deploy_hash)
            .unwrap()
            .receipt
            .contract_address
            .expect("contract created");

        assert!(!node.code(&counter, StateView::Latest).is_empty());

        let increment = TxParams {
            chain_id: 9001,
            nonce: 1,
            to: Some(counter),
            value: U256::ZERO,
            input: Bytes::from(INCREMENT.to_vec()),
            gas_limit: 100_000,
        };

        let increment_hash = node
            .submit_raw_transaction(&sign_eip1559(&signer, increment), NOW + 2)
            .unwrap();

        node.produce_block(NOW + 2).unwrap();

        let record = node.transaction(&increment_hash).unwrap();
        assert_eq!(record.block_number, 2);
        assert_eq!(record.receipt.logs.len(), 1);

        let output = node
            .call(
                &CallRequest {
                    to: Some(counter),
                    data: Bytes::from(COUNT.to_vec()),
                    ..Default::default()
                },
                StateView::Latest,
                NOW + 3,
            )
            .unwrap();

        assert_eq!(U256::from_be_slice(&output), U256::from(1));
    }

    #[test]
    fn call_reports_reverts() {
        let mut node = node();
        let signer = dev_signer(0);

        let raw = sign_eip1559(
            &signer,
            TxParams {
                chain_id: 9001,
                nonce: 0,
                to: None,
                value: U256::ZERO,
                input: counter_initcode(),
                gas_limit: 1_000_000,
            },
        );
        let hash = node.submit_raw_transaction(&raw, NOW).unwrap();
        node.produce_block(NOW).unwrap();
        let counter = node
            .transaction(&hash)
            .unwrap()
            .receipt
            .contract_address
            .unwrap();

        // fail() selector = keccak("fail()")[..4]
        let result = node.call(
            &CallRequest {
                to: Some(counter),
                data: Bytes::from(vec![0xa9, 0xcc, 0x47, 0x18]),
                ..Default::default()
            },
            StateView::Latest,
            NOW,
        );

        assert!(matches!(result, Err(CallError::Reverted(data)) if !data.is_empty()));
    }

    #[test]
    fn leftover_transactions_carry_to_next_block() {
        let config = NodeConfig {
            block_gas_limit: 42_000,
            ..NodeConfig::default()
        };
        let (mut node, _) = NodeCore::new(
            config,
            &[(DEV_ACCOUNTS[0].address, dev_account_balance())],
            NOW,
        );
        let signer = dev_signer(0);

        for nonce in 0..3 {
            node.submit_raw_transaction(
                &sign_eip1559(&signer, TxParams::transfer(9001, nonce, BOB, 1)),
                NOW,
            )
            .unwrap();
        }

        assert_eq!(node.produce_block(NOW).unwrap().block.transactions.len(), 2);
        assert_eq!(node.mempool_len(), 1);
        assert_eq!(
            node.produce_block(NOW + 1)
                .unwrap()
                .block
                .transactions
                .len(),
            1
        );
        assert_eq!(node.balance(&BOB, StateView::Latest), U256::from(3));
    }

    #[test]
    fn resumes_from_checkpoint() {
        let mut node = node();
        let raw = sign_eip1559(&dev_signer(0), TxParams::transfer(9001, 0, BOB, 7));
        node.submit_raw_transaction(&raw, NOW).unwrap();
        let commit = node.produce_block(NOW).unwrap();

        let mut restored =
            NodeCore::resume(NodeConfig::default(), commit.checkpoint, Some(commit.block));

        assert_eq!(restored.head_height(), Some(1));
        assert_eq!(restored.balance(&BOB, StateView::Latest), U256::from(7));
        assert_eq!(
            restored.nonce(&DEV_ACCOUNTS[0].address, StateView::Pending),
            1
        );

        let next = sign_eip1559(&dev_signer(0), TxParams::transfer(9001, 1, BOB, 1));
        restored.submit_raw_transaction(&next, NOW + 5).unwrap();
        let commit = restored.produce_block(NOW + 5).unwrap();
        assert_eq!(commit.block.header.height, 2);
    }
}
