use std::path::Path;
use tonic::transport::Server;
use tracing::{info, instrument};
use tracing_subscriber::EnvFilter;
use clap::Parser;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use flint::controller::ControllerService;
use flint::identity::IdentityService;
use flint::node::NodeService;
use flint::csi::controller_server::ControllerServer;
use flint::csi::node_server::NodeServer;
use flint::csi::identity_server::IdentityServer;

/// Must pass --endpoint <endpoint> and --node-id <id> args
#[derive(Debug, Clone, Parser)]
pub struct DriverArgs {
    #[clap(long)]
    pub node_id: String,
    #[clap(long)]
    pub endpoint: String,
}

#[instrument]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let DriverArgs {
        node_id, endpoint
    } = DriverArgs::parse();
    info!("starting driver with node id: {node_id} endpoint: {endpoint}");

    // remove socket file if present, ignore error if DNE
    let _ = std::fs::remove_file(endpoint.as_str());
    let listener = UnixListener::bind(Path::new(endpoint.as_str()))?;
    let stream = UnixListenerStream::new(listener);

    let node_server = NodeServer::new(NodeService::new(node_id));
    let identity_server = IdentityServer::new(IdentityService);
    let controller_server = ControllerServer::new(ControllerService);

    Server::builder()
        .add_service(controller_server)
        .add_service(identity_server)
        .add_service(node_server)
        .serve_with_incoming(stream)
        .await;

    Ok(())
}
