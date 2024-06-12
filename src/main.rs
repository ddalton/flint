use tonic::transport::Server;
use tracing::instrument;
use tracing_subscriber::EnvFilter;

#[instrument]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let addr = "[::1]:10000".parse().unwrap();

    let route_guide = RouteGuideService {
        features: Arc::new(data::load()),
    };

    let svc = RouteGuideServer::new(route_guide);

    Server::builder().add_service(svc).serve(addr).await?;

    Ok(())
}
