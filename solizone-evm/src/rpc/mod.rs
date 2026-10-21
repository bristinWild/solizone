//! Ethereum-compatible JSON-RPC server (jsonrpsee) over a running node.

pub mod api;
pub mod convert;
pub mod solizone;

use std::net::SocketAddr;

use jsonrpsee::server::{Server, ServerHandle};

pub use api::{EthApiServer, EthRpc};

use crate::{node::NodeHandle, publication::PublicationHandle};

use solizone::{SolizoneApiServer, SolizoneRpc};

/// Starts the HTTP + WebSocket JSON-RPC server. Returns the bound address
/// (useful with port 0) and a handle that stops the server when dropped
/// or `stop()`ped.
pub async fn start_server(
    node: NodeHandle,
    address: SocketAddr,
) -> Result<(SocketAddr, ServerHandle), String> {
    start_server_with_publication(node, address, None).await
}

/// Like [`start_server`], also exposing `solizone_publicationStatus` for a
/// node that publishes to Logos.
pub async fn start_server_with_publication(
    node: NodeHandle,
    address: SocketAddr,
    publication: Option<PublicationHandle>,
) -> Result<(SocketAddr, ServerHandle), String> {
    let server = Server::builder()
        .build(address)
        .await
        .map_err(|error| format!("failed to bind JSON-RPC server on {address}: {error}"))?;

    let bound = server
        .local_addr()
        .map_err(|error| format!("failed to read bound address: {error}"))?;

    let mut module = EthRpc::new(node.clone()).into_rpc();

    module
        .merge(SolizoneRpc::new(node, publication).into_rpc())
        .map_err(|error| format!("failed to register Solizone RPC methods: {error}"))?;

    let handle = server.start(module);

    Ok((bound, handle))
}
