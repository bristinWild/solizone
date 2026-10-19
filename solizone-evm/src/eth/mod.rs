//! Ethereum compatibility layer: signed transaction ingestion and
//! Ethereum-shaped views of Solizone data.

pub mod transaction;

#[cfg(test)]
pub(crate) mod test_utils;

pub use transaction::{EthTransactionError, SignedEthTransaction};
