pub mod models;
pub mod spdk_native;

// Include generated CSI protobuf types
pub mod csi {
    #![allow(non_camel_case_types)]
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/csi.rs"));
}

pub use models::*;
