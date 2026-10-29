//! Reading a Solizone chain back from its Logos channel.
//!
//! Two parts:
//!
//! * [`BlockAssembler`] is pure: it takes inscription payloads in the order
//!   Logos finalized them, keeps the Solizone blocks, and hands them out in
//!   height order. Foreign payloads and re-published duplicates are skipped.
//! * [`read_finalized`] drives the Zone SDK to walk the channel's finalized
//!   history and feed the assembler.
//!
//! Only *finalized* inscriptions are read, so a block that Logos could still
//! reorganize away is never imported.

use std::collections::BTreeMap;

#[cfg(feature = "logos")]
use lb_groth16::fr_from_bytes;
#[cfg(feature = "logos")]
use lb_key_management_system_service::keys::{Ed25519Key, ZkPublicKey};
#[cfg(feature = "logos")]
use lb_zone_sdk::{
    CommonHttpClient,
    adapter::NodeHttpClient,
    node_types::ChannelId,
    sequencer::{Event, FinalizedOp, FundingConfig, ZoneSequencer},
};

use crate::block::SolizoneBlock;

/// What happened to one channel payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    /// A new Solizone block was stored.
    Stored,
    /// Not a Solizone block (other data on the channel).
    Foreign,
    /// Looked like a Solizone block but did not decode.
    Malformed,
    /// A block we already have (a re-published copy) or one that is older
    /// than what was already handed out.
    Duplicate,
}

/// Collects Solizone blocks from channel payloads and releases them in
/// height order.
#[derive(Debug, Default)]
pub struct BlockAssembler {
    next_height: u64,
    pending: BTreeMap<u64, SolizoneBlock>,
    pub foreign: u64,
    pub malformed: u64,
    pub duplicates: u64,
}

impl BlockAssembler {
    /// Starts at block #0 (a chain read from scratch).
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts at `height` (continuing a chain that is already stored).
    pub fn starting_at(height: u64) -> Self {
        Self {
            next_height: height,
            ..Self::default()
        }
    }

    /// The height the next released block must have.
    pub fn next_height(&self) -> u64 {
        self.next_height
    }

    /// Blocks received but not yet released because an earlier one is
    /// missing.
    pub fn buffered(&self) -> usize {
        self.pending.len()
    }

    pub fn push(&mut self, payload: &[u8]) -> Pushed {
        if !payload.starts_with(b"SZB") {
            self.foreign += 1;
            return Pushed::Foreign;
        }

        let block = match SolizoneBlock::decode(payload) {
            Ok(block) => block,
            Err(_) => {
                self.malformed += 1;
                return Pushed::Malformed;
            }
        };

        let height = block.header.height;

        // First copy wins: later ones are re-publications. Anything that
        // does not replay is caught later, when the block is applied.
        if height < self.next_height || self.pending.contains_key(&height) {
            self.duplicates += 1;
            return Pushed::Duplicate;
        }

        self.pending.insert(height, block);

        Pushed::Stored
    }

    /// Releases the next block if it has arrived.
    pub fn pop_ready(&mut self) -> Option<SolizoneBlock> {
        let block = self.pending.remove(&self.next_height)?;

        self.next_height += 1;

        Some(block)
    }
}

/// How far a read got.
#[cfg(feature = "logos")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadSummary {
    /// Inscriptions seen on the channel.
    pub inscriptions: u64,
}

#[cfg(feature = "logos")]
/// Walks the channel's finalized history on `node_url`, calling
/// `on_payload` with each inscription payload in on-chain order. Returns when
/// the SDK reports it has caught up with finalized history, or with the
/// error `on_payload` returned.
///
/// The Zone SDK has no read-only client; this uses a sequencer with a
/// throwaway key that is not accredited on the channel, so it can never
/// publish. Funding is never used and is a placeholder.
pub async fn read_finalized(
    node_url: &str,
    channel_id: [u8; 32],
    mut on_payload: impl FnMut(&[u8]) -> Result<(), String>,
) -> Result<ReadSummary, String> {
    let url = node_url
        .parse()
        .map_err(|error| format!("invalid Logos node URL {node_url:?}: {error}"))?;

    let node = NodeHttpClient::new(CommonHttpClient::new(None), url);

    let mut key_bytes = [0u8; 32];
    getrandom_fill(&mut key_bytes)?;

    let placeholder = ZkPublicKey::new(
        fr_from_bytes(&[1u8; 32]).map_err(|error| format!("placeholder key: {error:?}"))?,
    );

    let funding = FundingConfig {
        funding_pk: placeholder,
        max_tx_fee: 0u64.into(),
        priority_fee_percent: FundingConfig::DEFAULT_PRIORITY_FEE_PERCENT,
        change_pk: None,
    };

    let mut sequencer = ZoneSequencer::init(
        ChannelId::from(channel_id),
        Ed25519Key::from_bytes(&key_bytes),
        node,
        funding,
        None,
    );

    let mut summary = ReadSummary::default();
    let mut last_report = std::time::Instant::now();

    loop {
        match sequencer.next_event().await {
            Event::BlocksProcessed {
                finalized,
                checkpoint,
                ..
            } => {
                if last_report.elapsed() >= std::time::Duration::from_secs(15) {
                    println!(
                        "reading Logos history: finalized slot {:?} ({} inscription(s) so far)",
                        checkpoint.lib_slot, summary.inscriptions
                    );
                    last_report = std::time::Instant::now();
                }

                for tx in &finalized {
                    for op in &tx.ops {
                        if let FinalizedOp::Inscription(info) = op {
                            summary.inscriptions += 1;
                            on_payload(info.payload.as_ref())?;
                        }
                    }
                }
            }
            Event::Ready => return Ok(summary),
            Event::MempoolPending(_) | Event::TurnNotification { .. } => {}
        }
    }
}

