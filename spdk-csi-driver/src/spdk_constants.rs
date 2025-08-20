// spdk_constants.rs - Shared SPDK naming conventions and constants

/// SPDK bdev naming constants - ensures consistency across the entire codebase
pub const NVME_BDEV_PREFIX: &str = "nvme";
pub const AIO_BDEV_PREFIX: &str = "aio";
pub const BDEV_NAME_SEPARATOR: &str = "_";  // SPDK's native convention

/// Helper functions for consistent bdev naming
pub fn format_nvme_bdev_name(device_name: &str) -> String {
    format!("{}{}{}", NVME_BDEV_PREFIX, BDEV_NAME_SEPARATOR, device_name)
}

pub fn format_aio_bdev_name(device_name: &str) -> String {
    format!("{}{}{}", AIO_BDEV_PREFIX, BDEV_NAME_SEPARATOR, device_name)
}

/// Extract device name from hardware_id (removes /dev/ prefix)
pub fn extract_device_name(hardware_id: &str) -> String {
    if let Some(name) = hardware_id.strip_prefix("/dev/") {
        name.to_string()
    } else {
        hardware_id.to_string()
    }
}