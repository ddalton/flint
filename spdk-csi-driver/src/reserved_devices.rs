use std::collections::HashSet;
use kube::{Api, Client};
use k8s_openapi::api::core::v1::ConfigMap;

/// Reserved devices configuration manager
/// Reads the flint-reserved-devices ConfigMap to determine which devices
/// should be skipped during CSI discovery (reserved for device plugin use)
#[derive(Clone, Debug)]
pub struct ReservedDevices {
    reserved_pci_addresses: HashSet<String>,
    namespace: String,
}

impl ReservedDevices {
    /// Load reserved devices from ConfigMap
    pub async fn load(namespace: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let client = Client::try_default().await?;
        let configmaps: Api<ConfigMap> = Api::namespaced(client, namespace);

        let mut reserved_pci_addresses = HashSet::new();

        match configmaps.get("flint-reserved-devices").await {
            Ok(cm) => {
                if let Some(data) = cm.data {
                    // Parse line-separated format
                    if let Some(devices_str) = data.get("reserved-devices") {
                        for line in devices_str.lines() {
                            let pci_addr = line.trim();
                            if !pci_addr.is_empty() && !pci_addr.starts_with('#') {
                                println!("🔒 [RESERVED_DEVICES] Loaded reserved device: {}", pci_addr);
                                reserved_pci_addresses.insert(pci_addr.to_string());
                            }
                        }
                    }

                    // Also support JSON format
                    if let Some(devices_json) = data.get("reserved-devices-json") {
                        if let Ok(devices) = serde_json::from_str::<Vec<String>>(devices_json) {
                            for pci_addr in devices {
                                println!("🔒 [RESERVED_DEVICES] Loaded reserved device (JSON): {}", pci_addr);
                                reserved_pci_addresses.insert(pci_addr);
                            }
                        }
                    }
                }

                println!("✅ [RESERVED_DEVICES] Loaded {} reserved device(s)", reserved_pci_addresses.len());
            }
            Err(e) => {
                println!("ℹ️ [RESERVED_DEVICES] ConfigMap not found (no devices reserved): {}", e);
                // Not an error - just means no devices are reserved
            }
        }

        Ok(Self {
            reserved_pci_addresses,
            namespace: namespace.to_string(),
        })
    }

    /// Check if a device is reserved (should be skipped by CSI)
    pub fn is_reserved(&self, pci_address: &str) -> bool {
        self.reserved_pci_addresses.contains(pci_address)
    }

    /// Get all reserved PCI addresses
    pub fn get_reserved_devices(&self) -> &HashSet<String> {
        &self.reserved_pci_addresses
    }

    /// Reload configuration from ConfigMap (for dynamic updates)
    pub async fn reload(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let new_config = Self::load(&self.namespace).await?;
        self.reserved_pci_addresses = new_config.reserved_pci_addresses;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reserved_devices_parsing() {
        // Test that we can parse various formats
        let mut reserved = HashSet::new();

        // Line-separated format
        let config = "0000:02:00.0\n0000:03:00.0\n# comment\n  0000:04:00.0  ";
        for line in config.lines() {
            let pci_addr = line.trim();
            if !pci_addr.is_empty() && !pci_addr.starts_with('#') {
                reserved.insert(pci_addr.to_string());
            }
        }

        assert_eq!(reserved.len(), 3);
        assert!(reserved.contains("0000:02:00.0"));
        assert!(reserved.contains("0000:03:00.0"));
        assert!(reserved.contains("0000:04:00.0"));
    }
}
