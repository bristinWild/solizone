//! Ethereum-compatible JSON-RPC server (jsonrpsee) over a running node.

pub mod api;
pub mod convert;

use std::net::SocketAddr;

use jsonrpsee::server::{Server, ServerHandle};

pub use api::{EthApiServer, EthRpc};

use crate::node::NodeHandle;

/// Starts the HTTP + WebSocket JSON-RPC server. Returns the bound address
/// (useful with port 0) and a handle that stops the server when dropped
/// or `stop()`ped.
pub async fn start_server(
    node: NodeHandle,
    address: SocketAddr,
) -> Result<(SocketAddr, ServerHandle), String> {
    let server = Server::builder()
        .build(address)
        .await
        .map_err(|error| format!("failed to bind JSON-RPC server on {address}: {error}"))?;

    let bound = server
        .local_addr()
        .map_err(|error| format!("failed to read bound address: {error}"))?;

    let handle = server.start(EthRpc::new(node).into_rpc());

    Ok((bound, handle))
}
