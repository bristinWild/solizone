//! Logos Zone SDK adapter: drives a `ZoneSequencer`, submits blocks as channel
//! inscriptions and mirrors transaction status into the publication state.

use std::{collections::HashSet, fs, path::Path};

use lb_groth16::fr_from_bytes;
use lb_key_management_system_service::keys::{Ed25519Key, ZkPublicKey};
use lb_zone_sdk::{
    CommonHttpClient,
    adapter::NodeHttpClient,
    node_types::{ChannelId, Inscription},
    sequencer::{
        Error as SequencerError, Event, FundingConfig, SequencerCheckpoint, SequencerClient,
        ZoneSequencer,
    },
};

use crate::node::NodeHandle;

use super::{
    feeder::{self, FeederConfig, InscriptionSink, SubmitError, Submitted},
    state::{
        PublicationHandle, PublicationState, PublicationStatus, PublicationStore, SharedState,
    },
};

/// Everything needed to talk to a Logos node as the zone's sequencer.
#[derive(Debug, Clone)]
pub struct LogosPublicationConfig {
    /// HTTP API of the Logos (Basecamp) node, e.g. `http://127.0.0.1:8080`.
    pub node_url: String,
    pub channel_id: [u8; 32],
    pub sequencer_key: [u8; 32],
    /// Public wallet address that funds inscription fees (not a secret).
    pub funding_address: [u8; 32],
    pub max_tx_fee: u64,
    pub feeder: FeederConfig,
}

/// Reads a 32-byte hex value (optionally `0x`-prefixed) from a file.
pub fn read_hex_32(path: &Path) -> Result<[u8; 32], String> {
    let raw = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;

    parse_hex_32(raw.trim()).map_err(|error| format!("{}: {error}", path.display()))
}

pub fn parse_hex_32(value: &str) -> Result<[u8; 32], String> {
    let value = value.strip_prefix("0x").unwrap_or(value);

    let bytes = hex::decode(value).map_err(|error| format!("invalid hex: {error}"))?;

    bytes
        .try_into()
        .map_err(|_| "must contain exactly 32 bytes".to_string())
}

struct LogosSink {
    client: SequencerClient,
}

impl InscriptionSink for LogosSink {
    async fn submit(&self, payload: Vec<u8>) -> Result<Submitted, SubmitError> {
        let size = payload.len();

        let inscription = Inscription::try_from(payload).map_err(|_| {
            SubmitError::Rejected(format!("{size} bytes exceeds the Logos inscription limit"))
        })?;

        match self.client.publish(inscription).await {
            Ok((result, checkpoint)) => Ok(Submitted {
                tx_hash: result.inscription_id().to_string(),
                checkpoint: serde_json::to_value(&checkpoint).ok(),
            }),
            Err(SequencerError::Unavailable { reason }) => {
                Err(SubmitError::Unavailable(reason.to_string()))
            }
            Err(SequencerError::Network(reason)) => Err(SubmitError::Unavailable(reason)),
            Err(SequencerError::ChannelStateChanged(reason)) => {
                Err(SubmitError::Unavailable(reason.to_string()))
            }
        }
    }
}

fn lock(state: &SharedState) -> std::sync::MutexGuard<'_, PublicationState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn pending_hashes(checkpoint: &SequencerCheckpoint) -> HashSet<String> {
    checkpoint
        .pending_txs
        .iter()
        .map(|(hash, _)| hash.to_string())
        .collect()
}

