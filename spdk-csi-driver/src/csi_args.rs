//! CSI argument checks: what every RPC verifies before it touches anything.
//!
//! The spec answers a missing required field with INVALID_ARGUMENT. The
//! first csi-sanity run (v5.6.0 on the kind tier, 2026-10-28) found most
//! RPCs doing something else: succeeding (DeleteVolume, ControllerUnpublish,
//! DeleteSnapshot, NodeUnpublish and NodeGetVolumeStats with an empty id or
//! path; CreateVolume with no name made an lvol called `vol_` that the
//! orphan sweep can never reap), answering NOT_FOUND or UNAVAILABLE from a
//! PV lookup under an empty name, or doing real work first (NodePublish made
//! its target directory and tried a restage). Each handler calls its check
//! here first; the checks are pure, so the codes are unit-tested.

use std::collections::HashMap;

use tonic::Status;

use crate::csi;

fn invalid(message: String) -> Status {
    Status::invalid_argument(message)
}

fn required(field: &str, value: &str) -> Result<(), Status> {
    if value.is_empty() {
        Err(invalid(format!("{field} is required")))
    } else {
        Ok(())
    }
}

fn capability(field: &str, cap: Option<&csi::VolumeCapability>) -> Result<(), Status> {
    let cap = cap.ok_or_else(|| invalid(format!("{field} is required")))?;
    if cap.access_mode.is_none() {
        return Err(invalid(format!("{field}.access_mode is required")));
    }
    if cap.access_type.is_none() {
        return Err(invalid(format!("{field} needs an access type (mount or block)")));
    }
    Ok(())
}

pub fn create_volume(req: &csi::CreateVolumeRequest) -> Result<(), Status> {
    if let Some(why) = crate::identity::storage_id_refusal(&req.name) {
        return Err(invalid(why));
    }
    if req.volume_capabilities.is_empty() {
        return Err(invalid("volume_capabilities is required".to_string()));
    }
    for cap in &req.volume_capabilities {
        capability("volume_capabilities", Some(cap))?;
    }
    if let Some(range) = &req.capacity_range {
        if range.required_bytes < 0 || range.limit_bytes < 0 {
            return Err(invalid(format!("capacity_range is negative: {range:?}")));
        }
        if range.limit_bytes > 0 && range.required_bytes > range.limit_bytes {
            return Err(invalid(format!(
                "capacity_range.required_bytes {} exceeds limit_bytes {}",
                range.required_bytes, range.limit_bytes
            )));
        }
    }
    Ok(())
}

/// The class's `numReplicas`: absent means 1. A value that does not parse,
/// or is outside 1..=[`crate::identity::MAX_REPLICAS`], is refused rather
/// than read as 1 -- a typo in a StorageClass must not quietly provision an
/// unreplicated volume.
pub fn replica_count(parameters: &HashMap<String, String>) -> Result<u32, Status> {
    let Some(raw) = parameters.get("numReplicas") else {
        return Ok(1);
    };
    let max = crate::identity::MAX_REPLICAS;
    match raw.trim().parse::<u32>() {
        Ok(n) if (1..=max).contains(&n) => Ok(n),
        _ => Err(invalid(format!(
            "numReplicas {raw:?} is not a replica count from 1 to {max}"
        ))),
    }
}

/// The class's `thinProvision`: absent means true; anything but
/// `true`/`false` is refused rather than read as the default.
pub fn thin_provision(parameters: &HashMap<String, String>) -> Result<bool, Status> {
    match parameters.get("thinProvision") {
        None => Ok(true),
        Some(raw) => raw
            .trim()
            .parse::<bool>()
            .map_err(|_| invalid(format!("thinProvision {raw:?} is not true or false"))),
    }
}

pub fn delete_volume(req: &csi::DeleteVolumeRequest) -> Result<(), Status> {
    required("volume_id", &req.volume_id)
}

pub fn controller_publish(req: &csi::ControllerPublishVolumeRequest) -> Result<(), Status> {
    required("volume_id", &req.volume_id)?;
    required("node_id", &req.node_id)?;
    capability("volume_capability", req.volume_capability.as_ref())
}

/// An empty `node_id` is legal here: it unpublishes from every node.
pub fn controller_unpublish(req: &csi::ControllerUnpublishVolumeRequest) -> Result<(), Status> {
    required("volume_id", &req.volume_id)
}

