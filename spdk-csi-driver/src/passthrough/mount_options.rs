//! `spec.mountOptions`: which mount-s3 flags a CR author may pass, and in
//! what shape (issue #60).
//!
//! The mounter (`mounter::mounter_args_for`) appends `mountOptions` to the
//! argv it builds, so every entry reaches mount-s3. Until #60 the field was
//! a denylist (`-o`, and `--endpoint-url` since #49) and every other flag
//! passed, among them flags that send the worker's traffic or data where
//! `node.endpointAllow` never judges (`--cache-xz`, `--transfer-acceleration`,
//! `--otlp-endpoint`, `--bind`), change its credential or trust store
//! (`--profile`, `--no-sign-request`, `--ca-bundle`), or make it write files
//! and verbose logs (`--log-directory`, `--debug`). Now it is an allowlist,
//! and the list is the OPERATOR's (s3csi/SECURITY.md P13: the operator
//! bounds what a CR author may choose):
//!
//! - [`FLAGS`] is every flag mount-s3 1.24.0 takes (`MOUNT_S3_VERSION` in
//!   `Dockerfile.passthrough`; `mount-s3 --help` from the mounter image),
//!   with its arity, so an entry is read the way mount-s3 reads it.
//! - [`Class::Builtin`] flags are allowed while the operator says nothing:
//!   tuning, modes, and attributes of the objects written to the bucket the
//!   CR already names.
//! - [`Class::Operator`] flags are refused unless the operator lists them in
//!   the S3 CSI chart's `node.mountOptionsAllow`, which REPLACES the built-in
//!   list ([`MountOptionsAllow`]; set but empty, it allows nothing).
//! - [`Class::Field`] flags the mounter sets from a spec field, and
//!   [`Class::Never`] ones it always sets or that stop mount-s3 mounting:
//!   no list admits them, and naming one in the operator's list is a
//!   start-up error.
//!
//! Two passes, both before any worker exists: [`check_shape`] needs no
//! configuration and runs in `MountSpec::validate`; the node driver then
//! judges each flag it returns against its list
//! (`s3csi::node::mount_options_allowed`).

/// How mount-s3 reads a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    /// Takes a value: `--flag VALUE` (two entries) or `--flag=VALUE`.
    Value,
    /// Takes none: `--flag`, alone.
    Switch,
}

/// Who may put a flag in `spec.mountOptions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// On the built-in list: allowed unless the operator's list replaces it.
    Builtin,
    /// Refused unless the operator's list names it.
    Operator,
    /// The mounter sets it from this spec field, so no list admits it (given
    /// a second time, mount-s3 would also exit on the duplicate).
    Field(&'static str),
    /// No list admits it, for this reason.
    Never(&'static str),
}

/// One mount-s3 flag.
#[derive(Debug, PartialEq, Eq)]
pub struct Flag {
    pub long: &'static str,
    pub short: Option<&'static str>,
    pub arity: Arity,
    pub class: Class,
}

const fn f(long: &'static str, short: Option<&'static str>, arity: Arity, class: Class) -> Flag {
    Flag { long, short, arity, class }
}

const FOREGROUND: &str =
    "the mounter always passes it: the worker runs mount-s3 in the foreground as its own child";
const ALLOW_OTHER: &str =
    "the mounter always passes it: without it mount-s3 refuses every uid but its own, \
     the pod's and the plugin's readiness probe included";
const PRINTS_AND_EXITS: &str = "mount-s3 would print and exit instead of mounting";

use Arity::{Switch, Value};
use Class::{Builtin, Field, Never, Operator};

/// Every flag of mount-s3 1.24.0, in `mount-s3 --help`'s order. Re-read
/// it when `MOUNT_S3_VERSION` moves: a flag a newer mount-s3 adds is
/// refused as unknown until it is classed here.
pub const FLAGS: &[Flag] = &[
    f("--foreground", Some("-f"), Switch, Never(FOREGROUND)),
    f("--help", Some("-h"), Switch, Never(PRINTS_AND_EXITS)),
    f("--version", Some("-V"), Switch, Never(PRINTS_AND_EXITS)),
    // Bucket options
    f("--prefix", None, Value, Field("spec.keyPrefix")),
    f("--region", None, Value, Field("spec.region")),
    f("--endpoint-url", None, Value, Field("spec.endpoint")),
    f("--force-path-style", None, Switch, Field("spec.pathStyle")),
    f("--transfer-acceleration", None, Switch, Operator),
    f("--dual-stack", None, Switch, Operator),
    f("--requester-pays", None, Switch, Builtin),
    f("--bucket-type", None, Value, Builtin),
    f("--storage-class", None, Value, Builtin),
    f("--expected-bucket-owner", None, Value, Builtin),
    f("--sse", None, Value, Builtin),
    f("--sse-kms-key-id", None, Value, Builtin),
    f("--upload-checksums", None, Value, Builtin),
    f("--infer-content-type", None, Switch, Builtin),
    // AWS credentials options
    f("--no-sign-request", None, Switch, Operator),
    f("--profile", None, Value, Operator),
    // Mount options
    f("--read-only", None, Switch, Field("spec.readOnly")),
    f("--allow-delete", None, Switch, Field("spec.readOnly")),
    f("--allow-overwrite", None, Switch, Field("spec.readOnly")),
    f("--incremental-upload", None, Switch, Builtin),
    f("--auto-unmount", None, Switch, Operator),
    f("--allow-root", None, Switch, Operator),
    f("--allow-other", None, Switch, Never(ALLOW_OTHER)),
    f("--uid", None, Value, Field("spec.uid")),
    f("--gid", None, Value, Field("spec.gid")),
    f("--dir-mode", None, Value, Builtin),
    f("--file-mode", None, Value, Builtin),
    // Client options
    f("--maximum-throughput-gbps", None, Value, Builtin),
    f("--max-threads", None, Value, Builtin),
    f("--memory-target", None, Value, Builtin),
    f("--part-size", None, Value, Builtin),
    f("--read-part-size", None, Value, Builtin),
    f("--write-part-size", None, Value, Builtin),
    f("--ca-bundle", None, Value, Operator),
    f("--bind", None, Value, Operator),
    // Logging options
    f("--log-directory", Some("-l"), Value, Operator),
    f("--log-metrics", None, Switch, Builtin),
    f("--otlp-endpoint", None, Value, Operator),
    f("--otlp-export-interval", None, Value, Operator),
    f("--debug", Some("-d"), Switch, Operator),
    f("--debug-crt", None, Switch, Operator),
    f("--no-log", None, Switch, Operator),
    // Caching options
    f("--cache", None, Value, Field("spec.cache")),
    f("--metadata-ttl", None, Value, Builtin),
    f("--negative-metadata-ttl", None, Value, Builtin),
    f("--max-cache-size", None, Value, Field("spec.cache.maxSizeMib")),
    f("--cache-xz", None, Value, Operator),
    // Advanced options
    f("--user-agent-prefix", None, Value, Builtin),
];

/// The flags mount-s3 takes more than once (one network interface each);
/// any other flag given twice makes it exit on the duplicate.
const REPEATABLE: &[&str] = &["--bind"];

/// A flag by either spelling, `--long` or `-s`.
pub fn flag(name: &str) -> Option<&'static Flag> {
    FLAGS.iter().find(|f| f.long == name || f.short == Some(name))
}