#[cfg(feature = "logos")]
fn getrandom_fill(buffer: &mut [u8; 32]) -> Result<(), String> {
    use std::io::Read;

    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(buffer))
        .map_err(|error| format!("failed to get random bytes: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        eth::test_utils::{TxParams, dev_signer, sign_eip1559},
        node::{
            core::{NodeConfig, NodeCore},
            dev_accounts::{DEV_ACCOUNTS, dev_account_balance},
        },
    };
    use revm::primitives::{Address, U256, address};

    const NOW: u64 = 1_800_000_000;

    fn payloads(transfers: u64) -> Vec<Vec<u8>> {
        let genesis: Vec<(Address, U256)> = DEV_ACCOUNTS
            .iter()
            .map(|account| (account.address, dev_account_balance()))
            .collect();

        let (mut node, genesis_commit) = NodeCore::new(NodeConfig::default(), &genesis, NOW);
        let mut blocks = vec![genesis_commit.block.encode()];

        for nonce in 0..transfers {
            let raw = sign_eip1559(
                &dev_signer(0),
                TxParams::transfer(
                    9001,
                    nonce,
                    address!("2222222222222222222222222222222222222222"),
                    7,
                ),
            );
            node.submit_raw_transaction(&raw, NOW).unwrap();
            blocks.push(node.produce_block(NOW + nonce + 1).unwrap().block.encode());
        }

        blocks
    }

    fn drain(assembler: &mut BlockAssembler) -> Vec<u64> {
        std::iter::from_fn(|| assembler.pop_ready())
            .map(|block| block.header.height)
            .collect()
    }

    #[test]
    fn releases_blocks_in_height_order() {
        let blocks = payloads(3);
        let mut assembler = BlockAssembler::new();

        for payload in &blocks {
            assert_eq!(assembler.push(payload), Pushed::Stored);
        }

        assert_eq!(drain(&mut assembler), [0, 1, 2, 3]);
        assert_eq!(assembler.next_height(), 4);
    }

    #[test]
    fn waits_for_a_missing_block() {
        let blocks = payloads(3);
        let mut assembler = BlockAssembler::new();

        assembler.push(&blocks[0]);
        assembler.push(&blocks[2]);
        assembler.push(&blocks[3]);

        // #1 is missing: only #0 is released, the rest stay buffered.
        assert_eq!(drain(&mut assembler), [0]);
        assert_eq!(assembler.buffered(), 2);

        assembler.push(&blocks[1]);
        assert_eq!(drain(&mut assembler), [1, 2, 3]);
        assert_eq!(assembler.buffered(), 0);
    }

    #[test]
    fn reorders_blocks_that_arrive_out_of_order() {
        let blocks = payloads(2);
        let mut assembler = BlockAssembler::new();

        for payload in blocks.iter().rev() {
            assembler.push(payload);
        }

        assert_eq!(drain(&mut assembler), [0, 1, 2]);
    }

    #[test]
    fn skips_republished_duplicates() {
        let blocks = payloads(1);
        let mut assembler = BlockAssembler::new();

        assert_eq!(assembler.push(&blocks[0]), Pushed::Stored);
        assert_eq!(assembler.push(&blocks[0]), Pushed::Duplicate);
        assert_eq!(drain(&mut assembler), [0]);

        // After release, a repeat of an old block is still a duplicate.
        assert_eq!(assembler.push(&blocks[0]), Pushed::Duplicate);
        assert_eq!(assembler.push(&blocks[1]), Pushed::Stored);
        assert_eq!(assembler.duplicates, 2);
    }

    #[test]
    fn ignores_foreign_and_malformed_payloads() {
        let blocks = payloads(0);
        let mut assembler = BlockAssembler::new();

        assert_eq!(assembler.push(b"hello world"), Pushed::Foreign);
        assert_eq!(assembler.push(b""), Pushed::Foreign);
        assert_eq!(assembler.push(b"SZB1garbage"), Pushed::Malformed);

        let mut truncated = blocks[0].clone();
        truncated.truncate(truncated.len() - 5);
        assert_eq!(assembler.push(&truncated), Pushed::Malformed);

        assert_eq!(assembler.push(&blocks[0]), Pushed::Stored);

        assert_eq!(
            (assembler.foreign, assembler.malformed),
            (2, 2),
            "counts are reported"
        );
    }

    #[test]
    fn continues_from_a_stored_height() {
        let blocks = payloads(3);
        let mut assembler = BlockAssembler::starting_at(2);

        assert_eq!(assembler.push(&blocks[0]), Pushed::Duplicate);
        assert_eq!(assembler.push(&blocks[1]), Pushed::Duplicate);
        assert_eq!(assembler.push(&blocks[2]), Pushed::Stored);
        assert_eq!(assembler.push(&blocks[3]), Pushed::Stored);

        assert_eq!(drain(&mut assembler), [2, 3]);
    }
}
