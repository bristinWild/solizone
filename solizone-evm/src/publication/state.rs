//! Persistent publication state: the cursor, per-block Logos transaction and
//! status, and the Zone SDK sequencer checkpoint, in one atomically written
//! file so they can never disagree after a crash.

use std::{
    collections::{BTreeMap, HashSet},
    fs, io,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};

/// How far a block's Logos transaction has progressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublicationStatus {
    /// Accepted by the sequencer, not yet confirmed in the node mempool.
    Submitted,
    /// In the Logos node's mempool.
    Mempool,
    /// Included in a block on the canonical (non-finalized) chain.
    OnChain,
    /// Was on chain but the branch was reorganized away; the block is
    /// re-published (see `PublicationState::orphan`).
    Orphaned,
    /// Included in a finalized Logos block.
    Finalized,
}

impl PublicationStatus {
    /// Still waiting on Logos (counts against the in-flight limit).
    pub fn in_flight(self) -> bool {
        self != Self::Finalized
    }

    /// The sequencer's checkpoint still holds this transaction as pending.
    fn in_sequencer_pending_set(self) -> bool {
        matches!(self, Self::Submitted | Self::Mempool)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationEntry {
    pub block_hash: String,
    pub tx_hash: String,
    pub status: PublicationStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PublicationState {
    /// Height of the next block to submit (everything below was submitted).
    pub next_height: u64,

    /// Submitted blocks and their Logos transaction status. Old finalized
    /// entries are pruned.
    pub entries: BTreeMap<u64, PublicationEntry>,

    /// Set when publication stopped for good (e.g. a block too large for an
    /// inscription). The node keeps running; the reason is reported over RPC.
    pub failed: Option<String>,

    /// Zone SDK `SequencerCheckpoint` as JSON.
    pub sequencer_checkpoint: Option<serde_json::Value>,
}

/// Finalized entries kept for status reporting.
const KEEP_FINALIZED: usize = 256;

impl PublicationState {
    pub fn in_flight(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.status.in_flight())
            .count()
    }

    /// Records that `height` was handed to the sequencer.
    pub fn record_submission(
        &mut self,
        height: u64,
        block_hash: String,
        tx_hash: String,
        checkpoint: Option<serde_json::Value>,
    ) {
        self.entries.insert(
            height,
            PublicationEntry {
                block_hash,
                tx_hash,
                status: PublicationStatus::Submitted,
            },
        );

        self.next_height = height + 1;

        if checkpoint.is_some() {
            self.sequencer_checkpoint = checkpoint;
        }

        self.prune();
    }

    /// Updates the status of the block published as `tx_hash`. Returns the
    /// height if it is one of ours.
    pub fn set_status(&mut self, tx_hash: &str, status: PublicationStatus) -> Option<u64> {
        let (height, entry) = self
            .entries
            .iter_mut()
            .find(|(_, entry)| entry.tx_hash == tx_hash)?;

        entry.status = status;

        Some(*height)
    }

    /// Applies a sequencer checkpoint received from an event, unless it is
    /// older than a submission we already recorded: it would not list that
    /// still-pending transaction, and restoring from it after a crash would
    /// lose the block. (A transaction that was mined but is not yet finalized
    /// also looks "missing", so the stored checkpoint can lag until
    /// finalization; restoring from it is safe, merely conservative.)
    /// Returns whether it was applied.
    pub fn apply_checkpoint(
        &mut self,
        checkpoint: serde_json::Value,
        pending_tx_hashes: &HashSet<String>,
    ) -> bool {
        let covers_pending = self
            .entries
            .values()
            .filter(|entry| entry.status.in_sequencer_pending_set())
            .all(|entry| pending_tx_hashes.contains(&entry.tx_hash));

        if covers_pending {
            self.sequencer_checkpoint = Some(checkpoint);
        }

        covers_pending
    }

    /// Handles transactions that left the canonical Logos chain: rewinds the
    /// cursor to the lowest affected block and forgets it and every later
    /// entry, so the feeder publishes them again in height order. Returns the
    /// height rewound to.
    pub fn orphan(&mut self, tx_hashes: &HashSet<String>) -> Option<u64> {
        let lowest = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                tx_hashes.contains(&entry.tx_hash) && entry.status != PublicationStatus::Finalized
            })
            .map(|(height, _)| *height)
            .min()?;

        self.entries.retain(|height, _| *height < lowest);
        self.next_height = lowest;

        Some(lowest)
    }

    fn prune(&mut self) {
        let finalized: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.status == PublicationStatus::Finalized)
            .map(|(height, _)| *height)
            .collect();

        if finalized.len() > KEEP_FINALIZED {
            for height in &finalized[..finalized.len() - KEEP_FINALIZED] {
                self.entries.remove(height);
            }
        }
    }
}

/// Atomic file storage for [`PublicationState`].
#[derive(Debug, Clone)]
pub struct PublicationStore {
    path: PathBuf,
}

