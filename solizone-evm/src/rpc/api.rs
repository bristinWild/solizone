//! Ethereum JSON-RPC surface of a Solizone node.

use alloy_primitives::{U64, hex};
use alloy_rpc_types_eth::{
    Block, BlockId, BlockNumberOrTag, FeeHistory, Filter, FilterBlockOption, Log, Transaction,
    TransactionReceipt, TransactionRequest,
};
use jsonrpsee::{
    core::RpcResult,
    proc_macros::rpc,
    types::{ErrorObject, ErrorObjectOwned},
};
use revm::primitives::{Address, B256, Bytes, TxKind, U256};

use crate::{
    node::{CallError, CallRequest, NodeCore, NodeHandle, StateView, runtime::unix_now},
    rpc::convert,
};

/// Maximum block range a single `eth_getLogs` request may scan.
const MAX_LOG_RANGE: u64 = 10_000;

pub const CLIENT_VERSION: &str = concat!("solizone/v", env!("CARGO_PKG_VERSION"));

#[rpc(server)]
pub trait EthApi {
    #[method(name = "web3_clientVersion")]
    fn client_version(&self) -> RpcResult<String>;

    #[method(name = "net_version")]
    fn net_version(&self) -> RpcResult<String>;

    #[method(name = "net_listening")]
    fn net_listening(&self) -> RpcResult<bool>;

    #[method(name = "eth_chainId")]
    fn chain_id(&self) -> RpcResult<U64>;

    #[method(name = "eth_blockNumber")]
    fn block_number(&self) -> RpcResult<U64>;

    #[method(name = "eth_syncing")]
    fn syncing(&self) -> RpcResult<bool>;

    #[method(name = "eth_accounts")]
    fn accounts(&self) -> RpcResult<Vec<Address>>;

    #[method(name = "eth_gasPrice")]
    fn gas_price(&self) -> RpcResult<U256>;

    #[method(name = "eth_maxPriorityFeePerGas")]
    fn max_priority_fee_per_gas(&self) -> RpcResult<U256>;

    #[method(name = "eth_feeHistory")]
    fn fee_history(
        &self,
        block_count: U64,
        newest_block: BlockNumberOrTag,
        reward_percentiles: Option<Vec<f64>>,
    ) -> RpcResult<FeeHistory>;

    #[method(name = "eth_getBalance")]
    fn balance(&self, address: Address, block: Option<BlockId>) -> RpcResult<U256>;

    #[method(name = "eth_getTransactionCount")]
    fn transaction_count(&self, address: Address, block: Option<BlockId>) -> RpcResult<U64>;

    #[method(name = "eth_getCode")]
    fn code(&self, address: Address, block: Option<BlockId>) -> RpcResult<Bytes>;

    #[method(name = "eth_getStorageAt")]
    fn storage_at(&self, address: Address, slot: U256, block: Option<BlockId>) -> RpcResult<B256>;

    #[method(name = "eth_call", blocking)]
    fn call(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        state_overrides: Option<serde_json::Value>,
    ) -> RpcResult<Bytes>;

    #[method(name = "eth_estimateGas", blocking)]
    fn estimate_gas(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        state_overrides: Option<serde_json::Value>,
    ) -> RpcResult<U64>;

    #[method(name = "eth_sendRawTransaction", blocking)]
    fn send_raw_transaction(&self, raw: Bytes) -> RpcResult<B256>;

    #[method(name = "eth_getTransactionByHash")]
    fn transaction_by_hash(&self, hash: B256) -> RpcResult<Option<Transaction>>;

    #[method(name = "eth_getTransactionReceipt")]
    fn transaction_receipt(&self, hash: B256) -> RpcResult<Option<TransactionReceipt>>;

    #[method(name = "eth_getBlockByNumber", blocking)]
    fn block_by_number(&self, number: BlockNumberOrTag, full: bool) -> RpcResult<Option<Block>>;

    #[method(name = "eth_getBlockByHash", blocking)]
    fn block_by_hash(&self, hash: B256, full: bool) -> RpcResult<Option<Block>>;

    #[method(name = "eth_getLogs", blocking)]
    fn logs(&self, filter: Filter) -> RpcResult<Vec<Log>>;
}

pub struct EthRpc {
    node: NodeHandle,
}

impl EthRpc {
    pub fn new(node: NodeHandle) -> Self {
        Self { node }
    }
}

fn rpc_error(code: i32, message: impl Into<String>) -> ErrorObjectOwned {
    ErrorObject::owned(code, message.into(), None::<()>)
}

fn invalid_params(message: impl Into<String>) -> ErrorObjectOwned {
    rpc_error(-32602, message)
}

fn server_error(message: impl Into<String>) -> ErrorObjectOwned {
    rpc_error(-32000, message)
}

