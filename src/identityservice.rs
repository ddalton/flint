use crate::csi::{
    GetPluginCapabilitiesRequest, GetPluginCapabilitiesResponse, GetPluginInfoRequest,
    GetPluginInfoResponse, ProbeRequest, ProbeResponse,
};
use tonic::{Request, Response, Status};
use tracing::{debug, info};

use crate::csi::identity_server::Identity;

pub struct IdentityService;

const PLUGIN_NAME: &str = "flint";

#[tonic::async_trait]
impl Identity for IdentityService {
    async fn get_plugin_info(
        &self,
        request: Request<GetPluginInfoRequest>,
    ) -> Result<Response<GetPluginInfoResponse>, Status> {
        info!("GetPluginInfo");
        let get_plugin_info_response = GetPluginInfoResponse {
            name: PLUGIN_NAME.to_owned(),
            vendor_version: env!("CARGO_PKG_VERSION").to_owned(),
            ..Default::default()
        };

        Ok(Response::new(get_plugin_info_response))
    }

    async fn get_plugin_capabilities(
        &self,
        request: Request<GetPluginCapabilitiesRequest>,
    ) -> Result<Response<GetPluginCapabilitiesResponse>, Status> {
        info!("GetPluginCapabilities");
        let get_plugin_capabilities_response = GetPluginCapabilitiesResponse {
            capabilities: vec![],
        };

        Ok(Response::new(get_plugin_capabilities_response))
    }

    async fn probe(
        &self,
        request: Request<ProbeRequest>,
    ) -> Result<Response<ProbeResponse>, Status> {
        debug!("Probe");
        Ok(Response::new(Default::default()))
    }
}