/// The capabilities themselves are not checked: an unsupported one is
/// answered with an empty `confirmed`, not an error.
pub fn validate_volume_capabilities(
    req: &csi::ValidateVolumeCapabilitiesRequest,
) -> Result<(), Status> {
    required("volume_id", &req.volume_id)?;
    if req.volume_capabilities.is_empty() {
        return Err(invalid("volume_capabilities is required".to_string()));
    }
    Ok(())
}

pub fn create_snapshot(req: &csi::CreateSnapshotRequest) -> Result<(), Status> {
    required("name", &req.name)?;
    required("source_volume_id", &req.source_volume_id)
}

pub fn delete_snapshot(req: &csi::DeleteSnapshotRequest) -> Result<(), Status> {
    required("snapshot_id", &req.snapshot_id)
}

pub fn controller_expand(req: &csi::ControllerExpandVolumeRequest) -> Result<(), Status> {
    required("volume_id", &req.volume_id)?;
    if req.capacity_range.is_none() {
        return Err(invalid("capacity_range is required".to_string()));
    }
    Ok(())
}

pub fn node_stage(req: &csi::NodeStageVolumeRequest) -> Result<(), Status> {
    required("volume_id", &req.volume_id)?;
    required("staging_target_path", &req.staging_target_path)?;
    capability("volume_capability", req.volume_capability.as_ref())
}

pub fn node_unstage(req: &csi::NodeUnstageVolumeRequest) -> Result<(), Status> {
    required("volume_id", &req.volume_id)?;
    required("staging_target_path", &req.staging_target_path)
}

/// `staging_target_path` stays optional: inline ephemeral volumes are
/// published without a stage.
pub fn node_publish(req: &csi::NodePublishVolumeRequest) -> Result<(), Status> {
    required("volume_id", &req.volume_id)?;
    required("target_path", &req.target_path)?;
    capability("volume_capability", req.volume_capability.as_ref())
}

pub fn node_unpublish(req: &csi::NodeUnpublishVolumeRequest) -> Result<(), Status> {
    required("volume_id", &req.volume_id)?;
    required("target_path", &req.target_path)
}

pub fn node_get_volume_stats(req: &csi::NodeGetVolumeStatsRequest) -> Result<(), Status> {
    required("volume_id", &req.volume_id)?;
    required("volume_path", &req.volume_path)
}

pub fn node_expand(req: &csi::NodeExpandVolumeRequest) -> Result<(), Status> {
    required("volume_id", &req.volume_id)?;
    required("volume_path", &req.volume_path)
}

