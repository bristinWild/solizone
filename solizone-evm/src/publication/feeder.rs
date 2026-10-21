//! Submits blocks to a sink strictly in height order, resuming from the
//! persistent cursor.

use std::{future::Future, time::Duration};

use crate::{block::SolizoneBlock, node::NodeHandle};

use super::state::{PublicationStore, SharedState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    /// Temporary (not connected, not our turn, queue full): try again later.
    Unavailable(String),
    /// This payload can never be published (e.g. too large for an inscription).
    Rejected(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Submitted {
    /// Identifier of the Logos transaction carrying the block.
    pub tx_hash: String,

    /// Sequencer checkpoint including the new pending transaction.
    pub checkpoint: Option<serde_json::Value>,
}

/// Where encoded blocks go.
pub trait InscriptionSink: Send + Sync {
    fn submit(
        &self,
        payload: Vec<u8>,
    ) -> impl Future<Output = Result<Submitted, SubmitError>> + Send;
}

/// Where blocks come from.
pub trait BlockSource: Send + Sync {
    fn head_height(&self) -> Option<u64>;

    fn block(&self, height: u64) -> Result<Option<SolizoneBlock>, String>;
}

impl BlockSource for NodeHandle {
    fn head_height(&self) -> Option<u64> {
        self.core().head_height()
    }

    fn block(&self, height: u64) -> Result<Option<SolizoneBlock>, String> {
        self.block_by_height(height)
    }
}

/// Refuses to continue when the stored cursor does not belong to this chain
/// (for example the chain was reset but the publication state was kept):
/// publishing would silently skip or mix blocks.
pub fn check_cursor<B: BlockSource>(
    state: &super::state::PublicationState,
    source: &B,
) -> Result<(), String> {
    if state.next_height == 0 {
        return Ok(());
    }

    let last = state.next_height - 1;

    let head = source.head_height();

    if head.is_none_or(|head| last > head) {
        return Err(format!(
            "publication state says block #{last} was already published but the chain head is \
             {}; the chain was probably reset. Reset the publication state too \
             (`solizone_node reset`)",
            head.map_or("empty".to_string(), |head| format!("#{head}"))
        ));
    }

    if let Some(entry) = state.entries.get(&last) {
        let block = source
            .block(last)?
            .ok_or_else(|| format!("block #{last} is missing from node storage"))?;

        if block.hash().to_string() != entry.block_hash {
            return Err(format!(
                "published block #{last} ({}) differs from this chain's block ({}); \
                 the stored publication state belongs to another chain",
                entry.block_hash,
                block.hash()
            ));
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
pub struct FeederConfig {
    /// Maximum blocks submitted to Logos but not yet finalized.
    pub max_in_flight: usize,
    pub poll_interval: Duration,
    pub retry_delay: Duration,
}

impl Default for FeederConfig {
    fn default() -> Self {
        Self {
            // The Zone SDK queues at most 10 pending publishes by default.
            max_in_flight: 8,
            poll_interval: Duration::from_millis(500),
            retry_delay: Duration::from_secs(3),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PumpOutcome {
    /// Every produced block has been submitted.
    CaughtUp,
    /// Waiting for earlier blocks to finalize before sending more.
    Backpressure,
    /// Temporary problem; try again after a delay.
    Retry(String),
    /// Publication stopped permanently; the reason is stored in the state.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PumpReport {
    pub submitted: usize,
    pub outcome: PumpOutcome,
}

fn lock(state: &SharedState) -> std::sync::MutexGuard<'_, super::state::PublicationState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fail(
    state: &SharedState,
    store: &PublicationStore,
    submitted: usize,
    reason: String,
) -> PumpReport {
    let mut guard = lock(state);
    guard.failed = Some(reason.clone());
    let _ = store.save(&guard);

    PumpReport {
        submitted,
        outcome: PumpOutcome::Failed(reason),
    }
}

/// Submits as many pending blocks as allowed, in order.
pub async fn pump<S, B>(
    sink: &S,
    source: &B,
    state: &SharedState,
    store: &PublicationStore,
    max_in_flight: usize,
) -> PumpReport
where
    S: InscriptionSink,
    B: BlockSource,
{
    let mut submitted = 0;

    loop {
        let next = {
            let guard = lock(state);

            if guard.failed.is_some() {
                return PumpReport {
                    submitted,
                    outcome: PumpOutcome::Failed(guard.failed.clone().unwrap_or_default()),
                };
            }

            if guard.in_flight() >= max_in_flight {
                return PumpReport {
                    submitted,
                    outcome: PumpOutcome::Backpressure,
                };
            }

            guard.next_height
        };

        let Some(head) = source.head_height() else {
            return PumpReport {
                submitted,
                outcome: PumpOutcome::CaughtUp,
            };
        };

        if next > head {
            return PumpReport {
                submitted,
                outcome: PumpOutcome::CaughtUp,
            };
        }

        let block = match source.block(next) {
            Ok(Some(block)) => block,
            Ok(None) => {
                return fail(
                    state,
                    store,
                    submitted,
                    format!("block #{next} is missing from node storage"),
                );
            }
            Err(error) => {
                return PumpReport {
                    submitted,
                    outcome: PumpOutcome::Retry(format!("failed to read block #{next}: {error}")),
                };
            }
        };

        if let Err(error) = block.validate() {
            return fail(
                state,
                store,
                submitted,
                format!("refusing to publish invalid block #{next}: {error:?}"),
            );
        }

        let block_hash = block.hash().to_string();

        match sink.submit(block.encode()).await {
            Ok(result) => {
                let mut guard = lock(state);

                guard.record_submission(next, block_hash, result.tx_hash, result.checkpoint);

                if let Err(error) = store.save(&guard) {
                    // The block is already with the sequencer; continuing
                    // without a durable cursor could publish it twice.
                    guard.failed = Some(error.clone());

                    return PumpReport {
                        submitted: submitted + 1,
                        outcome: PumpOutcome::Failed(error),
                    };
                }

                submitted += 1;
            }
            Err(SubmitError::Unavailable(reason)) => {
                return PumpReport {
                    submitted,
                    outcome: PumpOutcome::Retry(reason),
                };
            }
            Err(SubmitError::Rejected(reason)) => {
                return fail(
                    state,
                    store,
                    submitted,
                    format!("block #{next} cannot be published: {reason}"),
                );
            }
        }
    }
}

/// Runs [`pump`] forever (until publication fails permanently).
pub async fn run<S, B>(
    sink: S,
    source: B,
    state: SharedState,
    store: PublicationStore,
    config: FeederConfig,
) where
    S: InscriptionSink,
    B: BlockSource,
{
    loop {
        let report = pump(&sink, &source, &state, &store, config.max_in_flight).await;

        if report.submitted > 0 {
            let guard = lock(&state);
            println!(
                "published {} block(s) to Logos; next height #{}, {} in flight",
                report.submitted,
                guard.next_height,
                guard.in_flight()
            );
        }

        match report.outcome {
            PumpOutcome::CaughtUp | PumpOutcome::Backpressure => {
                tokio::time::sleep(config.poll_interval).await;
            }
            PumpOutcome::Retry(reason) => {
                eprintln!("publication waiting: {reason}");
                tokio::time::sleep(config.retry_delay).await;
            }
            PumpOutcome::Failed(reason) => {
                eprintln!("publication stopped: {reason}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use revm::primitives::{Address, U256, address};

    use super::*;
    use crate::{
        eth::test_utils::{TxParams, dev_signer, sign_eip1559},
        node::{
            NodeConfig, NodeCore,
            dev_accounts::{DEV_ACCOUNTS, dev_account_balance},
        },
        publication::state::{PublicationState, PublicationStatus},
    };

    const NOW: u64 = 1_800_000_000;
    const BOB: Address = address!("2222222222222222222222222222222222222222");

    /// A chain of `blocks` produced blocks plus genesis.
    fn chain(blocks: u64) -> Vec<SolizoneBlock> {
        let genesis: Vec<(Address, U256)> = DEV_ACCOUNTS
            .iter()
            .map(|account| (account.address, dev_account_balance()))
            .collect();

        let (mut node, genesis_commit) = NodeCore::new(NodeConfig::default(), &genesis, NOW);
        let mut chain = vec![genesis_commit.block];

        for nonce in 0..blocks {
            let raw = sign_eip1559(&dev_signer(0), TxParams::transfer(9001, nonce, BOB, 1));
            node.submit_raw_transaction(&raw, NOW + nonce).unwrap();
            chain.push(node.produce_block(NOW + nonce).unwrap().block);
        }

        chain
    }

    struct Source(Vec<SolizoneBlock>);

    impl BlockSource for Source {
        fn head_height(&self) -> Option<u64> {
            self.0.len().checked_sub(1).map(|h| h as u64)
        }

        fn block(&self, height: u64) -> Result<Option<SolizoneBlock>, String> {
            Ok(self.0.get(height as usize).cloned())
        }
    }

    /// Sink with scripted responses; records every payload it accepts.
    struct Sink {
        sent: Mutex<Vec<Vec<u8>>>,
        script: Mutex<VecDeque<Result<(), SubmitError>>>,
    }

    impl Sink {
        fn new(script: Vec<Result<(), SubmitError>>) -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                script: Mutex::new(script.into()),
            }
        }

        fn sent(&self) -> Vec<Vec<u8>> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl InscriptionSink for Sink {
        async fn submit(&self, payload: Vec<u8>) -> Result<Submitted, SubmitError> {
            let next = self.script.lock().unwrap().pop_front().unwrap_or(Ok(()));

            next?;

            let mut sent = self.sent.lock().unwrap();
            sent.push(payload);

            Ok(Submitted {
                tx_hash: format!("tx{}", sent.len() - 1),
                checkpoint: Some(serde_json::json!({ "sent": sent.len() })),
            })
        }
    }

    fn setup(name: &str) -> (SharedState, PublicationStore) {
        let path = std::env::temp_dir()
            .join(format!("solizone-feeder-{name}-{}", std::process::id()))
            .join("publication.json");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        (
            Arc::new(Mutex::new(PublicationState::default())),
            PublicationStore::new(path),
        )
    }

    fn cleanup(store: &PublicationStore) {
        let _ = std::fs::remove_dir_all(store.path().parent().unwrap());
    }

    #[tokio::test]
    async fn publishes_every_block_in_order() {
        let (state, store) = setup("order");
        let blocks = chain(2);
        let sink = Sink::new(vec![]);

        let report = pump(&sink, &Source(blocks.clone()), &state, &store, 8).await;

        assert_eq!(report.submitted, 3);
        assert_eq!(report.outcome, PumpOutcome::CaughtUp);

        let expected: Vec<Vec<u8>> = blocks.iter().map(|block| block.encode()).collect();
        assert_eq!(sink.sent(), expected);

        {
            let guard = state.lock().unwrap();
            assert_eq!(guard.next_height, 3);
            assert_eq!(guard.entries[&1].block_hash, blocks[1].hash().to_string());
            assert_eq!(guard.entries[&2].tx_hash, "tx2");
            assert_eq!(
                guard.sequencer_checkpoint,
                Some(serde_json::json!({"sent": 3}))
            );
        }

        // A second pump has nothing to do.
        let again = pump(&sink, &Source(blocks), &state, &store, 8).await;
        assert_eq!(again.submitted, 0);
        assert_eq!(again.outcome, PumpOutcome::CaughtUp);

        cleanup(&store);
    }

    #[tokio::test]
    async fn resumes_from_stored_cursor_after_restart() {
        let (state, store) = setup("resume");
        let blocks = chain(3);

        // First run publishes only blocks 0 and 1.
        let first = Sink::new(vec![]);
        pump(&first, &Source(blocks[..2].to_vec()), &state, &store, 8).await;
        assert_eq!(first.sent().len(), 2);

        // "Restart": reload state from disk; the chain has grown.
        let reloaded: SharedState = Arc::new(Mutex::new(store.load().unwrap()));
        let second = Sink::new(vec![]);
        let report = pump(&second, &Source(blocks.clone()), &reloaded, &store, 8).await;

        assert_eq!(report.submitted, 2);
        assert_eq!(second.sent()[0], blocks[2].encode());
        assert_eq!(second.sent()[1], blocks[3].encode());
        assert_eq!(reloaded.lock().unwrap().next_height, 4);

        cleanup(&store);
    }

    #[tokio::test]
    async fn retries_when_sink_unavailable_without_skipping() {
        let (state, store) = setup("retry");
        let blocks = chain(1);
        let sink = Sink::new(vec![Err(SubmitError::Unavailable("not ready".into()))]);

        let first = pump(&sink, &Source(blocks.clone()), &state, &store, 8).await;
        assert_eq!(first.submitted, 0);
        assert_eq!(first.outcome, PumpOutcome::Retry("not ready".into()));
        assert_eq!(state.lock().unwrap().next_height, 0);

        let second = pump(&sink, &Source(blocks.clone()), &state, &store, 8).await;
        assert_eq!(second.submitted, 2);
        assert_eq!(sink.sent()[0], blocks[0].encode());

        cleanup(&store);
    }

    #[tokio::test]
    async fn rejected_block_stops_publication_permanently() {
        let (state, store) = setup("rejected");
        let blocks = chain(2);
        let sink = Sink::new(vec![Ok(()), Err(SubmitError::Rejected("too large".into()))]);

        let report = pump(&sink, &Source(blocks.clone()), &state, &store, 8).await;

        assert_eq!(report.submitted, 1);
        assert!(matches!(report.outcome, PumpOutcome::Failed(ref r) if r.contains("block #1")));

        // The failure is persisted and sticks across restarts.
        let reloaded: SharedState = Arc::new(Mutex::new(store.load().unwrap()));
        assert!(reloaded.lock().unwrap().failed.is_some());

        let after = pump(&Sink::new(vec![]), &Source(blocks), &reloaded, &store, 8).await;
        assert_eq!(after.submitted, 0);
        assert!(matches!(after.outcome, PumpOutcome::Failed(_)));

        cleanup(&store);
    }

    #[tokio::test]
    async fn limits_blocks_in_flight() {
        let (state, store) = setup("backpressure");
        let blocks = chain(4);
        let sink = Sink::new(vec![]);

        let first = pump(&sink, &Source(blocks.clone()), &state, &store, 2).await;
        assert_eq!(first.submitted, 2);
        assert_eq!(first.outcome, PumpOutcome::Backpressure);

        // Finalizing the first block frees a slot.
        state
            .lock()
            .unwrap()
            .set_status("tx0", PublicationStatus::Finalized);

        let second = pump(&sink, &Source(blocks), &state, &store, 2).await;
        assert_eq!(second.submitted, 1);
        assert_eq!(second.outcome, PumpOutcome::Backpressure);

        cleanup(&store);
    }

    #[tokio::test]
    async fn missing_block_is_a_permanent_failure() {
        let (state, store) = setup("missing");
        let blocks = chain(1);

        struct Gappy(Vec<SolizoneBlock>);
        impl BlockSource for Gappy {
            fn head_height(&self) -> Option<u64> {
                Some(5)
            }
            fn block(&self, height: u64) -> Result<Option<SolizoneBlock>, String> {
                Ok(self.0.get(height as usize).cloned())
            }
        }

        let report = pump(&Sink::new(vec![]), &Gappy(blocks), &state, &store, 8).await;

        assert_eq!(report.submitted, 2);
        assert!(matches!(report.outcome, PumpOutcome::Failed(ref r) if r.contains("#2")));

        cleanup(&store);
    }

    #[test]
    fn cursor_check_detects_a_reset_chain() {
        let blocks = chain(2);
        let source = Source(blocks.clone());

        // Fresh state is always fine.
        assert!(check_cursor(&PublicationState::default(), &source).is_ok());

        // A cursor matching this chain is fine.
        let mut state = PublicationState::default();
        state.record_submission(2, blocks[2].hash().to_string(), "t2".into(), None);
        assert!(check_cursor(&state, &source).is_ok());

        // Cursor ahead of the chain (chain was reset).
        let mut ahead = PublicationState::default();
        ahead.record_submission(9, "h9".into(), "t9".into(), None);
        assert!(check_cursor(&ahead, &source).unwrap_err().contains("reset"));

        // Same height, different block (another chain).
        let mut other = PublicationState::default();
        other.record_submission(1, "0xdeadbeef".into(), "t1".into(), None);
        assert!(
            check_cursor(&other, &source)
                .unwrap_err()
                .contains("another chain")
        );

        // Empty chain with a non-zero cursor.
        assert!(check_cursor(&state, &Source(vec![])).is_err());
    }

    #[tokio::test]
    async fn empty_chain_is_caught_up() {
        let (state, store) = setup("empty");

        let report = pump(&Sink::new(vec![]), &Source(vec![]), &state, &store, 8).await;

        assert_eq!(report.outcome, PumpOutcome::CaughtUp);

        cleanup(&store);
    }
}
