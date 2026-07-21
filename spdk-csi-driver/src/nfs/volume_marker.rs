//! F30: volume-identity marker — the export must prove it is the volume
//! the server was told to serve.
//!
//! The incident (runx, 2026-07-20): an out-of-band lazy umount without a
//! kubelet restart let NodePublish bind-mount the BARE mountpoint
//! directory; the server then exported an EMPTY root-disk directory,
//! minted a fresh `fh.key` and an empty state DB, and served it
//! silently — every client handle failed HMAC and nothing said why. The
//! marker closes the "wrong/empty directory" class: NodeStage stamps
//! `<staging>/.flint-nfs/volume-id` right after the backing filesystem
//! mounts, and the server REFUSES to start unless the export carries
//! the id it was configured for.
//!
//! Verdict table (pinned by tests):
//!   marker == expected                → Serve
//!   marker != expected                → RefuseMismatch (wrong volume!)
//!   no marker, flint state present    → AdoptLegacy (pre-fix volume:
//!                                       stamp it now, serve)
//!   no marker, no flint state         → RefuseEmpty (the F30 shape: an
//!                                       empty dir is NEVER a formatted
//!                                       flint volume post-fix)

use std::path::Path;

/// Marker path relative to the export root.
pub const MARKER_REL: &str = ".flint-nfs/volume-id";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerVerdict {
    Serve,
    /// Pre-marker volume (has flint state but no marker): stamp and serve.
    AdoptLegacy,
    /// Marker present but for a DIFFERENT volume — serving would hand one
    /// volume's clients another volume's data.
    RefuseMismatch { found: String },
    /// No marker and no flint state: an empty/foreign directory (the F30
    /// empty-dir export). Refuse loudly instead of minting fresh state.
    RefuseEmpty,
}

/// Pure classification rule.
pub fn classify(marker: Option<&str>, expected: &str, has_flint_state: bool) -> MarkerVerdict {
    match marker {
        Some(m) if m == expected => MarkerVerdict::Serve,
        Some(m) => MarkerVerdict::RefuseMismatch {
            found: m.to_string(),
        },
        None if has_flint_state => MarkerVerdict::AdoptLegacy,
        None => MarkerVerdict::RefuseEmpty,
    }
}

/// Read state from the export, classify, and stamp the marker when the
/// verdict allows serving. Returns the verdict; the caller decides the
/// process's fate (the server exits on Refuse*).
pub fn verify_and_adopt(export_root: &Path, expected: &str) -> std::io::Result<MarkerVerdict> {
    let marker_path = export_root.join(MARKER_REL);
    let marker = match std::fs::read_to_string(&marker_path) {
        Ok(s) => Some(s.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    // "flint state" = the per-volume handle key: it exists on every
    // volume any flint server has ever served (fh_kernel creates it at
    // first boot), and never on an empty/foreign directory.
    let has_flint_state = export_root.join(".flint-nfs").join("fh.key").exists();
    let verdict = classify(marker.as_deref(), expected, has_flint_state);
    match &verdict {
        MarkerVerdict::Serve => {}
        MarkerVerdict::AdoptLegacy => {
            std::fs::create_dir_all(export_root.join(".flint-nfs"))?;
            // write-then-rename: a crash mid-write must not leave a
            // truncated marker that later reads as a mismatch.
            let tmp = export_root.join(".flint-nfs").join(".volume-id.tmp");
            std::fs::write(&tmp, expected)?;
            std::fs::rename(&tmp, &marker_path)?;
        }
        MarkerVerdict::RefuseMismatch { .. } | MarkerVerdict::RefuseEmpty => {}
    }
    Ok(verdict)
}

/// NodeStage-side stamp: write the marker (idempotent) onto a freshly
/// staged backing filesystem. Called only for NFS backing volumes —
/// user RWO filesystems must not get a `.flint-nfs/` dropping.
pub fn stamp(staging_root: &Path, volume_id: &str) -> std::io::Result<()> {
    let marker_path = staging_root.join(MARKER_REL);
    if let Ok(existing) = std::fs::read_to_string(&marker_path) {
        if existing.trim() == volume_id {
            return Ok(());
        }
        // A DIFFERENT id on a staging mount is the same wrong-volume
        // hazard the server refuses on — never overwrite silently.
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "staging carries volume-id {:?}, expected {:?} — refusing to restamp",
                existing.trim(),
                volume_id
            ),
        ));
    }
    std::fs::create_dir_all(staging_root.join(".flint-nfs"))?;
    let tmp = staging_root.join(".flint-nfs").join(".volume-id.tmp");
    std::fs::write(&tmp, volume_id)?;
    std::fs::rename(&tmp, &marker_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VOL: &str = "pvc-13cb973b-b005-42c1-8d17-c51e80321b5b";

    #[test]
    fn verdict_table_is_pinned() {
        assert_eq!(classify(Some(VOL), VOL, true), MarkerVerdict::Serve);
        assert_eq!(classify(Some(VOL), VOL, false), MarkerVerdict::Serve);
        assert_eq!(
            classify(Some("pvc-other"), VOL, true),
            MarkerVerdict::RefuseMismatch { found: "pvc-other".into() }
        );
        assert_eq!(classify(None, VOL, true), MarkerVerdict::AdoptLegacy);
        assert_eq!(classify(None, VOL, false), MarkerVerdict::RefuseEmpty);
    }

    /// The F30 incident shape end-to-end: an EMPTY export dir must be
    /// refused — the old server minted a fresh fh.key here and served
    /// nothing to everyone.
    #[test]
    fn empty_export_dir_is_refused() {
        let dir = std::env::temp_dir().join(format!("f30-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(verify_and_adopt(&dir, VOL).unwrap(), MarkerVerdict::RefuseEmpty);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Legacy volume (fh.key, no marker): adopt — and the stamp must be
    /// durable so the next boot takes the Serve arm.
    #[test]
    fn legacy_volume_adopts_then_serves() {
        let dir = std::env::temp_dir().join(format!("f30-legacy-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".flint-nfs")).unwrap();
        std::fs::write(dir.join(".flint-nfs/fh.key"), [0u8; 32]).unwrap();
        assert_eq!(verify_and_adopt(&dir, VOL).unwrap(), MarkerVerdict::AdoptLegacy);
        assert_eq!(verify_and_adopt(&dir, VOL).unwrap(), MarkerVerdict::Serve);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Wrong-volume marker: refuse, and DON'T touch the marker.
    #[test]
    fn mismatched_marker_refuses_and_preserves() {
        let dir = std::env::temp_dir().join(format!("f30-mismatch-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".flint-nfs")).unwrap();
        std::fs::write(dir.join(MARKER_REL), "pvc-other").unwrap();
        assert_eq!(
            verify_and_adopt(&dir, VOL).unwrap(),
            MarkerVerdict::RefuseMismatch { found: "pvc-other".into() }
        );
        assert_eq!(std::fs::read_to_string(dir.join(MARKER_REL)).unwrap(), "pvc-other");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// NodeStage stamp: idempotent for the same id, hard-refuses a
    /// different id (wrong-volume staging).
    #[test]
    fn stamp_idempotent_and_conflict_safe() {
        let dir = std::env::temp_dir().join(format!("f30-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        stamp(&dir, VOL).unwrap();
        stamp(&dir, VOL).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(MARKER_REL)).unwrap(), VOL);
        assert!(stamp(&dir, "pvc-other").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
