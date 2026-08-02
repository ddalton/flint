//! Merging driver-emitted mount defaults with operator `mountOptions`.
//!
//! WHY THIS EXISTS. Until 2026-08-02 the pNFS mount string emitted every
//! default unconditionally except the protocol version and `sec=`, then
//! appended the operator's options after them — betting on the kernel's
//! last-one-wins parse. On runax a StorageClass carrying
//! `mountOptions: ["nconnect=16"]` propagated correctly all the way to
//! `PV.spec.mountOptions` and the kernel still mounted `nconnect=4`.
//!
//! The asymmetry is the whole lesson: the two options that DID work
//! (version family, `sec=`) worked because the driver SUPPRESSED its own
//! default, producing a string with no duplicate key — they never depended
//! on ordering at all. Every option that did not work was the kind that
//! did. So the rule here is: never emit a family twice, and the question of
//! who wins never arises.
//!
//! The RWX/non-pNFS path had the same defect in a stronger form — its
//! option string was a compile-time literal and `mount_flags` was read
//! nowhere on that path, so `PV.spec.mountOptions` were dropped on the
//! floor for every RWX volume.

use std::collections::HashSet;

/// The key of `key=value`, or the whole token for a valueless flag.
fn key_of(opt: &str) -> &str {
    opt.split('=').next().unwrap_or(opt)
}

/// Canonical family of an option.
///
/// Options in one family are different SPELLINGS of one kernel knob, so an
/// operator setting any spelling must suppress a driver default written in
/// any other. Negations count: `resvport` must suppress `noresvport`, and
/// `rw` must suppress `ro`.
///
/// This is an explicit table rather than a clever `strip_prefix("no")`
/// rule, because a guessy rule is how the original bug got in. Anything
/// unrecognised is its own family, which is the safe default: an unknown
/// operator option is passed through and suppresses nothing.
pub fn family_of(opt: &str) -> &str {
    match key_of(opt) {
        // Protocol version: three spellings of one knob. Note the kernel
        // treats a genuine duplicate here as a mount FAILURE, not an
        // override, which is why suppression (not ordering) is required.
        "vers" | "nfsvers" | "minorversion" => "version",
        // Transport. `tcp`/`udp`/`rdma` are valueless spellings of `proto=`.
        "proto" | "tcp" | "udp" | "rdma" => "proto",
        // Reserved source port, and its negation.
        "resvport" | "noresvport" => "resvport",
        // Read-only/read-write. See the readonly handling in the builders:
        // a CSI readOnly publish is NOT an operator-overridable preference.
        "ro" | "rw" => "rw",
        // Attribute caching negations.
        "ac" | "noac" => "ac",
        "atime" | "noatime" => "atime",
        "diratime" | "nodiratime" => "diratime",
        "lock" | "nolock" => "lock",
        "cto" | "nocto" => "cto",
        "intr" | "nointr" => "intr",
        "hard" | "soft" => "hardness",
        // Everything else is its own family: sec, port, nconnect, rsize,
        // wsize, timeo, retrans, actimeo, ... `max_connect` deliberately
        // does NOT fold into `nconnect` — different knob.
        other => other,
    }
}

/// Split an operator entry into individual options.
///
/// Kubernetes gives `mountOptions` as a list, but operators routinely write
/// one comma-joined string, and both forms reach us. Empty and
/// whitespace-only tokens are dropped.
fn explode(flags: &[String]) -> Vec<String> {
    flags
        .iter()
        .flat_map(|f| f.split(','))
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .map(str::to_string)
        .collect()
}

/// Merge driver defaults with operator options so that each option family
/// appears exactly once.
///
/// `defaults` are emitted in order, minus any family the operator also
/// supplies. Operator options follow, de-duplicated by family with the LAST
/// occurrence winning (so a caller-supplied `nconnect=8,nconnect=16`
/// resolves to 16 rather than emitting both and hoping).
///
/// `forced` are options the operator may not override — the CSI `readOnly`
/// publish flag lives here. They are applied last and remove any operator
/// option in the same family. This is a deliberate behaviour change: `ro`
/// used to be emitted BEFORE the operator's flags, so under last-one-wins
/// an operator `rw` would silently defeat a read-only publish.
pub fn merge(defaults: &[String], user_flags: &[String], forced: &[String]) -> String {
    let user = explode(user_flags);
    let forced_v = explode(forced);

    let forced_fams: HashSet<&str> = forced_v.iter().map(|o| family_of(o)).collect();

    // Operator options, last-wins per family, original order preserved for
    // the surviving occurrence.
    let mut user_kept: Vec<String> = Vec::new();
    for opt in user.iter().rev() {
        let fam = family_of(opt);
        if forced_fams.contains(fam) {
            continue;
        }
        if !user_kept.iter().any(|k| family_of(k) == fam) {
            user_kept.push(opt.clone());
        }
    }
    user_kept.reverse();

    let taken: HashSet<&str> = user_kept
        .iter()
        .map(|o| family_of(o))
        .chain(forced_fams.iter().copied())
        .collect();

    let mut out: Vec<String> = defaults
        .iter()
        .filter(|d| !taken.contains(family_of(d)))
        .cloned()
        .collect();
    out.extend(user_kept);
    out.extend(forced_v);
    out.join(",")
}