/// `--long (-s)`, for messages.
fn spelled(f: &Flag) -> String {
    match f.short {
        Some(s) => format!("{} ({s})", f.long),
        None => f.long.to_string(),
    }
}

/// Read `spec.mountOptions` the way mount-s3 1.24.0 reads its arguments and
/// return the flags it names, in order. Needs no configuration, so it runs
/// with the CR (`MountSpec::validate`); which of the returned flags this
/// node allows is the node driver's to judge. Refused, each by name with
/// what to write instead:
///
/// - `-o` in any spelling: an s3fs option, and mount-s3's own fstab-style
///   `-o` would carry any flag past the list;
/// - a flag the mounter sets from a spec field (named in the refusal), or
///   one it always sets, or `--help`/`--version`;
/// - a flag mount-s3 1.24.0 does not have, and an entry that is neither a
///   flag nor the value of the flag before it (mount-s3 would read it as a
///   third positional and exit), `--` included;
/// - a value flag whose value is missing or starts with `-` (mount-s3 would
///   read it as a flag), a value with a control character (some reach an
///   HTTP header the worker signs, #72), and a value given to a switch;
/// - short flags run together or with a value attached (`-dl`, `-l/x`):
///   one short flag per entry, its value the next entry or after `=`;
/// - a flag given twice, but for `--bind`.
pub fn check_shape(opts: &[String]) -> Result<Vec<&'static Flag>, String> {
    let mut flags: Vec<&'static Flag> = Vec::new();
    let mut i = 0;
    while let Some(o) = opts.get(i).map(String::as_str) {
        i += 1;
        if o == "--" {
            return Err("spec.mountOptions contains \"--\" — mount-s3 would read every entry after it as a \
                        positional and exit; drop it"
                .into());
        }
        let (name, inline): (&str, Option<&str>) = if o.starts_with("--") {
            match o.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (o, None),
            }
        } else if let Some(rest) = o.strip_prefix('-').filter(|r| !r.is_empty()) {
            // `-o` here is an s3fs-shaped option that outlived s3fs. Caught
            // because of what it cost: mount-s3 exited on it, and the pod was
            // a PRIVILEGED sidecar in CrashLoopBackOff whose reason existed
            // only in a container log (measured on kind 2026-08-27, from a CR
            // written before the driver field was removed). And mount-s3
            // 1.24.0 does read `-o`, as fstab-style options (`mount-s3
            // <BUCKET> <DIRECTORY> -o <OPTIONS>`): a valid one would set
            // flags this list never sees. So every spelling is refused, `-o
            // x`, `-o=x` and `-ox`.
            if let Some(attached) = rest.strip_prefix('o') {
                let val = if attached.is_empty() {
                    opts.get(i).map(String::as_str).unwrap_or("")
                } else {
                    attached.strip_prefix('=').unwrap_or(attached)
                };
                return Err(format!(
                    "spec.mountOptions contains \"-o\" (\"{val}\") — that is an s3fs option, and Mountpoint \
                     for S3, which this mounts with, reads -o as fstab-style options that would carry any \
                     flag past the operator's list. Drop it, or write the mount-s3 --flag equivalent"
                ));
            }
            let end = 1 + rest.chars().next().map_or(0, char::len_utf8);
            match o[end..].strip_prefix('=') {
                _ if o.len() == end => (o, None),
                Some(v) => (&o[..end], Some(v)),
                None => {
                    return Err(format!(
                        "spec.mountOptions contains {o:?} — write one short flag per entry, and its value as \
                         the next entry or after = (\"-l\", \"/dir\" or \"-l=/dir\")"
                    ))
                }
            }
        } else {
            let after = match i {
                1 => String::new(),
                _ => format!(" (after {:?})", opts[i - 2]),
            };
            return Err(format!(
                "spec.mountOptions has {o:?} where a flag belongs{after} — a value goes in the entry right \
                 after the flag that takes it ([\"--metadata-ttl\", \"60\"]) or after = (\"--metadata-ttl=60\"), \
                 and mount-s3 would read anything else as a third positional and exit"
            ));
        };
        if name.contains(char::is_whitespace) {
            return Err(format!(
                "spec.mountOptions entry {o:?} has a space in it — a flag and its value are two entries \
                 ([\"--metadata-ttl\", \"60\"]) or one joined by = (\"--metadata-ttl=60\")"
            ));
        }
        let Some(flag) = flag(name) else {
            return Err(format!(
                "spec.mountOptions contains {name:?}, which is not a flag of mount-s3 1.24.0 (the mounter's) — \
                 mount-s3 would exit on it; drop it or fix the spelling"
            ));
        };
        match flag.class {
            // The endpoint is spec.endpoint's to name: node.endpointAllow
            // judges that field, and the mounter passes it as --endpoint-url.
            // The same flag in mountOptions would send the worker, signing
            // with the mount's credential, to a host the list never judged
            // (issue #49); beside spec.endpoint, mount-s3 exits on the
            // duplicate flag.
            Field("spec.endpoint") => {
                return Err(format!(
                    "spec.mountOptions sets the endpoint ({o:?}) — name it in spec.endpoint, which \
                     node.endpointAllow judges"
                ))
            }
            Field(field) => {
                return Err(format!(
                    "spec.mountOptions sets {} ({o:?}) — the mounter sets that flag from {field}; set {field} \
                     instead",
                    flag.long
                ))
            }
            Never(why) => return Err(format!("spec.mountOptions contains {o:?} — {why}; drop it")),
            Builtin | Operator => {}
        }
        match flag.arity {
            Switch => {
                if inline.is_some() {
                    return Err(format!(
                        "spec.mountOptions gives {} a value ({o:?}) — it is a switch and takes none; write \
                         {name} alone",
                        flag.long
                    ));
                }
            }
            Value => {
                let value = match inline {
                    Some(v) => Some(v),
                    None => {
                        i += 1;
                        opts.get(i - 1).map(String::as_str)
                    }
                };
                match value {
                    None | Some("") => {
                        return Err(format!(
                            "spec.mountOptions gives {} no value — it takes one: [\"{name}\", \"<value>\"] or \
                             \"{name}=<value>\"",
                            flag.long
                        ))
                    }
                    Some(v) if v.starts_with('-') => {
                        return Err(format!(
                            "spec.mountOptions gives {} the value {v:?} — a value that starts with - reads as a \
                             flag (did {name} lose its value?); give {name} its own value",
                            flag.long
                        ))
                    }
                    // A value is plain text. Some reach an HTTP header on every
                    // request the worker signs (--user-agent-prefix,
                    // --storage-class, --sse, --sse-kms-key-id,
                    // --expected-bucket-owner), where a CR or LF would try to
                    // add headers to them (issue #72). No mount-s3 value has a
                    // use for a control character.
                    Some(v) if v.chars().any(char::is_control) => {
                        return Err(format!(
                            "spec.mountOptions gives {} a value with a control character ({v:?}) — a value is \
                             plain text, and some reach an HTTP header the worker signs; remove it",
                            flag.long
                        ))
                    }
                    Some(_) => {}
                }
            }
        }
        if !REPEATABLE.contains(&flag.long) && flags.iter().any(|seen| seen.long == flag.long) {
            return Err(format!(
                "spec.mountOptions sets {} twice — mount-s3 takes it once and would exit on the second; \
                 keep one",
                spelled(flag)
            ));
        }
        flags.push(flag);
    }
    Ok(flags)
}

