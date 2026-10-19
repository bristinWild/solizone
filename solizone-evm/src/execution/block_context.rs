use revm::primitives::Address;

/// Block-level environment exposed to contracts during execution.
///
/// This is what Solidity sees through `block.chainid`, `block.number`,
/// `block.timestamp`, `block.gaslimit`, `block.coinbase` and `block.basefee`.
///
/// `BlockProducer` derives it from its own chain configuration and the
/// height / timestamp of the block being produced, so contract-visible
/// values always match the canonical Solizone block header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockContext {
    pub chain_id: u64,
    pub number: u64,
    pub timestamp: u64,
    pub gas_limit: u64,
    pub beneficiary: Address,
    pub basefee: u64,
}

impl BlockContext {
    pub fn new(chain_id: u64, number: u64, timestamp: u64, gas_limit: u64) -> Self {
        Self {
            chain_id,
            number,
            timestamp,
            gas_limit,
            beneficiary: Address::ZERO,
            basefee: 0,
        }
    }
}

impl Default for BlockContext {
    /// Matches REVM's mainnet defaults (chain id 1, block 0, timestamp 1,
    /// unbounded gas). Only the legacy context-free helpers use this;
    /// node block production always supplies an explicit context.
    fn default() -> Self {
        Self {
            chain_id: 1,
            number: 0,
            timestamp: 1,
            gas_limit: u64::MAX,
            beneficiary: Address::ZERO,
            basefee: 0,
        }
    }
}