/// NOT_FOUND for a volume path that does not exist: the spec's answer for
/// "no volume at this path" in NodeGetVolumeStats and NodeExpandVolume.
pub fn volume_path_exists(volume_path: &str) -> Result<(), Status> {
    if std::path::Path::new(volume_path).exists() {
        Ok(())
    } else {
        Err(Status::not_found(format!("volume path {volume_path} does not exist")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use csi::volume_capability::{access_mode::Mode, AccessMode, AccessType, MountVolume};
    use tonic::Code;

    fn cap() -> csi::VolumeCapability {
        csi::VolumeCapability {
            access_mode: Some(AccessMode { mode: Mode::SingleNodeWriter as i32 }),
            access_type: Some(AccessType::Mount(MountVolume::default())),
        }
    }

    fn code<T>(r: Result<T, Status>) -> Option<Code> {
        r.err().map(|s| s.code())
    }

    #[test]
    fn create_volume_refuses_a_missing_name_or_capability_before_any_work() {
        let good = csi::CreateVolumeRequest {
            name: "pvc-a6846916-3267-404e-bb22-72d820652299".into(),
            volume_capabilities: vec![cap()],
            ..Default::default()
        };
        assert_eq!(code(create_volume(&good)), None);
        let no_name = csi::CreateVolumeRequest { name: String::new(), ..good.clone() };
        assert_eq!(code(create_volume(&no_name)), Some(Code::InvalidArgument));
        let no_caps = csi::CreateVolumeRequest { volume_capabilities: vec![], ..good.clone() };
        assert_eq!(code(create_volume(&no_caps)), Some(Code::InvalidArgument));
        let no_type = csi::CreateVolumeRequest {
            volume_capabilities: vec![csi::VolumeCapability { access_type: None, ..cap() }],
            ..good.clone()
        };
        assert_eq!(code(create_volume(&no_type)), Some(Code::InvalidArgument));
        // csi-sanity's maximum-length name: 128 bytes
        let long = csi::CreateVolumeRequest { name: "a".repeat(128), ..good.clone() };
        assert_eq!(code(create_volume(&long)), Some(Code::InvalidArgument));
        let inverted = csi::CreateVolumeRequest {
            capacity_range: Some(csi::CapacityRange { required_bytes: 2, limit_bytes: 1 }),
            ..good
        };
        assert_eq!(code(create_volume(&inverted)), Some(Code::InvalidArgument));
    }

    #[test]
    fn class_parameters_that_do_not_parse_are_refused_not_defaulted() {
        let p = |k: &str, v: &str| HashMap::from([(k.to_string(), v.to_string())]);
        assert_eq!(replica_count(&HashMap::new()).unwrap(), 1);
        assert_eq!(replica_count(&p("numReplicas", "3")).unwrap(), 3);
        for bad in ["0", "10", "three", "-1", ""] {
            assert_eq!(code(replica_count(&p("numReplicas", bad))), Some(Code::InvalidArgument), "{bad:?}");
        }
        assert!(thin_provision(&HashMap::new()).unwrap());
        assert!(!thin_provision(&p("thinProvision", "false")).unwrap());
        assert_eq!(code(thin_provision(&p("thinProvision", "yes"))), Some(Code::InvalidArgument));
    }

    #[test]
    fn every_rpc_answers_a_missing_required_field_with_invalid_argument() {
        let ia = Some(Code::InvalidArgument);
        assert_eq!(code(delete_volume(&Default::default())), ia);
        assert_eq!(
            code(controller_publish(&csi::ControllerPublishVolumeRequest {
                volume_id: "v".into(),
                volume_capability: Some(cap()),
                ..Default::default()
            })),
            ia,
            "no node id"
        );
        assert_eq!(
            code(controller_publish(&csi::ControllerPublishVolumeRequest {
                volume_id: "v".into(),
                node_id: "n".into(),
                ..Default::default()
            })),
            ia,
            "no capability"
        );
        assert_eq!(code(controller_unpublish(&Default::default())), ia);
        assert_eq!(
            code(controller_unpublish(&csi::ControllerUnpublishVolumeRequest {
                volume_id: "v".into(),
                ..Default::default()
            })),
            None,
            "an empty node id unpublishes everywhere"
        );
        assert_eq!(
            code(validate_volume_capabilities(&csi::ValidateVolumeCapabilitiesRequest {
                volume_id: "v".into(),
                ..Default::default()
            })),
            ia
        );
        assert_eq!(
            code(create_snapshot(&csi::CreateSnapshotRequest {
                source_volume_id: "v".into(),
                ..Default::default()
            })),
            ia
        );
        assert_eq!(code(create_snapshot(&csi::CreateSnapshotRequest { name: "s".into(), ..Default::default() })), ia);
        assert_eq!(code(delete_snapshot(&Default::default())), ia);
        assert_eq!(
            code(controller_expand(&csi::ControllerExpandVolumeRequest {
                volume_id: "v".into(),
                ..Default::default()
            })),
            ia
        );
        assert_eq!(
            code(node_stage(&csi::NodeStageVolumeRequest {
                volume_id: "v".into(),
                volume_capability: Some(cap()),
                ..Default::default()
            })),
            ia
        );
        assert_eq!(code(node_unstage(&csi::NodeUnstageVolumeRequest { volume_id: "v".into(), ..Default::default() })), ia);
        let publish = csi::NodePublishVolumeRequest {
            volume_id: "v".into(),
            target_path: "/t".into(),
            volume_capability: Some(cap()),
            ..Default::default()
        };
        assert_eq!(code(node_publish(&publish)), None, "no staging path: an inline ephemeral");
        assert_eq!(code(node_publish(&csi::NodePublishVolumeRequest { volume_capability: None, ..publish.clone() })), ia);
        assert_eq!(code(node_publish(&csi::NodePublishVolumeRequest { target_path: String::new(), ..publish })), ia);
        assert_eq!(code(node_unpublish(&csi::NodeUnpublishVolumeRequest { volume_id: "v".into(), ..Default::default() })), ia);
        assert_eq!(code(node_get_volume_stats(&csi::NodeGetVolumeStatsRequest { volume_path: "/p".into(), ..Default::default() })), ia);
        assert_eq!(code(node_expand(&csi::NodeExpandVolumeRequest { volume_id: "v".into(), ..Default::default() })), ia);
    }

    #[test]
    fn a_volume_path_that_does_not_exist_is_not_found() {
        assert_eq!(code(volume_path_exists("some/path")), Some(Code::NotFound));
        assert_eq!(code(volume_path_exists(env!("CARGO_MANIFEST_DIR"))), None);
    }
}