fn call_error(error: CallError) -> ErrorObjectOwned {
    match error {
        CallError::Reverted(data) => {
            let message = match convert::revert_reason(&data) {
                Some(reason) => format!("execution reverted: {reason}"),
                None => "execution reverted".to_string(),
            };

            ErrorObject::owned(3, message, Some(hex::encode_prefixed(&data)))
        }
        other => server_error(other.to_string()),
    }
}

fn head(core: &NodeCore) -> u64 {
    core.head_height().unwrap_or(0)
}

/// Solizone keeps only the latest state, so state queries accept
/// latest/pending (and the head height or hash) but not historical blocks.
fn state_view(core: &NodeCore, block: Option<BlockId>) -> RpcResult<StateView> {
    let head = head(core);

    let historical = || {
        Err(invalid_params(
            "historical state is not available; query latest or pending",
        ))
    };

    match block.unwrap_or(BlockId::Number(BlockNumberOrTag::Latest)) {
        BlockId::Number(BlockNumberOrTag::Pending) => Ok(StateView::Pending),
        BlockId::Number(
            BlockNumberOrTag::Latest | BlockNumberOrTag::Safe | BlockNumberOrTag::Finalized,
        ) => Ok(StateView::Latest),
        BlockId::Number(BlockNumberOrTag::Earliest) if head == 0 => Ok(StateView::Latest),
        BlockId::Number(BlockNumberOrTag::Number(number)) if number == head => {
            Ok(StateView::Latest)
        }
        BlockId::Hash(hash)
            if core
                .cached_block_by_height(head)
                .is_some_and(|block| block.hash() == hash.block_hash) =>
        {
            Ok(StateView::Latest)
        }
        _ => historical(),
    }
}

fn resolve_number(core: &NodeCore, tag: BlockNumberOrTag) -> u64 {
    match tag {
        BlockNumberOrTag::Number(number) => number,
        BlockNumberOrTag::Earliest => 0,
        _ => head(core),
    }
}

fn call_request(request: TransactionRequest) -> CallRequest {
    let to = match request.to {
        Some(TxKind::Call(to)) => Some(to),
        Some(TxKind::Create) | None => None,
    };

    CallRequest {
        from: request.from,
        to,
        value: request.value.unwrap_or_default(),
        data: request.input.input().cloned().unwrap_or_default(),
        gas: request.gas,
    }
}

fn reject_overrides(overrides: &Option<serde_json::Value>) -> RpcResult<()> {
    match overrides {
        None | Some(serde_json::Value::Null) => Ok(()),
        Some(_) => Err(invalid_params("state overrides are not supported")),
    }
}

impl EthRpc {
    fn block_view(&self, block: Option<crate::block::SolizoneBlock>, full: bool) -> Option<Block> {
        let block = block?;
        let core = self.node.core();
        let records = core.block_transaction_records(block.header.height);

        Some(convert::rpc_block(&block, records.as_deref(), full))
    }
}

impl EthApiServer for EthRpc {
    fn client_version(&self) -> RpcResult<String> {
        Ok(CLIENT_VERSION.to_string())
    }

    fn net_version(&self) -> RpcResult<String> {
        Ok(self.node.core().chain_id().to_string())
    }

    fn net_listening(&self) -> RpcResult<bool> {
        Ok(true)
    }

    fn chain_id(&self) -> RpcResult<U64> {
        Ok(U64::from(self.node.core().chain_id()))
    }

    fn block_number(&self) -> RpcResult<U64> {
        Ok(U64::from(head(&self.node.core())))
    }

    fn syncing(&self) -> RpcResult<bool> {
        Ok(false)
    }

    fn accounts(&self) -> RpcResult<Vec<Address>> {
        Ok(Vec::new())
    }

    // Solizone does not charge gas fees yet: every fee quantity is zero.

    fn gas_price(&self) -> RpcResult<U256> {
        Ok(U256::ZERO)
    }

    fn max_priority_fee_per_gas(&self) -> RpcResult<U256> {
        Ok(U256::ZERO)
    }

    fn fee_history(
        &self,
        block_count: U64,
        newest_block: BlockNumberOrTag,
        reward_percentiles: Option<Vec<f64>>,
    ) -> RpcResult<FeeHistory> {
        let core = self.node.core();
        let newest = resolve_number(&core, newest_block).min(head(&core));
        let count = block_count.to::<u64>().clamp(1, 1024).min(newest + 1);
        let oldest = newest + 1 - count;
        let blocks = count as usize;

        Ok(FeeHistory {
            base_fee_per_gas: vec![0; blocks + 1],
            gas_used_ratio: vec![0.0; blocks],
            base_fee_per_blob_gas: vec![0; blocks + 1],
            blob_gas_used_ratio: vec![0.0; blocks],
            oldest_block: oldest,
            reward: reward_percentiles.map(|percentiles| vec![vec![0; percentiles.len()]; blocks]),
        })
    }

