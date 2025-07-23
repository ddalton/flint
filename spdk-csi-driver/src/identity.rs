// identity.rs - Identity service implementation
use std::sync::Arc;
use crate::driver::SpdkCsiDriver;
use spdk_csi_driver::csi::{
    identity_server::Identity,
    GetPluginInfoRequest, GetPluginInfoResponse,
    GetPluginCapabilitiesRequest, GetPluginCapabilitiesResponse,
    ProbeRequest, ProbeResponse,
    PluginCapability, plugin_capability,
};
use tonic::{Request, Response, Status};
use kube;

const PLUGIN_NAME: &str = "spdk.csi.storage.io";
const PLUGIN_VERSION: &str = "1.0.0";

pub struct IdentityService {
    driver: Arc<SpdkCsiDriver>,
}

impl IdentityService {
    pub fn new(driver: Arc<SpdkCsiDriver>) -> Self {
        Self { driver }
    }

    /// Check if SPDK is healthy and ready
    async fn check_spdk_health(&self) -> bool {
        use reqwest::Client as HttpClient;
        use serde_json::json;

        let http_client = HttpClient::new();
        
        // Quick health check via SPDK RPC
        match http_client
            .post(&self.driver.spdk_rpc_url)
            .json(&json!({"method": "spdk_get_version"}))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(response) => response.status().is_success(),
            Err(_) => false,
        }
    }

    /// Check if Kubernetes API is accessible (for controller mode)
    async fn check_kubernetes_api_readiness(&self) -> bool {
        // Quick check to see if we can reach the Kubernetes API
        // This is what the controller actually needs to work
        match kube::Client::try_default().await {
            Ok(_) => {
                println!("Kubernetes API is accessible");
                true
            }
            Err(e) => {
                println!("Kubernetes API not accessible: {}", e);
                false
            }
        }
    }
}

#[tonic::async_trait]
impl Identity for IdentityService {
    async fn get_plugin_info(
        &self, 
        _request: Request<GetPluginInfoRequest>
    ) -> Result<Response<GetPluginInfoResponse>, Status> {
        Ok(Response::new(GetPluginInfoResponse {
            name: PLUGIN_NAME.to_string(),
            vendor_version: PLUGIN_VERSION.to_string(),
            manifest: Default::default(),
        }))
    }

    async fn get_plugin_capabilities(
        &self, 
        _request: Request<GetPluginCapabilitiesRequest>
    ) -> Result<Response<GetPluginCapabilitiesResponse>, Status> {
        let capabilities = vec![
            // Controller service capability
            PluginCapability {
                r#type: Some(plugin_capability::Type::Service(
                    plugin_capability::Service {
                        r#type: plugin_capability::service::Type::ControllerService as i32,
                    },
                )),
            },
            // Volume accessibility constraints
            PluginCapability {
                r#type: Some(plugin_capability::Type::Service(
                    plugin_capability::Service {
                        r#type: plugin_capability::service::Type::VolumeAccessibilityConstraints as i32,
                    },
                )),
            },
            // Volume expansion capability
            PluginCapability {
                r#type: Some(plugin_capability::Type::VolumeExpansion(
                    plugin_capability::VolumeExpansion {
                        r#type: plugin_capability::volume_expansion::Type::Online as i32,
                    },
                )),
            },
        ];

        Ok(Response::new(GetPluginCapabilitiesResponse {
            capabilities,
        }))
    }

    async fn probe(
        &self, 
        _request: Request<ProbeRequest>
    ) -> Result<Response<ProbeResponse>, Status> {
        // Check if we're running in controller mode by checking CSI_MODE environment variable
        let csi_mode = std::env::var("CSI_MODE").unwrap_or("all".to_string());
        
        let is_ready = if csi_mode == "controller" {
            // Controller mode: ready if we can connect to Kubernetes API
            // Controllers work through K8s APIs, not direct SPDK access
            println!("Controller mode: checking Kubernetes API readiness...");
            self.check_kubernetes_api_readiness().await
        } else {
            // Node mode or all mode: check SPDK health
            self.check_spdk_health().await
        };

        if !is_ready {
            println!("Health check failed during probe (mode: {})", csi_mode);
        }

        Ok(Response::new(ProbeResponse {
            ready: Some(is_ready),
        }))
    }
}
