//! Solizone transaction pool.
//!
//! Distinct from the Logos mempool: this holds signed Ethereum transactions
//! waiting to be executed into a Solizone block. Ordering is FIFO by arrival;
//! the node only admits a transaction after it executes successfully against
//! the pending state, so per-sender nonces in the pool are always contiguous
//! and FIFO order is also valid nonce order.

use std::collections::{HashMap, VecDeque};

use revm::primitives::{Address, B256};

use crate::execution::transaction::SolizoneTransaction;

#[derive(Debug, Clone)]
pub struct PendingTransaction {
    pub hash: B256,
    pub tx: SolizoneTransaction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MempoolError {
    AlreadyKnown,
    Full {
        capacity: usize,
    },
    /// A different transaction with the same sender and nonce is pending.
    /// Fee-based replacement is not supported yet.
    ReplacementNotSupported,
}

impl std::fmt::Display for MempoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyKnown => write!(f, "already known"),
            Self::Full { capacity } => write!(f, "txpool is full ({capacity} transactions)"),
            Self::ReplacementNotSupported => {
                write!(f, "replacement transaction not supported")
            }
        }
    }
}

impl std::error::Error for MempoolError {}

#[derive(Debug)]
pub struct Mempool {
    capacity: usize,
    order: VecDeque<B256>,
    transactions: HashMap<B256, PendingTransaction>,
    by_sender_nonce: HashMap<(Address, u64), B256>,
    per_sender: HashMap<Address, u64>,
}

impl Mempool {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            order: VecDeque::new(),
            transactions: HashMap::new(),
            by_sender_nonce: HashMap::new(),
            per_sender: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn contains(&self, hash: &B256) -> bool {
        self.transactions.contains_key(hash)
    }

    pub fn get(&self, hash: &B256) -> Option<&PendingTransaction> {
        self.transactions.get(hash)
    }

    /// Number of pending transactions from `sender`.
    pub fn pending_count(&self, sender: &Address) -> u64 {
        self.per_sender.get(sender).copied().unwrap_or(0)
    }

    /// Checks whether `pending` could be inserted, without inserting it.
    pub fn check(&self, pending: &PendingTransaction) -> Result<(), MempoolError> {
        if self.transactions.contains_key(&pending.hash) {
            return Err(MempoolError::AlreadyKnown);
        }

        if self
            .by_sender_nonce
            .contains_key(&(pending.tx.sender, pending.tx.nonce))
        {
            return Err(MempoolError::ReplacementNotSupported);
        }

        if self.order.len() >= self.capacity {
            return Err(MempoolError::Full {
                capacity: self.capacity,
            });
        }

        Ok(())
    }

    pub fn insert(&mut self, pending: PendingTransaction) -> Result<(), MempoolError> {
        self.check(&pending)?;

        self.order.push_back(pending.hash);
        self.by_sender_nonce
            .insert((pending.tx.sender, pending.tx.nonce), pending.hash);
        *self.per_sender.entry(pending.tx.sender).or_insert(0) += 1;
        self.transactions.insert(pending.hash, pending);

        Ok(())
    }

    /// Removes and returns transactions from the front of the queue whose
    /// summed gas limits fit in `gas_limit`. Stops at the first transaction
    /// that does not fit, so FIFO (and per-sender nonce) order is preserved.
    pub fn take_for_block(&mut self, gas_limit: u64) -> Vec<PendingTransaction> {
        let mut taken = Vec::new();
        let mut gas = 0u64;

        while let Some(hash) = self.order.front() {
            let tx_gas = self.transactions[hash].tx.gas_limit;

            if gas.saturating_add(tx_gas) > gas_limit {
                break;
            }

            gas += tx_gas;

            let hash = self.order.pop_front().expect("front exists");
            taken.push(self.remove_entry(&hash));
        }

        taken
    }

    /// Drains every pending transaction in FIFO order.
    pub fn drain_all(&mut self) -> Vec<PendingTransaction> {
        let hashes: Vec<B256> = self.order.drain(..).collect();

        hashes.iter().map(|hash| self.remove_entry(hash)).collect()
    }

    fn remove_entry(&mut self, hash: &B256) -> PendingTransaction {
        let pending = self.transactions.remove(hash).expect("indexed transaction");

        self.by_sender_nonce
            .remove(&(pending.tx.sender, pending.tx.nonce));

        if let Some(count) = self.per_sender.get_mut(&pending.tx.sender) {
            *count -= 1;

            if *count == 0 {
                self.per_sender.remove(&pending.tx.sender);
            }
        }

        pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::transaction::TransactionKind;
    use revm::primitives::{Bytes, U256, address, keccak256};

    fn pending(sender: Address, nonce: u64, gas_limit: u64) -> PendingTransaction {
        let tx = SolizoneTransaction {
            sender,
            nonce,
            kind: TransactionKind::Call(address!("2222222222222222222222222222222222222222")),
            value: U256::from(1),
            data: Bytes::new(),
            gas_limit,
            signed: None,
        };

        PendingTransaction {
            hash: keccak256(tx.encode_canonical()),
            tx,
        }
    }

    const A: Address = address!("1111111111111111111111111111111111111111");
    const B: Address = address!("3333333333333333333333333333333333333333");

    #[test]
    fn keeps_fifo_order_and_counts() {
        let mut pool = Mempool::new(10);
        pool.insert(pending(A, 0, 21_000)).unwrap();
        pool.insert(pending(B, 0, 21_000)).unwrap();
        pool.insert(pending(A, 1, 21_000)).unwrap();

        assert_eq!(pool.pending_count(&A), 2);
        assert_eq!(pool.pending_count(&B), 1);

        let taken = pool.take_for_block(u64::MAX);
        let order: Vec<(Address, u64)> = taken.iter().map(|p| (p.tx.sender, p.tx.nonce)).collect();
        assert_eq!(order, vec![(A, 0), (B, 0), (A, 1)]);
        assert!(pool.is_empty());
        assert_eq!(pool.pending_count(&A), 0);
    }

    #[test]
    fn rejects_duplicates_replacements_and_overflow() {
        let mut pool = Mempool::new(2);
        let first = pending(A, 0, 21_000);
        pool.insert(first.clone()).unwrap();

        assert_eq!(pool.insert(first), Err(MempoolError::AlreadyKnown));
        assert_eq!(
            pool.insert(pending(A, 0, 30_000)),
            Err(MempoolError::ReplacementNotSupported)
        );

        pool.insert(pending(A, 1, 21_000)).unwrap();
        assert_eq!(
            pool.insert(pending(A, 2, 21_000)),
            Err(MempoolError::Full { capacity: 2 })
        );
    }

    #[test]
    fn takes_only_what_fits_in_block_gas() {
        let mut pool = Mempool::new(10);
        pool.insert(pending(A, 0, 21_000)).unwrap();
        pool.insert(pending(A, 1, 21_000)).unwrap();
        pool.insert(pending(A, 2, 21_000)).unwrap();

        let taken = pool.take_for_block(50_000);
        assert_eq!(taken.len(), 2);
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.pending_count(&A), 1);
    }
}