    fn balance(&self, address: Address, block: Option<BlockId>) -> RpcResult<U256> {
        let core = self.node.core();
        let view = state_view(&core, block)?;
        Ok(core.balance(&address, view))
    }

    fn transaction_count(&self, address: Address, block: Option<BlockId>) -> RpcResult<U64> {
        let core = self.node.core();
        let view = state_view(&core, block)?;
        Ok(U64::from(core.nonce(&address, view)))
    }

    fn code(&self, address: Address, block: Option<BlockId>) -> RpcResult<Bytes> {
        let core = self.node.core();
        let view = state_view(&core, block)?;
        Ok(core.code(&address, view))
    }

    fn storage_at(&self, address: Address, slot: U256, block: Option<BlockId>) -> RpcResult<B256> {
        let core = self.node.core();
        let view = state_view(&core, block)?;
        Ok(B256::from(core.storage_at(&address, slot, view)))
    }

    fn call(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        state_overrides: Option<serde_json::Value>,
    ) -> RpcResult<Bytes> {
        reject_overrides(&state_overrides)?;

        let mut core = self.node.core();
        let view = state_view(&core, block)?;

        core.call(&call_request(request), view, unix_now())
            .map_err(call_error)
    }

    fn estimate_gas(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        state_overrides: Option<serde_json::Value>,
    ) -> RpcResult<U64> {
        reject_overrides(&state_overrides)?;

        let mut core = self.node.core();

        // Estimates default to the pending state so dependent transactions
        // (deploy, then call) estimate correctly before the next block.
        let view = match block {
            None => StateView::Pending,
            Some(block) => state_view(&core, Some(block))?,
        };

        core.estimate_gas(&call_request(request), view, unix_now())
            .map(U64::from)
            .map_err(call_error)
    }

    fn send_raw_transaction(&self, raw: Bytes) -> RpcResult<B256> {
        self.node
            .core()
            .submit_raw_transaction(&raw, unix_now())
            .map_err(|error| server_error(error.to_string()))
    }

    fn transaction_by_hash(&self, hash: B256) -> RpcResult<Option<Transaction>> {
        let core = self.node.core();

        if let Some(record) = core.transaction(&hash) {
            return Ok(convert::rpc_transaction(record));
        }

        Ok(core
            .pending_transaction(&hash)
            .and_then(convert::rpc_pending_transaction))
    }

    fn transaction_receipt(&self, hash: B256) -> RpcResult<Option<TransactionReceipt>> {
        Ok(self
            .node
            .core()
            .transaction(&hash)
            .and_then(convert::rpc_receipt))
    }

    fn block_by_number(&self, number: BlockNumberOrTag, full: bool) -> RpcResult<Option<Block>> {
        let height = resolve_number(&self.node.core(), number);

        let block = self.node.block_by_height(height).map_err(server_error)?;

        Ok(self.block_view(block, full))
    }

    fn block_by_hash(&self, hash: B256, full: bool) -> RpcResult<Option<Block>> {
        let block = self.node.block_by_hash(hash).map_err(server_error)?;

        Ok(self.block_view(block, full))
    }

    fn logs(&self, filter: Filter) -> RpcResult<Vec<Log>> {
        let (from, to) = {
            let core = self.node.core();

            match filter.block_option {
                FilterBlockOption::Range {
                    from_block,
                    to_block,
                } => (
                    resolve_number(&core, from_block.unwrap_or(BlockNumberOrTag::Latest)),
                    resolve_number(&core, to_block.unwrap_or(BlockNumberOrTag::Latest))
                        .min(head(&core)),
                ),
                FilterBlockOption::AtBlockHash(hash) => {
                    let height = core
                        .cached_block_by_hash(&hash)
                        .map(|block| block.header.height)
                        .ok_or_else(|| invalid_params("unknown block hash"))?;
                    (height, height)
                }
            }
        };

        if from > to {
            return Ok(Vec::new());
        }

        if to - from >= MAX_LOG_RANGE {
            return Err(invalid_params(format!(
                "block range too large; maximum is {MAX_LOG_RANGE} blocks"
            )));
        }

        let core = self.node.core();
        let mut logs = Vec::new();

        for height in from..=to {
            let Some(records) = core.block_transaction_records(height) else {
                continue;
            };

            for record in records {
                logs.extend(
                    convert::rpc_logs(record)
                        .into_iter()
                        .filter(|log| filter.matches(&log.inner)),
                );
            }
        }

        Ok(logs)
    }
}