impl PublicationStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn load(&self) -> Result<PublicationState, String> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| format!("invalid {}: {error}", self.path.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(PublicationState::default())
            }
            Err(error) => Err(format!("failed to read {}: {error}", self.path.display())),
        }
    }

    pub fn save(&self, state: &PublicationState) -> Result<(), String> {
        let write = || -> io::Result<()> {
            if let Some(parent) = self.path.parent() {
                fs::create_dir_all(parent)?;
            }

            let temp = self.path.with_extension("json.tmp");

            fs::write(&temp, serde_json::to_vec_pretty(state)?)?;
            fs::rename(&temp, &self.path)
        };

        write().map_err(|error| format!("failed to save {}: {error}", self.path.display()))
    }
}

pub type SharedState = Arc<Mutex<PublicationState>>;

/// Read access to publication progress for RPC and logging.
#[derive(Clone)]
pub struct PublicationHandle {
    state: SharedState,
}

impl PublicationHandle {
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }

    fn lock(&self) -> MutexGuard<'_, PublicationState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn snapshot(&self) -> PublicationState {
        self.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "solizone-publication-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        directory.join("publication.json")
    }

    fn hashes(items: &[&str]) -> HashSet<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn records_submissions_and_counts_in_flight() {
        let mut state = PublicationState::default();

        state.record_submission(0, "h0".into(), "t0".into(), None);
        state.record_submission(
            1,
            "h1".into(),
            "t1".into(),
            Some(serde_json::json!({"a": 1})),
        );

        assert_eq!(state.next_height, 2);
        assert_eq!(state.in_flight(), 2);
        assert_eq!(
            state.sequencer_checkpoint,
            Some(serde_json::json!({"a": 1}))
        );

        assert_eq!(
            state.set_status("t0", PublicationStatus::Finalized),
            Some(0)
        );
        assert_eq!(state.in_flight(), 1);
        assert_eq!(
            state.set_status("unknown", PublicationStatus::OnChain),
            None
        );
    }

    #[test]
    fn stale_checkpoints_are_not_applied() {
        let mut state = PublicationState::default();
        state.record_submission(
            0,
            "h0".into(),
            "t0".into(),
            Some(serde_json::json!("fresh")),
        );

        // An event checkpoint that predates the submission does not list t0.
        assert!(!state.apply_checkpoint(serde_json::json!("stale"), &hashes(&[])));
        assert_eq!(state.sequencer_checkpoint, Some(serde_json::json!("fresh")));

        // One that lists it is applied.
        assert!(state.apply_checkpoint(serde_json::json!("newer"), &hashes(&["t0"])));
        assert_eq!(state.sequencer_checkpoint, Some(serde_json::json!("newer")));

        // Once the transaction is on chain it is no longer pending, and a
        // checkpoint without it is correct.
        state.set_status("t0", PublicationStatus::OnChain);
        assert!(state.apply_checkpoint(serde_json::json!("after"), &hashes(&[])));
    }

    #[test]
    fn orphaned_transactions_rewind_the_cursor() {
        let mut state = PublicationState::default();
        for height in 0..4u64 {
            state.record_submission(height, format!("h{height}"), format!("t{height}"), None);
        }
        state.set_status("t0", PublicationStatus::Finalized);

        // Unknown or already finalized hashes change nothing.
        assert_eq!(state.orphan(&hashes(&["nope", "t0"])), None);
        assert_eq!(state.next_height, 4);

        // Orphaning #2 forgets #2 and #3, keeps #0 and #1, and re-opens #2.
        assert_eq!(state.orphan(&hashes(&["t2"])), Some(2));
        assert_eq!(state.next_height, 2);
        assert_eq!(state.entries.keys().copied().collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn prunes_old_finalized_entries_only() {
        let mut state = PublicationState::default();

        for height in 0..(KEEP_FINALIZED as u64 + 10) {
            state.record_submission(height, format!("h{height}"), format!("t{height}"), None);
            state.set_status(&format!("t{height}"), PublicationStatus::Finalized);
        }

        // An unfinished entry survives pruning.
        let last = KEEP_FINALIZED as u64 + 10;
        state.record_submission(last, "hl".into(), "tl".into(), None);

        assert!(state.entries.len() <= KEEP_FINALIZED + 1);
        assert!(state.entries.contains_key(&last));
        assert!(!state.entries.contains_key(&0));
    }

    #[test]
    fn store_round_trips_and_defaults_when_missing() {
        let path = temp_path("store");
        let store = PublicationStore::new(&path);

        assert_eq!(store.load().unwrap(), PublicationState::default());

        let mut state = PublicationState::default();
        state.record_submission(
            4,
            "h4".into(),
            "t4".into(),
            Some(serde_json::json!({"x": [1, 2]})),
        );
        state.failed = Some("boom".into());
        store.save(&state).unwrap();

        assert_eq!(PublicationStore::new(&path).load().unwrap(), state);
        assert!(!path.with_extension("json.tmp").exists());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn store_rejects_corrupt_file() {
        let path = temp_path("corrupt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not json").unwrap();

        assert!(PublicationStore::new(&path).load().is_err());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
