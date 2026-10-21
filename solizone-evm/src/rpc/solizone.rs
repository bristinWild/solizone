//! Solizone-specific JSON-RPC methods (outside the `eth_` namespace).

use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use serde::Serialize;

use crate::{node::NodeHandle, publication::PublicationHandle};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedBlock {
    pub height: u64,
    pub block_hash: String,
    pub tx_hash: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationStatusView {
    /// Whether this node was started with `--publish`.
    pub enabled: bool,
    pub head_height: Option<u64>,
    /// Next block that will be submitted to Logos.
    pub next_height: u64,
    /// Produced blocks not yet submitted.
    pub unsubmitted: u64,
    /// Submitted blocks not yet finalized on Logos.
    pub in_flight: usize,
    /// Why publication stopped permanently, if it did.
    pub failed: Option<String>,
    /// The most recent submissions, newest first.
    pub recent: Vec<PublishedBlock>,
}

#[rpc(server)]
pub trait SolizoneApi {
    /// Progress of publishing blocks to Logos.
    #[method(name = "solizone_publicationStatus")]
    fn publication_status(&self) -> RpcResult<PublicationStatusView>;
}

pub struct SolizoneRpc {
    node: NodeHandle,
    publication: Option<PublicationHandle>,
}

impl SolizoneRpc {
    pub fn new(node: NodeHandle, publication: Option<PublicationHandle>) -> Self {
        Self { node, publication }
    }
}

const RECENT: usize = 10;

impl SolizoneApiServer for SolizoneRpc {
    fn publication_status(&self) -> RpcResult<PublicationStatusView> {
        let head_height = self.node.core().head_height();

        let Some(publication) = &self.publication else {
            return Ok(PublicationStatusView {
                enabled: false,
                head_height,
                next_height: 0,
                unsubmitted: 0,
                in_flight: 0,
                failed: None,
                recent: Vec::new(),
            });
        };

        let state = publication.snapshot();

        let produced = head_height.map_or(0, |head| head + 1);

        Ok(PublicationStatusView {
            enabled: true,
            head_height,
            next_height: state.next_height,
            unsubmitted: produced.saturating_sub(state.next_height),
            in_flight: state.in_flight(),
            failed: state.failed.clone(),
            recent: state
                .entries
                .iter()
                .rev()
                .take(RECENT)
                .map(|(height, entry)| PublishedBlock {
                    height: *height,
                    block_hash: entry.block_hash.clone(),
                    tx_hash: entry.tx_hash.clone(),
                    status: format!("{:?}", entry.status).to_lowercase(),
                })
                .collect(),
        })
    }
}
