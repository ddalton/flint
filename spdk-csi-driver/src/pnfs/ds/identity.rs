//! DS identity ↔ data-volume binding guard (durable-DS plan Phase 2).
//!
//! The chart convention (pod name → device_id → PVC follows the pod)
//! keeps a DS bound to its own data volume across reschedules — by
//! convention only. This module is the refusal for when convention
//! breaks: an operator re-pointing a PVC, a restore from another DS's
//! snapshot, or an `_hr`-style identity-aliasing bug handing DS-A
//! DS-B's volume. Serving another device's stripes under this
//! device_id corrupts client reads silently (stripe maps address
//! devices, not volumes), so the DS refuses to start instead.
//!
//! On first boot the DS stamps `<data_dir>/.flint-ds-identity` with
//! its device_id and a creation timestamp; every later boot verifies
//! the marker matches. The creation stamp also rides along in
//! RegisterRequest so the MDS can WARN when a device re-registers
//! with a different volume than it had before.

use std::fmt;
use std::path::{Path, PathBuf};

/// Marker file name, relative to the DS data dir (the bdev mount point).
pub const MARKER_FILE: &str = ".flint-ds-identity";

/// First line of the marker; bump the suffix on format changes.
const MARKER_HEADER: &str = "flint-ds-identity v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityMarker {
    pub device_id: String,
    /// Unix seconds at first stamp — stable for the volume's lifetime.
    pub created_at: u64,
}

#[derive(Debug)]
pub enum IdentityError {
    /// The volume belongs to a different device — REFUSE to serve.
    Mismatch {
        expected: String,
        found: String,
        marker_path: PathBuf,
    },
    /// Marker exists but can't be parsed — refuse rather than guess.
    Corrupt { marker_path: PathBuf, detail: String },
    Io(std::io::Error),
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mismatch { expected, found, marker_path } => write!(
                f,
                "data volume belongs to device '{}' but this DS is '{}' \
                 ({}). REFUSING to serve another device's stripes — check \
                 the PVC↔pod binding; if this volume was deliberately \
                 re-assigned, delete the marker file to re-stamp.",
                found, expected, marker_path.display(),
            ),
            Self::Corrupt { marker_path, detail } => write!(
                f,
                "identity marker {} is unreadable ({}) — refusing to \
                 guess volume ownership; inspect or delete the marker to \
                 re-stamp",
                marker_path.display(), detail,
            ),
            Self::Io(e) => write!(f, "identity marker I/O: {}", e),
        }
    }
}

impl std::error::Error for IdentityError {}

impl From<std::io::Error> for IdentityError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Verify the identity marker in `data_dir` against `device_id`,
/// stamping a fresh marker if this is the volume's first boot.
///
/// Returns the marker (fresh or verified) so the caller can report
/// `created_at` in RegisterRequest.
pub fn verify_or_stamp(data_dir: &Path, device_id: &str) -> Result<IdentityMarker, IdentityError> {
    let path = data_dir.join(MARKER_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let marker = parse(&content).map_err(|detail| IdentityError::Corrupt {
                marker_path: path.clone(),
                detail,
            })?;
            if marker.device_id != device_id {
                return Err(IdentityError::Mismatch {
                    expected: device_id.to_string(),
                    found: marker.device_id,
                    marker_path: path,
                });
            }
            Ok(marker)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let marker = IdentityMarker {
                device_id: device_id.to_string(),
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            };
            // Temp-file + rename so a crash mid-write can't leave a
            // half-marker that bricks the next boot as Corrupt.
            let tmp = data_dir.join(format!("{}.tmp", MARKER_FILE));
            std::fs::write(&tmp, render(&marker))?;
            std::fs::rename(&tmp, &path)?;
            Ok(marker)
        }
        Err(e) => Err(IdentityError::Io(e)),
    }
}

fn render(marker: &IdentityMarker) -> String {
    format!(
        "{}\ndevice_id={}\ncreated_at={}\n",
        MARKER_HEADER, marker.device_id, marker.created_at,
    )
}

fn parse(content: &str) -> Result<IdentityMarker, String> {
    let mut lines = content.lines();
    match lines.next() {
        Some(h) if h == MARKER_HEADER => {}
        Some(h) => return Err(format!("unknown header '{}'", h)),
        None => return Err("empty marker".into()),
    }
    let mut device_id = None;
    let mut created_at = None;
    for line in lines {
        if let Some(v) = line.strip_prefix("device_id=") {
            device_id = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("created_at=") {
            created_at = Some(v.parse::<u64>().map_err(|e| format!("created_at: {}", e))?);
        }
    }
    Ok(IdentityMarker {
        device_id: device_id.ok_or("missing device_id")?,
        created_at: created_at.ok_or("missing created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "flint-ds-identity-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id(),
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn first_boot_stamps_then_verifies() {
        let dir = tmpdir();
        let stamped = verify_or_stamp(&dir, "ds-a").expect("first boot stamps");
        assert_eq!(stamped.device_id, "ds-a");
        assert!(stamped.created_at > 0);
        // Second boot: verified, and created_at is STABLE (not re-stamped).
        let verified = verify_or_stamp(&dir, "ds-a").expect("re-boot verifies");
        assert_eq!(verified, stamped);
    }

    #[test]
    fn mismatched_device_refuses() {
        let dir = tmpdir();
        verify_or_stamp(&dir, "ds-b").unwrap();
        let err = verify_or_stamp(&dir, "ds-a").expect_err("must refuse ds-b's volume");
        match err {
            IdentityError::Mismatch { expected, found, .. } => {
                assert_eq!(expected, "ds-a");
                assert_eq!(found, "ds-b");
            }
            other => panic!("expected Mismatch, got {:?}", other),
        }
    }

    #[test]
    fn corrupt_marker_refuses() {
        let dir = tmpdir();
        std::fs::write(dir.join(MARKER_FILE), "not a marker\n").unwrap();
        match verify_or_stamp(&dir, "ds-a") {
            Err(IdentityError::Corrupt { .. }) => {}
            other => panic!("expected Corrupt, got {:?}", other),
        }
    }

    #[test]
    fn round_trip_parse() {
        let m = IdentityMarker { device_id: "flint-pnfs-ds-0".into(), created_at: 1_700_000_000 };
        assert_eq!(parse(&render(&m)).unwrap(), m);
    }
}