/// The flags a CR's `spec.mountOptions` may carry on this node:
/// `node.mountOptionsAllow` (FLINT_S3CSI_MOUNT_OPTIONS_ALLOW). Unset, the
/// built-in list ([`MountOptionsAllow::builtin`]); set, the operator's list
/// REPLACES it, and set but empty allows nothing — deliberately unlike
/// `node.endpointAllow`, where empty admits any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountOptionsAllow {
    /// Long spellings, in the order given.
    longs: Vec<&'static str>,
    /// Whether the operator's list replaced the built-in one.
    operator: bool,
}

impl MountOptionsAllow {
    /// What a node allows while its operator says nothing: every
    /// [`Class::Builtin`] flag.
    pub fn builtin() -> Self {
        MountOptionsAllow {
            longs: FLAGS.iter().filter(|f| f.class == Builtin).map(|f| f.long).collect(),
            operator: false,
        }
    }

    /// The operator's list, in place of the built-in one. Each entry is
    /// spelled as mount-s3 takes it, `--debug` or `-d`; none is an empty
    /// list, which allows nothing. A flag mount-s3 1.24.0 does not have, or
    /// one no list may admit (the mounter sets it, or it stops mount-s3
    /// mounting), is an error by name — at start-up, not at a tenant's
    /// publish.
    pub fn operator<S: AsRef<str>>(entries: &[S]) -> Result<Self, String> {
        let mut longs: Vec<&'static str> = Vec::new();
        for e in entries.iter().map(AsRef::as_ref) {
            if e == "-o" {
                return Err("-o can never be allowed: mount-s3 reads it as fstab-style options, which would \
                            carry any flag past this list"
                    .into());
            }
            if e.contains('=') {
                return Err(format!(
                    "{e:?} carries a value — list the flag alone, e.g. --metadata-ttl"
                ));
            }
            let Some(f) = flag(e) else {
                return Err(format!(
                    "{e:?} is not a flag of mount-s3 1.24.0 (the mounter's) — spell it as mount-s3 takes it, \
                     e.g. --metadata-ttl or -d"
                ));
            };
            match f.class {
                Field(field) => {
                    return Err(format!(
                        "{e} can never be allowed: the mounter sets it from {field}"
                    ))
                }
                Never(why) => return Err(format!("{e} can never be allowed: {why}")),
                Builtin | Operator => {}
            }
            if !longs.contains(&f.long) {
                longs.push(f.long);
            }
        }
        Ok(MountOptionsAllow { longs, operator: true })
    }