/// Starts publishing `node`'s blocks to Logos in background tasks. Returns
/// once the sequencer is created; connecting and backfilling continue in the
/// background, and publication begins when the sequencer reports ready.
pub fn spawn(
    config: LogosPublicationConfig,
    node: NodeHandle,
    store: PublicationStore,
) -> Result<PublicationHandle, String> {
    let funding_pk = ZkPublicKey::new(
        fr_from_bytes(&config.funding_address)
            .map_err(|error| format!("invalid funding address: {error:?}"))?,
    );

    let funding = FundingConfig {
        funding_pk,
        max_tx_fee: config.max_tx_fee.into(),
        priority_fee_percent: FundingConfig::DEFAULT_PRIORITY_FEE_PERCENT,
        change_pk: None,
    };

    let url = config
        .node_url
        .parse()
        .map_err(|error| format!("invalid Logos node URL {:?}: {error}", config.node_url))?;

    let logos_node = NodeHttpClient::new(CommonHttpClient::new(None), url);

    let initial = store.load()?;

    feeder::check_cursor(&initial, &node)?;

    let checkpoint = match &initial.sequencer_checkpoint {
        Some(value) => match serde_json::from_value::<SequencerCheckpoint>(value.clone()) {
            Ok(checkpoint) => {
                println!(
                    "Logos publication: restored sequencer checkpoint ({} pending transaction(s))",
                    checkpoint.pending_txs.len()
                );
                Some(checkpoint)
            }
            Err(error) => {
                eprintln!("Logos publication: ignoring incompatible checkpoint: {error}");
                None
            }
        },
        None => None,
    };

    if let Some(reason) = &initial.failed {
        eprintln!("Logos publication was stopped earlier: {reason}");
    }

    println!(
        "Logos publication: next block to publish is #{}",
        initial.next_height
    );

    let state: SharedState = std::sync::Arc::new(std::sync::Mutex::new(initial));

    let mut sequencer = ZoneSequencer::init(
        ChannelId::from(config.channel_id),
        Ed25519Key::from_bytes(&config.sequencer_key),
        logos_node,
        funding,
        checkpoint,
    );

    let client = sequencer.client();
    let mut ready = sequencer.subscribe_ready();

    // Drives the sequencer: connection, backfill, live blocks, resubmission.
    // Everything else (publishing, status) goes through its client handles.
    {
        let state = state.clone();
        let store = store.clone();

        tokio::spawn(async move {
            loop {
                match sequencer.next_event().await {
                    Event::BlocksProcessed {
                        checkpoint,
                        channel_update,
                        finalized,
                        ..
                    } => {
                        let pending = pending_hashes(&checkpoint);
                        let orphaned: HashSet<String> = channel_update
                            .orphaned()
                            .iter()
                            .map(|tx| tx.tx_hash().to_string())
                            .collect();

                        let mut guard = lock(&state);
                        let mut changed = false;

                        for tx in &finalized {
                            let tx_hash = tx.tx_hash.to_string();

                            if let Some(height) =
                                guard.set_status(&tx_hash, PublicationStatus::Finalized)
                            {
                                println!("block #{height} finalized on Logos ({tx_hash})");
                                changed = true;
                            }
                        }

                        if !orphaned.is_empty()
                            && let Some(height) = guard.orphan(&orphaned)
                        {
                            eprintln!(
                                "Logos publication: transaction(s) orphaned, \
                                 re-publishing from block #{height}"
                            );
                            changed = true;
                        }

                        if let Ok(value) = serde_json::to_value(&checkpoint) {
                            changed |= guard.apply_checkpoint(value, &pending);
                        }

                        if changed && let Err(error) = store.save(&guard) {
                            eprintln!("Logos publication: {error}");
                        }
                    }
                    Event::Ready => println!("Logos publication: sequencer ready"),
                    Event::MempoolPending(_) | Event::TurnNotification { .. } => {}
                }
            }
        });
    }

    // Feeds blocks to the sequencer once it is connected and ready.
    {
        let state = state.clone();
        let feeder_config = config.feeder.clone();

        tokio::spawn(async move {
            if ready.wait_for(|ready| *ready).await.is_err() {
                return;
            }

            feeder::run(LogosSink { client }, node, state, store, feeder_config).await;
        });
    }

    Ok(PublicationHandle::new(state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_with_and_without_prefix() {
        let plain = "11".repeat(32);

        assert_eq!(parse_hex_32(&plain).unwrap(), [0x11; 32]);
        assert_eq!(parse_hex_32(&format!("0x{plain}")).unwrap(), [0x11; 32]);
    }

    #[test]
    fn rejects_wrong_length_and_bad_hex() {
        assert!(parse_hex_32("1234").is_err());
        assert!(parse_hex_32(&"zz".repeat(32)).is_err());
    }
}
