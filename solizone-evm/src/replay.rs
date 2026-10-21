//! Deterministic chain replay.
//!
//! Re-executes published blocks on top of a known genesis and checks that the
//! result is exactly what the block claims: same transactions, same state
//! root, same receipts root, same block hash. This is what makes a chain read
//! back from Logos *verifiable*: a block is accepted only if executing it
//! reproduces it bit for bit.
//!
//! Replay never trusts a block's own header. Every field is recomputed.

use revm::primitives::{Address, B256, U256};
use revm::state::AccountInfo;

use crate::{
    block::{BlockValidationError, SolizoneBlock},
    block_builder::build_block,
    execution::{BlockContext, RevmExecutionEngine, receipt::ExecutionReceipt},
    node::core::NodeConfig,
    state::{MemoryState, SolizoneCheckpoint},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// Blocks must arrive in order, one after another.
    UnexpectedHeight { expected: u64, got: u64 },
    ParentMismatch {
        height: u64,
        expected: B256,
        got: B256,
    },
    WrongChain {
        height: u64,
        expected: u64,
        got: u64,
    },
    WrongVersion {
        height: u64,
        expected: u16,
        got: u16,
    },
    WrongGasLimit {
        height: u64,
        expected: u64,
        got: u64,
    },
    /// The block's own structure is invalid (transactions root, chain id).
    Invalid {
        height: u64,
        reason: BlockValidationError,
    },
    /// The block carries a transaction a valid block could not include.
    TransactionRejected { height: u64, index: usize },
    /// Executing the block did not reproduce it (wrong state root, receipts
    /// root, gas used or timestamp-dependent result).
    HashMismatch {
        height: u64,
        claimed: B256,
        computed: B256,
    },
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedHeight { expected, got } => {
                write!(f, "expected block #{expected}, got #{got}")
            }
            Self::ParentMismatch {
                height,
                expected,
                got,
            } => write!(
                f,
                "block #{height} has parent {got:#x}, expected {expected:#x}"
            ),
            Self::WrongChain {
                height,
                expected,
                got,
            } => write!(f, "block #{height} is for chain {got}, expected {expected}"),
            Self::WrongVersion {
                height,
                expected,
                got,
            } => write!(f, "block #{height} has version {got}, expected {expected}"),
            Self::WrongGasLimit {
                height,
                expected,
                got,
            } => write!(
                f,
                "block #{height} has gas limit {got}, expected {expected}"
            ),
            Self::Invalid { height, reason } => {
                write!(f, "block #{height} is invalid: {reason:?}")
            }
            Self::TransactionRejected { height, index } => {
                write!(f, "block #{height}: transaction {index} cannot be executed")
            }
            Self::HashMismatch {
                height,
                claimed,
                computed,
            } => write!(
                f,
                "block #{height} does not replay: claims {claimed:#x}, executing it gives {computed:#x}"
            ),
        }
    }
}

impl std::error::Error for ReplayError {}

/// Replays blocks in order against an in-memory state.
pub struct ChainReplayer {
    config: NodeConfig,
    engine: RevmExecutionEngine,
    state: MemoryState,
    next_height: u64,
    parent_hash: B256,
}

impl ChainReplayer {
    /// A chain that has not produced any block yet, starting from the given
    /// genesis balances. The first block to apply is the genesis block #0.
    pub fn from_genesis(config: &NodeConfig, genesis: &[(Address, U256)]) -> Self {
        let mut state = MemoryState::new();

        for (address, balance) in genesis {
            state.insert_account_info(*address, AccountInfo::from_balance(*balance));
        }

        Self {
            config: config.clone(),
            engine: RevmExecutionEngine::new(),
            state,
            next_height: 0,
            parent_hash: B256::ZERO,
        }
    }

    /// Continues from a stored checkpoint.
    pub fn from_checkpoint(config: &NodeConfig, checkpoint: SolizoneCheckpoint) -> Self {
        Self {
            config: config.clone(),
            engine: RevmExecutionEngine::new(),
            state: MemoryState::from_snapshot(checkpoint.state),
            next_height: checkpoint.next_height,
            parent_hash: checkpoint.parent_hash,
        }
    }

    pub fn next_height(&self) -> u64 {
        self.next_height
    }

    pub fn parent_hash(&self) -> B256 {
        self.parent_hash
    }

    /// The checkpoint after the last applied block, in the form the node
    /// stores and resumes from.
    pub fn checkpoint(&self) -> SolizoneCheckpoint {
        SolizoneCheckpoint::new(self.state.snapshot(), self.next_height, self.parent_hash)
    }

