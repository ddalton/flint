// identity.rs - Identity service implementation
use std::sync::Arc;
use crate::driver::SpdkCsiDriver;
use crate::csi_driver::csi::csi::v1::{
    identity_server::Identity,
    GetPluginInfoRequest, GetPluginInfoResponse,
    GetPluginCapabilitiesRequest, GetPluginCapabilitiesResponse,
    ProbeRequest, ProbeResponse,
    PluginCapability, plugin_capability,
};
use tonic::{Request, Response, Status};

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
}

#[tonic::async_trait]
impl Identity for IdentityService {
    async fn get_plugin_info(
        &self,
        _request: Request<GetPluginInfoRequest>,
    ) -> Result<Response<GetPluginInfoResponse>, Status> {
        let mut manifest = std::collections::HashMap::new();
        manifest.insert("description".to_string(), "SPDK CSI Driver with NVMe-oF support".to_string());
        manifest.insert("repository".to_string(), "https://github.com/your-org/spdk-csi-driver".to_string());

        Ok(Response::new(GetPluginInfoResponse {
            name: PLUGIN_NAME.to_string(),
            vendor_version: PLUGIN_VERSION.to_string(),
            manifest,
        }))
    }

    async fn get_plugin_capabilities(
        &self,
        _request: Request<GetPluginCapabilitiesRequest>,
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
        // Perform actual health check instead of always returning true
        let is_ready = self.check_spdk_health().await;

        if !is_ready {
            println!("SPDK health check failed during probe");
        }

        Ok(Response::new(ProbeResponse {
            ready: Some(is_ready),
        }))
    }
}
