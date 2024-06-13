pub mod identityservice;
pub mod nodeservice;
pub(crate) mod spdkdriver;

pub mod csi {
    tonic::include_proto!("csi.v1");
}