/// Read the operator's `mountOptions` off a CSI request, announcing the
/// modes that yield nothing.
///
/// The read used to be a silent `.unwrap_or_default()`. After runax we
/// could not tell "the operator set nothing" from "kubelet did not pass
/// them" from "the kernel ignored them", because nothing on the node said
/// which. Now the empty cases name themselves.
pub fn operator_mount_flags(
    tag: &str,
    vc: Option<&crate::csi::VolumeCapability>,
) -> Vec<String> {
    let Some(vc) = vc else {
        eprintln!("⚠️  [{tag}] request carried NO volume_capability — operator mountOptions cannot be honoured");
        return Vec::new();
    };
    match vc.access_type.as_ref() {
        Some(crate::csi::volume_capability::AccessType::Mount(m)) => {
            if m.mount_flags.is_empty() {
                eprintln!("📋 [{tag}] operator mountOptions: none supplied");
            } else {
                eprintln!("📋 [{tag}] operator mountOptions: {:?}", m.mount_flags);
            }
            m.mount_flags.clone()
        }
        Some(crate::csi::volume_capability::AccessType::Block(_)) => {
            eprintln!("⚠️  [{tag}] Block access type — no mountOptions apply");
            Vec::new()
        }
        None => {
            eprintln!("⚠️  [{tag}] volume_capability carried NO access_type — operator mountOptions cannot be honoured");
            Vec::new()
        }
    }
}

/// Driver defaults for the RWX / single-server NFS publish.
///
/// `sec=sys` is load-bearing: without it the client negotiates AUTH_NULL
/// (this server's SECINFO lists it first), no uid reaches the server, and
/// every file lands owned by root — ownership-sensitive workloads such as
/// postgres then refuse to start on the volume. It is a DEFAULT, not a
/// forced option: an operator asking for `sec=krb5` gets it.
pub fn build_rwx_nfs_mount_opts(readonly: bool, user_flags: &[String]) -> String {
    let defaults: Vec<String> = ["vers=4.2", "noresvport", "sec=sys"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let forced: Vec<String> = if readonly {
        vec!["ro".to_string()]
    } else {
        Vec::new()
    };
    merge(&defaults, user_flags, &forced)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The regression test for the code-proven RWX defect: that path built
    /// its option string as a compile-time literal and never read
    /// `mount_flags` at all, so PV `spec.mountOptions` were discarded for
    /// every RWX volume.
    #[test]
    fn the_rwx_path_honours_operator_mount_options() {
        let opts = build_rwx_nfs_mount_opts(false, &f(&["vers=4.1", "nconnect=8"]));
        assert!(opts.split(',').any(|o| o == "vers=4.1"), "{opts}");
        assert!(
            !opts.split(',').any(|o| o == "vers=4.2"),
            "driver default survived beside the operator pin: {opts}"
        );
        assert!(opts.split(',').any(|o| o == "nconnect=8"), "{opts}");
        assert!(
            opts.split(',').any(|o| o == "sec=sys"),
            "sec=sys is load-bearing and must survive: {opts}"
        );
    }

    /// No existing volume's mount may change. RWX volumes remount on pod
    /// restart, so a refactor that shifted the default string would retune
    /// the fleet silently.
    #[test]
    fn the_rwx_default_string_is_unchanged() {
        assert_eq!(build_rwx_nfs_mount_opts(false, &[]), "vers=4.2,noresvport,sec=sys");
        assert_eq!(build_rwx_nfs_mount_opts(true, &[]), "vers=4.2,noresvport,sec=sys,ro");
    }

    /// A read-only publish is a CSI decision, not an operator preference.
    #[test]
    fn a_read_only_publish_refuses_an_operator_rw() {
        let opts = build_rwx_nfs_mount_opts(true, &f(&["rw", "nconnect=8"]));
        assert!(opts.split(',').any(|o| o == "ro"), "{opts}");
        assert!(
            !opts.split(',').any(|o| o == "rw"),
            "operator rw defeated the CSI readOnly publish: {opts}"
        );
        assert!(
            opts.split(',').any(|o| o == "nconnect=8"),
            "refusing rw must not refuse everything else: {opts}"
        );
    }

    /// Families exist so that a default written in one spelling is
    /// suppressed by an operator option written in another.
    #[test]
    fn the_family_table_covers_every_default_the_driver_emits() {
        for (d, over) in [
            ("vers=4.2", "vers=4.1"),
            ("noresvport", "resvport"),
            ("sec=sys", "sec=krb5"),
        ] {
            assert_eq!(
                family_of(d),
                family_of(over),
                "`{over}` does not share a family with default `{d}`"
            );
        }
        assert_eq!(family_of("tcp"), family_of("proto=tcp"));
        assert_eq!(family_of("rw"), family_of("ro"));
        assert_eq!(family_of("minorversion=2"), family_of("vers=4.1"));
        assert_ne!(
            family_of("nconnect=4"),
            family_of("max_connect=8"),
            "max_connect is a different knob and must not suppress nconnect"
        );
    }

    #[test]
    fn a_repeated_operator_option_resolves_to_the_last_value() {
        let opts = build_rwx_nfs_mount_opts(false, &f(&["nconnect=8", "nconnect=16"]));
        assert!(opts.split(',').any(|o| o == "nconnect=16"), "{opts}");
        assert!(!opts.split(',').any(|o| o == "nconnect=8"), "{opts}");
    }

    #[test]
    fn a_comma_joined_entry_is_split_into_separate_options() {
        let opts = build_rwx_nfs_mount_opts(false, &f(&["hard,vers=4.1,timeo=600"]));
        for want in ["hard", "vers=4.1", "timeo=600"] {
            assert!(opts.split(',').any(|o| o == want), "{want} missing: {opts}");
        }
        assert!(!opts.split(',').any(|o| o == "vers=4.2"), "{opts}");
    }

    #[test]
    fn empty_and_whitespace_entries_are_dropped() {
        let opts = build_rwx_nfs_mount_opts(false, &f(&["", "   ", "hard"]));
        assert!(!opts.contains(",,"), "{opts}");
        assert!(opts.split(',').any(|o| o == "hard"), "{opts}");
    }
}
