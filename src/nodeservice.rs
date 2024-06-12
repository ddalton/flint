use tonic::Request;

use crate::csi::{node_server::Node, NodeStageVolumeRequest};

#[derive(Debug)]
pub struct NodeService {
    driver: SpdkDriver,
    node_id: String,
}

impl NodeService {
    pub fn new(node_id: String) -> Self {
        NodeService {
            driver: SpdkDriver::new(),
            node_id,
        }
    }
}

#[tonic::async_trait]
impl Node for NodeService {

}