    /// Verifies `block` by executing it. On success the replayer advances
    /// and returns the block's receipts; on error nothing changes.
    pub fn apply(&mut self, block: &SolizoneBlock) -> Result<Vec<ExecutionReceipt>, ReplayError> {
        let header = &block.header;
        let height = header.height;

        if height != self.next_height {
            return Err(ReplayError::UnexpectedHeight {
                expected: self.next_height,
                got: height,
            });
        }

        if header.parent_hash != self.parent_hash {
            return Err(ReplayError::ParentMismatch {
                height,
                expected: self.parent_hash,
                got: header.parent_hash,
            });
        }

        if header.chain_id != self.config.chain_id {
            return Err(ReplayError::WrongChain {
                height,
                expected: self.config.chain_id,
                got: header.chain_id,
            });
        }

        if header.version != self.config.version {
            return Err(ReplayError::WrongVersion {
                height,
                expected: self.config.version,
                got: header.version,
            });
        }

        if header.gas_limit != self.config.block_gas_limit {
            return Err(ReplayError::WrongGasLimit {
                height,
                expected: self.config.block_gas_limit,
                got: header.gas_limit,
            });
        }

        block
            .validate()
            .map_err(|reason| ReplayError::Invalid { height, reason })?;

        // Execute on a copy so a bad block leaves the replayer untouched.
        let mut state = self.state.clone();

        let ctx = BlockContext::new(header.chain_id, height, header.timestamp, header.gas_limit);

        let outcome = self
            .engine
            .execute_block_checked(&ctx, &mut state, &block.transactions);

        if let Some(rejected) = outcome.rejected.first() {
            let index = block
                .transactions
                .iter()
                .position(|tx| *tx == rejected.transaction)
                .unwrap_or(0);

            return Err(ReplayError::TransactionRejected { height, index });
        }

        let rebuilt = build_block(
            header.version,
            header.chain_id,
            height,
            header.parent_hash,
            header.timestamp,
            outcome.state_root,
            &outcome.included,
            &outcome.receipts,
            header.gas_limit,
        );

        if rebuilt.hash() != block.hash() {
            return Err(ReplayError::HashMismatch {
                height,
                claimed: block.hash(),
                computed: rebuilt.hash(),
            });
        }

        self.state = state;
        self.next_height = height + 1;
        self.parent_hash = block.hash();

        Ok(outcome.receipts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        eth::test_utils::{TxParams, dev_signer, sign_eip1559},
        node::{
            core::{BlockCommit, NodeCore},
            dev_accounts::{DEV_ACCOUNTS, dev_account_balance},
        },
    };
    use revm::primitives::address;

    const NOW: u64 = 1_800_000_000;
    const BOB: Address = address!("2222222222222222222222222222222222222222");

    fn genesis() -> Vec<(Address, U256)> {
        DEV_ACCOUNTS
            .iter()
            .map(|account| (account.address, dev_account_balance()))
            .collect()
    }

    /// A chain with genesis plus `transfers` single-transfer blocks.
    fn chain(transfers: u64) -> Vec<BlockCommit> {
        let (mut node, genesis_commit) = NodeCore::new(NodeConfig::default(), &genesis(), NOW);
        let mut commits = vec![genesis_commit];

        for nonce in 0..transfers {
            let raw = sign_eip1559(
                &dev_signer(0),
                TxParams::transfer(9001, nonce, BOB, 1_000 + nonce),
            );
            node.submit_raw_transaction(&raw, NOW + nonce).unwrap();
            commits.push(node.produce_block(NOW + nonce + 1).expect("block"));
        }

        commits
    }

    fn replayer() -> ChainReplayer {
        ChainReplayer::from_genesis(&NodeConfig::default(), &genesis())
    }

    #[test]
    fn replays_a_produced_chain_exactly() {
        let commits = chain(4);
        let mut replay = replayer();

        for commit in &commits {
            let receipts = replay.apply(&commit.block).expect("replays");

            assert_eq!(receipts, commit.receipts);
            assert_eq!(replay.checkpoint(), commit.checkpoint);
        }

        assert_eq!(replay.next_height(), 5);
    }

    #[test]
    fn replays_through_the_wire_encoding() {
        let commits = chain(2);
        let mut replay = replayer();

        for commit in &commits {
            let decoded = SolizoneBlock::decode(&commit.block.encode()).unwrap();

            replay.apply(&decoded).expect("replays");
        }

        assert_eq!(replay.checkpoint(), commits.last().unwrap().checkpoint);
    }

    #[test]
    fn rejects_out_of_order_blocks() {
        let commits = chain(2);
        let mut replay = replayer();

        assert_eq!(
            replay.apply(&commits[1].block).unwrap_err(),
            ReplayError::UnexpectedHeight {
                expected: 0,
                got: 1
            }
        );

        replay.apply(&commits[0].block).unwrap();

        // Repeating a block is also out of order.
        assert_eq!(
            replay.apply(&commits[0].block).unwrap_err(),
            ReplayError::UnexpectedHeight {
                expected: 1,
                got: 0
            }
        );
    }

    #[test]
    fn rejects_a_wrong_parent() {
        let commits = chain(1);
        let mut replay = replayer();
        replay.apply(&commits[0].block).unwrap();

        let mut forged = commits[1].block.clone();
        forged.header.parent_hash = B256::repeat_byte(7);

        assert!(matches!(
            replay.apply(&forged),
            Err(ReplayError::ParentMismatch { height: 1, .. })
        ));
    }

    #[test]
    fn rejects_a_forged_state_root() {
        let commits = chain(1);
        let mut replay = replayer();
        replay.apply(&commits[0].block).unwrap();

        let mut forged = commits[1].block.clone();
        forged.header.state_root = B256::repeat_byte(9);

        assert!(matches!(
            replay.apply(&forged),
            Err(ReplayError::HashMismatch { height: 1, .. })
        ));
    }

    #[test]
    fn rejects_wrong_chain_id_version_and_gas_limit() {
        let commits = chain(1);
        let mut replay = replayer();
        replay.apply(&commits[0].block).unwrap();

        let mut wrong_chain = commits[1].block.clone();
        wrong_chain.header.chain_id = 1;
        assert!(matches!(
            replay.apply(&wrong_chain),
            Err(ReplayError::WrongChain { .. })
        ));

        let mut wrong_version = commits[1].block.clone();
        wrong_version.header.version = 2;
        assert!(matches!(
            replay.apply(&wrong_version),
            Err(ReplayError::WrongVersion { .. })
        ));

        let mut wrong_gas = commits[1].block.clone();
        wrong_gas.header.gas_limit = 1;
        assert!(matches!(
            replay.apply(&wrong_gas),
            Err(ReplayError::WrongGasLimit { .. })
        ));
    }

    #[test]
    fn rejects_a_replaced_transaction() {
        let commits = chain(2);
        let mut replay = replayer();
        replay.apply(&commits[0].block).unwrap();

        // Block #1's header with block #2's transaction: transactions root
        // no longer matches.
        let mut forged = commits[1].block.clone();
        forged.transactions = commits[2].block.transactions.clone();

        assert!(matches!(
            replay.apply(&forged),
            Err(ReplayError::Invalid { height: 1, .. })
        ));
    }

    #[test]
    fn a_failed_block_leaves_the_replayer_untouched() {
        let commits = chain(2);
        let mut replay = replayer();
        replay.apply(&commits[0].block).unwrap();

        let before = replay.checkpoint();

        let mut forged = commits[1].block.clone();
        forged.header.state_root = B256::repeat_byte(1);
        assert!(replay.apply(&forged).is_err());

        assert_eq!(replay.checkpoint(), before);

        // The genuine block still applies afterwards.
        replay.apply(&commits[1].block).unwrap();
        replay.apply(&commits[2].block).unwrap();
    }

    #[test]
    fn resumes_from_a_checkpoint() {
        let commits = chain(3);

        let mut replay =
            ChainReplayer::from_checkpoint(&NodeConfig::default(), commits[1].checkpoint.clone());

        assert_eq!(replay.next_height(), 2);

        replay.apply(&commits[2].block).unwrap();
        replay.apply(&commits[3].block).unwrap();

        assert_eq!(replay.checkpoint(), commits[3].checkpoint);
    }

    #[test]
    fn a_different_genesis_does_not_replay() {
        let commits = chain(0);
        let mut replay = ChainReplayer::from_genesis(
            &NodeConfig::default(),
            &[(DEV_ACCOUNTS[0].address, U256::from(1u64))],
        );

        assert!(matches!(
            replay.apply(&commits[0].block),
            Err(ReplayError::HashMismatch { height: 0, .. })
        ));
    }
}