    pub fn admits(&self, f: &Flag) -> bool {
        self.longs.contains(&f.long)
    }

    /// The allowed flags, long spellings.
    pub fn flags(&self) -> &[&'static str] {
        &self.longs
    }

    /// The list as a refusal names it.
    pub fn describe(&self) -> String {
        match (self.operator, self.longs.is_empty()) {
            (false, _) => format!("unset, so the built-in list: {}", self.longs.join(" ")),
            (true, true) => "set and empty, so no flag is allowed".into(),
            (true, false) => format!("\"{}\"", self.longs.join(" ")),
        }
    }
}

/// Each flag `check_shape` reads in `opts` must be on `allow`.
pub fn check_allowed(opts: &[String], allow: &MountOptionsAllow) -> Result<(), String> {
    for f in check_shape(opts)? {
        if !allow.admits(f) {
            return Err(format!(
                "spec.mountOptions contains {}, which this node driver does not allow: node.mountOptionsAllow \
                 (FLINT_S3CSI_MOUNT_OPTIONS_ALLOW) is {} — drop it, or have the operator add it",
                spelled(f),
                allow.describe()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(opts: &[&str]) -> Vec<String> {
        opts.iter().map(|o| o.to_string()).collect()
    }

    fn longs(opts: &[&str]) -> Vec<&'static str> {
        check_shape(&v(opts))
            .unwrap_or_else(|e| panic!("{opts:?}: {e}"))
            .iter()
            .map(|f| f.long)
            .collect()
    }

    /// The decided default (issue #60): tuning, modes and attributes of the
    /// objects written, plus --log-metrics. Written out here, not derived
    /// from the table, so a flag moved in or out of the list is a failure.
    const DEFAULT_VALUE_FLAGS: &[&str] = &[
        "--metadata-ttl",
        "--negative-metadata-ttl",
        "--memory-target",
        "--max-threads",
        "--maximum-throughput-gbps",
        "--part-size",
        "--read-part-size",
        "--write-part-size",
        "--file-mode",
        "--dir-mode",
        "--storage-class",
        "--sse",
        "--sse-kms-key-id",
        "--upload-checksums",
        "--bucket-type",
        "--expected-bucket-owner",
        "--user-agent-prefix",
    ];
    const DEFAULT_SWITCHES: &[&str] =
        &["--infer-content-type", "--incremental-upload", "--requester-pays", "--log-metrics"];
    /// Off the default list on purpose; the operator may add them.
    const OPERATOR_FLAGS: &[&str] = &[
        "--cache-xz",
        "--transfer-acceleration",
        "--dual-stack",
        "--otlp-endpoint",
        "--otlp-export-interval",
        "--bind",
        "--profile",
        "--no-sign-request",
        "--ca-bundle",
        "--log-directory",
        "--debug",
        "--debug-crt",
        "--allow-root",
        "--auto-unmount",
        "--no-log",
    ];
    /// Set by the mounter from a spec field: (flag, the field the refusal names).
    const FIELD_FLAGS: &[(&str, &str)] = &[
        ("--prefix", "spec.keyPrefix"),
        ("--region", "spec.region"),
        ("--endpoint-url", "spec.endpoint"),
        ("--force-path-style", "spec.pathStyle"),
        ("--read-only", "spec.readOnly"),
        ("--allow-delete", "spec.readOnly"),
        ("--allow-overwrite", "spec.readOnly"),
        ("--uid", "spec.uid"),
        ("--gid", "spec.gid"),
        ("--cache", "spec.cache"),
        ("--max-cache-size", "spec.cache.maxSizeMib"),
    ];
    const NEVER_FLAGS: &[&str] =
        &["--foreground", "-f", "--allow-other", "--help", "-h", "--version", "-V"];

    /// The table is mount-s3 1.24.0's `--help`, every flag once: 51 long
    /// names, the short forms -f -h -V -l -d, and each class exactly the
    /// decided set. A newer mount-s3 changes this test, which is the point.
    #[test]
    fn the_table_is_every_flag_of_mount_s3_1_24_0_classed_as_decided() {
        assert_eq!(FLAGS.len(), 51);
        for (i, a) in FLAGS.iter().enumerate() {
            assert!(a.long.starts_with("--") && a.long.len() > 3, "{a:?}");
            for b in &FLAGS[i + 1..] {
                assert_ne!(a.long, b.long);
                assert!(a.short.is_none() || a.short != b.short, "{a:?} {b:?}");
            }
        }
        let shorts: Vec<(&str, &str)> =
            FLAGS.iter().filter_map(|f| f.short.map(|s| (s, f.long))).collect();
        assert_eq!(
            shorts,
            vec![
                ("-f", "--foreground"),
                ("-h", "--help"),
                ("-V", "--version"),
                ("-l", "--log-directory"),
                ("-d", "--debug")
            ]
        );
        let class_of = |c: fn(&Class) -> bool| -> Vec<&str> {
            let mut l: Vec<&str> = FLAGS.iter().filter(|f| c(&f.class)).map(|f| f.long).collect();
            l.sort();
            l
        };
        let sorted = |l: &[&'static str]| -> Vec<&'static str> {
            let mut l = l.to_vec();
            l.sort();
            l
        };
        assert_eq!(
            class_of(|c| *c == Class::Builtin),
            sorted(&[DEFAULT_VALUE_FLAGS, DEFAULT_SWITCHES].concat())
        );
        assert_eq!(class_of(|c| *c == Class::Operator), sorted(OPERATOR_FLAGS));
        assert_eq!(
            class_of(|c| matches!(c, Class::Field(_))),
            sorted(&FIELD_FLAGS.iter().map(|(f, _)| *f).collect::<Vec<_>>())
        );
        assert_eq!(
            class_of(|c| matches!(c, Class::Never(_))),
            sorted(&["--foreground", "--allow-other", "--help", "--version"])
        );
        for &d in DEFAULT_VALUE_FLAGS {
            assert_eq!(flag(d).unwrap().arity, Arity::Value, "{d}");
        }
        for &d in DEFAULT_SWITCHES {
            assert_eq!(flag(d).unwrap().arity, Arity::Switch, "{d}");
        }
        assert_eq!(MountOptionsAllow::builtin().flags().len(), 21);
    }

    /// Every flag on the default list passes, value flags in both forms;
    /// the documented uses (--metadata-ttl, --memory-target, --file-mode,
    /// --dir-mode) are among them; an empty list passes any allowlist.
    #[test]
    fn the_default_list_admits_tuning_modes_and_object_attributes_in_both_forms() {
        let allow = MountOptionsAllow::builtin();
        for &d in DEFAULT_VALUE_FLAGS {
            for opts in [v(&[d, "1"]), vec![format!("{d}=1")]] {
                check_allowed(&opts, &allow).unwrap_or_else(|e| panic!("{opts:?}: {e}"));
            }
        }
        for &d in DEFAULT_SWITCHES {
            check_allowed(&v(&[d]), &allow).unwrap_or_else(|e| panic!("{d}: {e}"));
        }
        let all = v(&[
            "--metadata-ttl",
            "60",
            "--memory-target=900",
            "--file-mode",
            "0755",
            "--dir-mode=0775",
            "--infer-content-type",
            "--sse",
            "aws:kms",
            "--sse-kms-key-id",
            "arn:aws:kms:us-east-1:1:key/k",
        ]);
        assert_eq!(
            longs(&all.iter().map(String::as_str).collect::<Vec<_>>()),
            vec![
                "--metadata-ttl",
                "--memory-target",
                "--file-mode",
                "--dir-mode",
                "--infer-content-type",
                "--sse",
                "--sse-kms-key-id"
            ]
        );
        assert!(check_allowed(&all, &allow).is_ok());
        assert!(
            check_allowed(&[], &MountOptionsAllow::operator::<&str>(&[]).unwrap()).is_ok(),
            "no options, nothing to allow"
        );
    }

    /// A flag the mounter sets from a spec field is refused naming that
    /// field, in every form, whatever the list; --endpoint-url keeps #49's
    /// message.
    #[test]
    fn a_flag_the_mounter_sets_from_a_spec_field_is_refused_naming_the_field() {
        for &(fl, field) in FIELD_FLAGS {
            let forms = match flag(fl).unwrap().arity {
                Arity::Value => vec![
                    v(&[fl, "x"]),
                    vec![format!("{fl}=x")],
                    v(&["--metadata-ttl", "60", fl, "x"]),
                ],
                Arity::Switch => vec![v(&[fl]), v(&["--metadata-ttl", "60", fl])],
            };
            for opts in forms {
                let e = check_shape(&opts).expect_err(fl);
                assert!(e.contains(field), "{opts:?} must name {field}: {e}");
                let all_allowed = MountOptionsAllow::operator(OPERATOR_FLAGS).unwrap();
                assert!(check_allowed(&opts, &all_allowed).is_err(), "no list admits {fl}");
            }
        }
        let e = check_shape(&v(&["--endpoint-url=http://other-s3:9000"])).unwrap_err();
        assert!(
            e.starts_with("spec.mountOptions sets the endpoint")
                && e.contains("node.endpointAllow"),
            "{e}"
        );
        let e = check_shape(&v(&["--allow-delete"])).unwrap_err();
        assert!(e.contains("set spec.readOnly instead"), "{e}");
    }

    /// --foreground, --allow-other, --help and --version are never allowed,
    /// short forms included, and naming one (or a spec-field flag, or -o)
    /// in the operator's list is an error by name.
    #[test]
    fn never_allowable_flags_are_refused_and_cannot_be_listed() {
        for &n in NEVER_FLAGS {
            let e = check_shape(&v(&[n])).expect_err(n);
            assert!(e.contains(n) && e.contains("drop it"), "{e}");
            let e = MountOptionsAllow::operator(&[n]).expect_err(n);
            assert!(e.contains(n) && e.contains("can never be allowed"), "{e}");
        }
        assert!(check_shape(&v(&["--help"])).unwrap_err().contains("print and exit"));
        assert!(check_shape(&v(&["--allow-other"])).unwrap_err().contains("always passes it"));
        for &(fl, field) in FIELD_FLAGS {
            let e = MountOptionsAllow::operator(&[fl]).expect_err(fl);
            assert!(
                e.contains(fl) && e.contains(field) && e.contains("can never be allowed"),
                "{e}"
            );
        }
        assert!(MountOptionsAllow::operator(&["-o"]).unwrap_err().contains("can never be allowed"));
    }

    /// The flags off the default list are refused by default, naming the
    /// flag and the knob; an operator who lists one admits it, by either
    /// spelling.
    #[test]
    fn operator_flags_are_refused_by_default_and_admitted_once_listed() {
        let builtin = MountOptionsAllow::builtin();
        for &fl in OPERATOR_FLAGS {
            let opts = match flag(fl).unwrap().arity {
                Arity::Value => v(&[fl, "x"]),
                Arity::Switch => v(&[fl]),
            };
            assert!(check_shape(&opts).is_ok(), "{fl}: the shape is fine");
            let e = check_allowed(&opts, &builtin).expect_err(fl);
            assert!(
                e.contains(fl)
                    && e.contains("node.mountOptionsAllow")
                    && e.contains("FLINT_S3CSI_MOUNT_OPTIONS_ALLOW"),
                "{e}"
            );
            assert!(
                e.contains("unset, so the built-in list: ") && e.contains("--metadata-ttl"),
                "{e}"
            );
            let listed = MountOptionsAllow::operator(&[fl]).unwrap();
            assert!(check_allowed(&opts, &listed).is_ok(), "{fl} listed");
        }
        let e = check_allowed(&v(&["--cache-xz", "xz--usw2-az1--x-s3"]), &builtin).unwrap_err();
        assert!(
            e.starts_with(
                "spec.mountOptions contains --cache-xz, which this node driver does not allow"
            ),
            "{e}"
        );
        // Listed by its short form, given by its long one, and the reverse.
        let short = MountOptionsAllow::operator(&["-d", "-l"]).unwrap();
        assert_eq!(short.flags(), &["--debug", "--log-directory"]);
        assert!(check_allowed(&v(&["--debug", "--log-directory", "/tmp/l"]), &short).is_ok());
        let long = MountOptionsAllow::operator(&["--debug", "--log-directory", "--debug"]).unwrap();
        assert_eq!(long.flags(), &["--debug", "--log-directory"], "duplicates fold");
        assert!(check_allowed(&v(&["-d", "-l", "/tmp/l"]), &long).is_ok());
        let e = check_allowed(&v(&["-d"]), &builtin).unwrap_err();
        assert!(e.contains("--debug (-d)"), "a refusal names both spellings: {e}");
    }

    /// The operator's list REPLACES the default one, and an empty list
    /// allows nothing at all, the default flags included.
    #[test]
    fn an_operator_list_replaces_the_default_and_an_empty_one_allows_nothing() {
        let only_debug = MountOptionsAllow::operator(&["--debug"]).unwrap();
        let e = check_allowed(&v(&["--metadata-ttl", "60"]), &only_debug).unwrap_err();
        assert!(e.contains("--metadata-ttl") && e.contains("\"--debug\""), "{e}");
        let none = MountOptionsAllow::operator::<&str>(&[]).unwrap();
        assert!(none.flags().is_empty());
        for &d in DEFAULT_VALUE_FLAGS {
            let e = check_allowed(&v(&[d, "1"]), &none).expect_err(d);
            assert!(e.contains("set and empty, so no flag is allowed"), "{e}");
        }
        for &d in DEFAULT_SWITCHES {
            assert!(check_allowed(&v(&[d]), &none).is_err(), "{d}");
        }
        assert_ne!(none, MountOptionsAllow::builtin());
    }

    /// An entry in the operator's list that is not a mount-s3 1.24.0 flag
    /// as mount-s3 spells it is an error naming it.
    #[test]
    fn an_unknown_entry_in_the_operator_list_is_an_error_by_name() {
        for bad in
            ["--metadata-tll", "metadata-ttl", "--s3fs-thing", "-x", "--metadata-ttl=60", "-"]
        {
            let e = MountOptionsAllow::operator(&[bad]).expect_err(bad);
            assert!(e.contains(bad), "{bad}: {e}");
        }
        assert!(MountOptionsAllow::operator(&["--metadata-tll"])
            .unwrap_err()
            .contains("not a flag of mount-s3 1.24.0"));
    }

    /// `--flag value` and `--flag=value` read the same; a value that is
    /// missing, empty, or starts with - (it would be read as a flag) is
    /// refused naming the flag that lost it.
    #[test]
    fn a_value_flag_needs_a_value_that_does_not_look_like_a_flag() {
        assert_eq!(longs(&["--metadata-ttl", "60"]), vec!["--metadata-ttl"]);
        assert_eq!(longs(&["--metadata-ttl=60"]), vec!["--metadata-ttl"]);
        assert_eq!(
            longs(&["--user-agent-prefix=a=b"]),
            vec!["--user-agent-prefix"],
            "only the first = splits"
        );
        for opts in [
            v(&["--metadata-ttl"]),
            v(&["--metadata-ttl="]),
            v(&["--metadata-ttl", ""]),
            v(&["--max-threads", "8", "--part-size"]),
        ] {
            let e = check_shape(&opts).expect_err("missing value");
            assert!(e.contains("no value") && e.contains("takes one"), "{opts:?}: {e}");
        }
        for opts in [
            v(&["--metadata-ttl", "--file-mode", "0640"]),
            v(&["--metadata-ttl", "-1"]),
            v(&["--metadata-ttl=-1"]),
            v(&["--user-agent-prefix", "--endpoint-url=http://evil:9000"]),
            v(&["--storage-class", "-o"]),
        ] {
            let e = check_shape(&opts).expect_err("a dashed value");
            assert!(
                e.contains("reads as a flag")
                    && e.contains(&opts[0].split('=').next().unwrap().to_string()),
                "{opts:?}: {e}"
            );
        }
    }

    /// A switch takes no value: `--flag=value` is refused, and a value in
    /// the next entry is a stray positional.
    /// Issue #72: a value is plain text. Each flag whose value reaches an
    /// HTTP header the worker signs, given CR/LF (a header injection), LF,
    /// CR, TAB, ESC, DEL or a C1 control in either form, is refused by name
    /// with the value shown escaped; the same flags with a plain value pass.
    #[test]
    fn a_value_with_a_control_character_is_refused() {
        const HEADER_FLAGS: &[&str] = &[
            "--user-agent-prefix",
            "--storage-class",
            "--sse",
            "--sse-kms-key-id",
            "--expected-bucket-owner",
        ];
        for fl in HEADER_FLAGS {
            for bad in [
                "a\r\nx-amz-acl: public-read",
                "a\nb",
                "a\rb",
                "a\tb",
                "a\u{1b}[31m",
                "a\u{7f}",
                "a\u{85}b",
            ] {
                for opts in [v(&[*fl, bad]), vec![format!("{fl}={bad}")]] {
                    let e = check_shape(&opts).expect_err(&format!("{opts:?} must be refused"));
                    assert!(e.contains(fl) && e.contains("a control character"), "{opts:?}: {e}");
                    assert!(
                        !e.chars().any(char::is_control),
                        "the refusal shows the value escaped: {e:?}"
                    );
                }
            }
            assert_eq!(longs(&[*fl, "a plain value"]), vec![*fl], "{fl} with a plain value");
        }
    }

    #[test]
    fn a_switch_takes_no_value() {
        for opts in [v(&["--infer-content-type=true"]), v(&["--requester-pays="]), v(&["-d=1"])] {
            let e = check_shape(&opts).expect_err("a switch with a value");
            assert!(e.contains("is a switch and takes none"), "{opts:?}: {e}");
        }
        let e = check_shape(&v(&["--infer-content-type", "true"])).unwrap_err();
        assert!(
            e.contains("\"true\" where a flag belongs (after \"--infer-content-type\")"),
            "{e}"
        );
    }

    /// An entry that is neither a flag nor the value of the flag before it
    /// would be mount-s3's third positional: refused by name, `--` and the
    /// empty string included; so is an unknown flag, and a flag written
    /// with its value in one entry.
    #[test]
    fn a_stray_positional_or_an_unknown_flag_is_refused_by_name() {
        for opts in [
            v(&["60"]),
            v(&["--metadata-ttl", "60", "60"]),
            v(&[""]),
            v(&["-"]),
            v(&["s3://other-bucket"]),
        ] {
            let e = check_shape(&opts).expect_err("stray");
            assert!(
                e.contains("where a flag belongs") && e.contains("third positional"),
                "{opts:?}: {e}"
            );
        }
        assert!(check_shape(&v(&["--", "x"])).unwrap_err().contains("\"--\""));
        for bad in ["--endpoint-urls-are-not-a-flag", "--metadata-tll", "-x", "--cache-xzz"] {
            let e = check_shape(&v(&[bad])).expect_err(bad);
            assert!(e.contains(bad) && e.contains("not a flag of mount-s3 1.24.0"), "{e}");
        }
        let e = check_shape(&v(&["--metadata-ttl 60"])).unwrap_err();
        assert!(e.contains("has a space in it"), "{e}");
    }

    /// Short forms are read as mount-s3 reads them, one per entry, the value
    /// in the next entry or after =; run together or attached, refused.
    #[test]
    fn short_forms_are_read_one_per_entry() {
        assert_eq!(longs(&["-d"]), vec!["--debug"]);
        assert_eq!(longs(&["-l", "/tmp/logs"]), vec!["--log-directory"]);
        assert_eq!(longs(&["-l=/tmp/logs", "-d"]), vec!["--log-directory", "--debug"]);
        assert!(check_shape(&v(&["-l"])).unwrap_err().contains("no value"));
        assert!(check_shape(&v(&["-l", "-d"])).unwrap_err().contains("reads as a flag"));
        for bad in ["-dl", "-l/tmp/logs", "-df"] {
            let e = check_shape(&v(&[bad])).expect_err(bad);
            assert!(e.contains("one short flag per entry"), "{bad}: {e}");
        }
        assert!(check_shape(&v(&["-f"])).unwrap_err().contains("always passes it"));
        assert!(check_shape(&v(&["-V"])).unwrap_err().contains("print and exit"));
    }

    /// -o is s3fs's, and mount-s3's own fstab-style options: refused in
    /// every spelling, naming the value it carried.
    #[test]
    fn the_o_option_is_refused_in_every_spelling() {
        for (opts, val) in [
            (v(&["-o", "allow_other"]), "allow_other"),
            (v(&["-o=endpoint-url=http://x"]), "endpoint-url=http://x"),
            (v(&["-oro"]), "ro"),
            (v(&["--metadata-ttl", "60", "-o"]), ""),
        ] {
            let e = check_shape(&opts).expect_err("-o");
            assert!(
                e.contains(&format!("\"-o\" (\"{val}\")")) && e.contains("s3fs option"),
                "{opts:?}: {e}"
            );
        }
    }

    /// mount-s3 exits on a flag given twice; --bind is the one it repeats.
    #[test]
    fn a_flag_given_twice_is_refused_but_bind_repeats() {
        for opts in [
            v(&["--metadata-ttl", "60", "--metadata-ttl=30"]),
            v(&["--infer-content-type", "--infer-content-type"]),
            v(&["-d", "--debug"]),
        ] {
            let e = check_shape(&opts).expect_err("twice");
            assert!(e.contains("twice"), "{opts:?}: {e}");
        }
        assert_eq!(longs(&["--bind", "eth0", "--bind=eth1"]), vec!["--bind", "--bind"]);
    }
}
