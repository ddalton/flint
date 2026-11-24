//! `nfs.chert.us/dirMode`: the permission bits of a per-volume NFS server's export root, the
//! directory every pod that mounts the volume sees as its root.
//!
//! The export root is the server's backing directory: an emptyDir for an `nfsEmptyDir` class,
//! the lvol's filesystem root otherwise. Both start out owned by root with mode 0755, so a pod
//! running as any other uid gets EACCES on its first create there. pNFS volumes have
//! `pnfs.chert.us/dirMode` (with `dirGid`); this is the same parameter for the per-volume server.
//! The class sets it, CreateVolume validates it and records it in the volume context, the
//! controller passes it to the server pod as `--dir-mode`, and the server applies it to the export
//! root at every start, once the export has proven it is the volume (F30).
//!
//! There is no `dirGid` here, so a mode that gives "other" no access is refused, as pNFS refuses
//! one without a `dirGid`: only root could use the volume.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// The StorageClass parameter, and the volume_context key CreateVolume records it under.
pub const KEY: &str = "nfs.chert.us/dirMode";

/// Parse an octal mode as the parameter and `--dir-mode` spell it: `1777`, `0777` or `0o777`.
pub fn parse(raw: &str) -> Result<u32, String> {
    let t = raw.trim();
    let invalid = || format!("{KEY}: '{raw}' is not a valid octal mode");
    let mode = u32::from_str_radix(t.strip_prefix("0o").unwrap_or(t), 8).map_err(|_| invalid())?;
    if mode == 0 || mode > 0o7777 {
        return Err(invalid());
    }
    if mode & 0o007 == 0 {
        return Err(format!(
            "{KEY} = {mode:o} denies access to other users, and a per-volume NFS export root is \
             owned by root: no pod but a root one could use the volume"
        ));
    }
    Ok(mode)
}

/// The class's mode, if it sets one.
pub fn from_parameters(params: &HashMap<String, String>) -> Result<Option<u32>, String> {
    params.get(KEY).map(|raw| parse(raw)).transpose()
}

/// The volume_context value recording `mode`, in octal as a class spells it.
pub fn context_value(mode: u32) -> String {
    format!("{mode:04o}")
}

/// The mode a volume's context records, if any. CreateVolume validated it, so an error means the
/// PV was edited by hand.
pub fn from_context(ctx: &HashMap<String, String>) -> Result<Option<u32>, String> {
    from_parameters(ctx)
}

/// Give `export` the permission bits `mode`; returns whether they changed.
pub fn apply(export: &Path, mode: u32) -> std::io::Result<bool> {
    let current = std::fs::metadata(export)?.permissions().mode() & 0o7777;
    if current == mode {
        return Ok(false);
    }
    std::fs::set_permissions(export, std::fs::Permissions::from_mode(mode))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(kv: &[(&str, &str)]) -> HashMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn modes_are_octal_in_the_spellings_a_class_uses() {
        assert_eq!(parse("1777"), Ok(0o1777));
        assert_eq!(parse("0777"), Ok(0o777));
        assert_eq!(parse("0o775"), Ok(0o775));
        assert_eq!(parse(" 777 "), Ok(0o777));
    }

    #[test]
    fn a_mode_that_is_not_octal_or_not_a_mode_is_refused() {
        for bad in ["", "0", "8", "778", "abc", "17777", "-1", "0x1ff"] {
            assert!(parse(bad).is_err(), "{bad:?} was accepted");
        }
    }

    // As pNFS refuses one without a dirGid: the root is root's, so only a root pod could write.
    #[test]
    fn a_mode_closed_to_other_users_is_refused() {
        let e = parse("0770").unwrap_err();
        assert!(e.contains("denies access to other users"), "{e}");
        assert!(parse("0750").is_err());
        assert_eq!(parse("0775"), Ok(0o775));
    }

    #[test]
    fn a_class_without_the_parameter_leaves_the_root_alone() {
        assert_eq!(from_parameters(&params(&[("nfsEmptyDir", "true")])), Ok(None));
        assert_eq!(from_parameters(&params(&[(KEY, "1777")])), Ok(Some(0o1777)));
        assert!(from_parameters(&params(&[(KEY, "0700")])).is_err());
    }

    #[test]
    fn the_context_records_what_the_server_reads_back() {
        let ctx = params(&[(KEY, &context_value(0o1777))]);
        assert_eq!(ctx[KEY], "1777");
        assert_eq!(from_context(&ctx), Ok(Some(0o1777)));
        assert_eq!(context_value(0o777), "0777");
        assert_eq!(from_context(&params(&[])), Ok(None));
    }

    #[test]
    fn the_export_root_gets_the_mode_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(apply(dir.path(), 0o1777).unwrap());
        assert_eq!(std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o7777, 0o1777);
        assert!(!apply(dir.path(), 0o1777).unwrap(), "an unchanged mode is not rewritten");
        assert!(apply(&dir.path().join("missing"), 0o777).is_err());
    }
}
