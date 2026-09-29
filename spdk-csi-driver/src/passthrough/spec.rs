//! The `FlintPassthroughMount` spec — pure configuration, read at
//! admission time and never reconciled.
//!
//! There is no controller behind this CR and no status subresource,
//! because there is nothing to converge: a passthrough mount owns no
//! bucket state, takes no claim, keeps no manifest and holds no lease.
//! The whole product is "an S3 prefix appears as a directory in this
//! pod", so the CR is the argument list for a `mount-s3` command and
//! the `s3.csi.chert.us` node plugin is the only reader. Anything that
//! needs a control loop
//! belongs in flint-lean, not here.
//!
//! THERE IS ONE MOUNTER, ON PURPOSE. Mountpoint for S3, always: fast
//! sequential reads, and a write model that is sequential writes to
//! whole objects and nothing else — no rename, no append, no in-place
//! modification, at any setting. A front end that also shipped a POSIX
//! *emulation* (s3fs, goofys) would be offering a working tree it
//! cannot actually keep: uncoordinated, last-writer-wins, undetected.
//! A pod that wants `git`, `pip install` or sqlite wants flint-lean,
//! whose publish boundary is the thing that makes those safe.
//!
//! The type is plain serde over the CR's `spec` object (the node plugin
//! fetches the CR as a `DynamicObject`), so the CRD schema in
//! `flint-passthrough-chart/crds/` is the single source of truth for
//! validation the API server performs, and [`MountSpec::validate`] is
//! the source of truth for what the injector refuses. Both exist on
//! purpose: the CRD stops a bad CR from being stored, and `validate`
//! stops a CR stored by an older schema from reaching a shell.

use serde::{Deserialize, Serialize};

fn default_mount_path() -> String {
    "/mnt/s3".into()
}

/// Mountpoint's local block cache. See [`MountSpec::cache`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CacheSpec {
    #[serde(default)]
    pub enabled: bool,
    /// The cache ceiling in MiB. Required when `enabled`, because the
    /// unbounded default would fill the worker's emptyDir and get it
    /// EVICTED — and an evicted passthrough worker cannot be restarted.
    #[serde(default)]
    pub max_size_mib: Option<u64>,
}

/// One mounter per node for the CR's read-only consumers. See
/// [`MountSpec::sharing`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SharingSpec {
    #[serde(default)]
    pub read_only: bool,
}

// Serialize is here for ONE reason: it is what lets
// `the_crd_and_the_struct_agree_on_every_field` enumerate this
// struct's fields at runtime and compare them against the hand-written
// CRD. Nothing in the product serializes a MountSpec.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MountSpec {
    pub bucket: String,
    #[serde(default)]
    pub key_prefix: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default = "default_mount_path")]
    pub mount_path: String,
    #[serde(default)]
    pub read_only: bool,
    /// Path-style addressing. Unset means "true when `endpoint` is
    /// set" — every self-hosted gateway (MinIO, the lean proxy, Ceph
    /// RGW) needs it, and AWS proper does not.
    #[serde(default)]
    pub path_style: Option<bool>,
    /// A Secret whose keys are AWS_* VERBATIM, the same shape
    /// flint-lean's `credentialsSecretRef` takes. Unset means the
    /// ambient chain — IRSA, instance profile, anything the AWS SDK
    /// resolves on its own, which mount-s3 uses natively.
    #[serde(default)]
    pub credentials_secret_ref: Option<String>,
    #[serde(default)]
    pub uid: Option<i64>,
    #[serde(default)]
    pub gid: Option<i64>,
    /// Extra mounter arguments, passed through as ARGV — never
    /// concatenated into a shell string. See `mounter::mounter_args_for`.
    #[serde(default)]
    pub mount_options: Vec<String>,
    /// Mountpoint's local block cache, in the worker's own `scratch`
    /// emptyDir (`/tmp`, `workers.scratchSize`, default 1Gi). OFF by
    /// default and opt-in per mount.
    ///
    /// The emptyDir was provisioned and documented as "mount-s3's cache"
    /// from the start and `--cache` was never passed, so every repeated
    /// read re-fetched from S3 and the gigabyte was dead weight. For a
    /// dataset mounted read-only and read many times — the flagship
    /// case — this is the largest single performance lever available.
    ///
    /// `maxSizeMib` must leave room under `workers.scratchSize`: the
    /// emptyDir's `sizeLimit` EVICTS the worker pod when exceeded, and
    /// an evicted passthrough worker is an unrecoverable mount
    /// (`restartPolicy: Never`).
    #[serde(default)]
    pub cache: Option<CacheSpec>,
    /// `sharing.readOnly: true` — every READ-ONLY consumer of this CR on
    /// one node shares ONE mounter (one worker pod, one FUSE mount, one
    /// block cache, one connection pool) instead of each pod getting its
    /// own; each pod's target is a bind of that mount. OFF by default.
    /// Design of record: docs/plans/passthrough-read-only-mount-sharing.md.
    ///
    /// A sharing CR that names no `cache` gets one by default: three
    /// quarters of `workers.scratchSize` ([`MountSpec::with_default_cache`]);
    /// `cache: { enabled: false }` opts out, `cache.maxSizeMib` chooses.
    ///
    /// Who shares: pods whose effective access is read (the CR's
    /// `readOnly`, or an SA in `readOnlyServiceAccounts`, or the pod's own
    /// `readOnly: true`) with the same effective uid/gid, on `identity.mode`
    /// broker (with a broker whose backend scopes a read grant by the CR:
    /// sts or static) or ambient. A read-write consumer keeps its own
    /// mounter. `static` is refused with this set: that key is the pod's
    /// own Secret.
    ///
    /// What it costs, so the choice is informed: one mounter's death
    /// strands every member (not one pod); one memory limit and one
    /// prefetch budget serve them all; and a member's grant cannot be
    /// revoked on its own before its pod exits — every member holds the
    /// same authority, which is why sharing grants nothing, and also why
    /// the mount cannot tell members apart. Per-member registrations and
    /// credential exchanges are still logged by the broker as before.
    #[serde(default)]
    pub sharing: Option<SharingSpec>,
    /// Per-mount image override (the chart's default otherwise).
    /// WEBHOOK DELIVERY ONLY: the CSI node driver never reads it — the
    /// worker image is chart-pinned, because this field is the
    /// privileged-escape knob (design §2.3 T3).
    #[serde(default)]
    pub image: Option<String>,
    /// CSI delivery: which ServiceAccounts in this namespace may mount
    /// the CR, read-write or read-only. ABSENT = DENY — never "any pod in
    /// this namespace".
    #[serde(default)]
    pub consumers: Option<crate::s3csi::policy::MountConsumers>,
    /// CSI delivery: how the worker gets its credential (design §4.4).
    #[serde(default)]
    pub identity: Option<crate::s3csi::policy::Identity>,
    // NO per-mount `resources`. There was a field here and it was dead:
    // the webhook injector took the mounter's resources from the CHART
    // and never looked at the CR's, so a spec that set it would have
    // been accepted and ignored. (The injector, and the chart value it
    // read, are both gone since v1.45.0 — this comment named them until
    // v1.50.0, which sent readers after settings that do not exist.)
    // It is also the right answer on purpose — the CR is writable by
    // tenants and the container it configures is privileged, so the
    // limits are the cluster operator's to set, not the mount author's.
    // Found by `the_crd_and_the_struct_agree_on_every_field`.
}

/// A Kubernetes resource quantity (`1Gi`, `512Mi`, `2G`, `1073741824`)
/// in whole MiB, rounded down. `None` for anything else.
pub fn quantity_mib(q: &str) -> Option<u64> {
    let q = q.trim();
    let split = q.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(q.len());
    let (num, suffix) = q.split_at(split);
    let n: f64 = num.parse().ok()?;
    let bytes = match suffix {
        "" => n,
        "Ki" => n * 1024.0,
        "Mi" => n * 1024.0 * 1024.0,
        "Gi" => n * 1024f64.powi(3),
        "Ti" => n * 1024f64.powi(4),
        "k" | "K" => n * 1e3,
        "M" => n * 1e6,
        "G" => n * 1e9,
        "T" => n * 1e12,
        _ => return None,
    };
    if !bytes.is_finite() || bytes < 0.0 {
        return None;
    }
    Some((bytes / (1024.0 * 1024.0)) as u64)
}

/// The smallest default cache worth having; below it the CR must say.
pub const DEFAULT_SHARED_CACHE_MIN_MIB: u64 = 64;

/// The block cache a CR that shares its mounter gets when it names none:
/// three quarters of the worker's scratch emptyDir (`workers.scratchSize`,
/// so 768 MiB at the chart's 1Gi default), leaving headroom under the
/// limit whose overrun evicts the worker — and, under sharing, strands
/// every member. Never below [`DEFAULT_SHARED_CACHE_MIN_MIB`], and none
/// at all when the size cannot be read.
pub fn default_shared_cache_mib(scratch_size: &str) -> Option<u64> {
    let ceiling = quantity_mib(scratch_size)? * 3 / 4;
    (ceiling >= DEFAULT_SHARED_CACHE_MIN_MIB).then_some(ceiling)
}

impl MountSpec {
    /// True when requests should use path-style addressing.
    pub fn use_path_style(&self) -> bool {
        self.path_style.unwrap_or_else(|| self.endpoint.is_some())
    }

    /// The spec as the mounter sees it: a CR that shares its mounter and
    /// names no `cache` gets the default one ([`default_shared_cache_mib`]),
    /// because sharing is for a dataset read many times and without the
    /// cache every read that is not concurrent goes to S3. A CR that names
    /// `cache` keeps it, `enabled: false` included — that is how a shared
    /// CR opts out.
    pub fn with_default_cache(mut self, scratch_size: &str) -> Self {
        if self.shares_read_only() && self.cache.is_none() {
            if let Some(mib) = default_shared_cache_mib(scratch_size) {
                self.cache = Some(CacheSpec { enabled: true, max_size_mib: Some(mib) });
            }
        }
        self
    }

    /// `spec.sharing.readOnly`: the CR asks that its read-only consumers
    /// on a node share one mounter.
    pub fn shares_read_only(&self) -> bool {
        self.sharing.as_ref().is_some_and(|s| s.read_only)
    }

    /// Everything the injector refuses. Each arm names the field and
    /// what to do about it: this message is the only thing the person
    /// who wrote the pod will see.
    pub fn validate(&self) -> Result<(), String> {
        if self.bucket.is_empty() {
            return Err("spec.bucket is empty".into());
        }
        if !self
            .bucket
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        {
            return Err(format!(
                "spec.bucket {:?} has characters outside [A-Za-z0-9._-]",
                self.bucket
            ));
        }

        // The mount path reaches a shell (quoted, via an env var) and
        // the kernel. Keep it to a shape that cannot be anything but a
        // path, so neither reader has to be clever.
        if !self.mount_path.starts_with('/') || self.mount_path == "/" {
            return Err(format!(
                "spec.mountPath {:?} must be an absolute path below /",
                self.mount_path
            ));
        }
        if self.mount_path.ends_with('/') {
            return Err(format!(
                "spec.mountPath {:?} must not end in / — the /proc/mounts probe matches the \
                 path exactly",
                self.mount_path
            ));
        }
        if !self
            .mount_path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
        {
            return Err(format!(
                "spec.mountPath {:?} has characters outside [A-Za-z0-9./_-]",
                self.mount_path
            ));
        }

        // mount-s3 has no `-o` flag — every option it takes is a
        // `--long` one — so `-o` here is an s3fs-shaped option that
        // outlived s3fs. Caught because of what it costs otherwise:
        // mount-s3 exits on the unknown argument, and the pod is a
        // PRIVILEGED sidecar in CrashLoopBackOff whose reason exists
        // only in a container log. Measured on kind 2026-08-27, from a
        // CR written before the driver field was removed.
        if let Some(i) = self.mount_options.iter().position(|o| o == "-o") {
            let val = self.mount_options.get(i + 1).map(String::as_str).unwrap_or("");
            return Err(format!(
                "spec.mountOptions contains \"-o\" (\"{val}\") — that is an s3fs option and \
                 this mounts with Mountpoint for S3, which takes only --long flags and would \
                 exit on it. Drop it, or write the mount-s3 equivalent"
            ));
        }
        if let Some(p) = &self.key_prefix {
            if p.starts_with('/') {
                return Err(format!(
                    "spec.keyPrefix {p:?} must not start with / — it is an object key \
                     prefix, not a path"
                ));
            }
            if p.split('/').any(|seg| seg == "..") {
                return Err(format!("spec.keyPrefix {p:?} must not contain a .. segment"));
            }
        }
        // An enabled cache with no ceiling is unbounded, and the worker's
        // scratch emptyDir has a `sizeLimit`: exceeding it EVICTS the
        // worker, and an evicted passthrough worker cannot be restarted
        // (`restartPolicy: Never`), so the tenant's mount is gone for
        // good. The CRD requires only `enabled`, so this is the check
        // that closes it.
        if let Some(c) = self.cache.as_ref().filter(|c| c.enabled) {
            match c.max_size_mib {
                None => {
                    return Err(
                        "spec.cache.enabled is true but spec.cache.maxSizeMib is unset — an \
                         unbounded cache fills the worker's scratch emptyDir, whose sizeLimit \
                         then evicts the worker, and an evicted passthrough worker cannot be \
                         restarted. Set maxSizeMib with headroom under workers.scratchSize"
                            .into(),
                    )
                }
                Some(0) => {
                    return Err("spec.cache.maxSizeMib is 0 — set a real ceiling in MiB, or set \
                                spec.cache.enabled to false"
                        .into())
                }
                Some(_) => {}
            }
        }
        // A shared mounter holds ONE credential for every member. With
        // `static` that credential is whichever pod's nodePublishSecretRef
        // came first — the pod author's choice, not the CR's — so the
        // members' authority would not be the same function of the CR,
        // which is the whole argument for sharing. Refused by name.
        if self.shares_read_only() && self.identity.as_ref().is_some_and(|i| i.mode == "static") {
            return Err("spec.sharing.readOnly is true with spec.identity.mode static — a shared mounter \
                        holds one credential for every member, and a static key is the pod's own \
                        nodePublishSecretRef. Use identity.mode broker or ambient, or drop sharing"
                .into());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Fields the CRD declares ON PURPOSE without a struct field
    /// behind them. Each one exists to make the API server REFUSE a
    /// value rather than prune it silently — see the tombstone note in
    /// crds/flintpassthroughmounts.yaml.
    const TOMBSTONES: &[&str] = &["driver"];

    /// An enabled cache with no ceiling is unbounded: it fills the
    /// worker's scratch emptyDir, whose sizeLimit then EVICTS the
    /// worker — and an evicted passthrough worker cannot be restarted
    /// (`restartPolicy: Never`), so the tenant's mount is gone for good.
    /// The CRD requires only `enabled`, so this is the check that closes
    /// it.
    #[test]
    fn an_enabled_cache_without_a_ceiling_is_refused() {
        let mut s = MountSpec {
            bucket: "b".into(),
            key_prefix: None,
            endpoint: None,
            region: None,
            mount_path: "/mnt/s3".into(),
            read_only: false,
            path_style: None,
            credentials_secret_ref: None,
            uid: None,
            gid: None,
            mount_options: vec![],
            cache: None,
            sharing: None,
            image: None,
            consumers: None,
            identity: None,
        };
        s.cache = Some(CacheSpec { enabled: true, max_size_mib: None });
        let e = s.validate().expect_err("an unbounded cache must be refused");
        assert!(e.contains("maxSizeMib"), "the message must name the field to set: {e}");
        s.cache = Some(CacheSpec { enabled: true, max_size_mib: Some(0) });
        assert!(s.validate().is_err(), "a zero ceiling is not a ceiling");
        s.cache = Some(CacheSpec { enabled: true, max_size_mib: Some(512) });
        assert!(s.validate().is_ok(), "a bounded cache is fine");
        s.cache = Some(CacheSpec { enabled: false, max_size_mib: None });
        assert!(s.validate().is_ok(), "disabled needs no ceiling");
    }

    #[test]
    fn quantities_read_in_whole_mib() {
        for (q, want) in [
            ("1Gi", Some(1024)),
            ("512Mi", Some(512)),
            ("1.5Gi", Some(1536)),
            ("2G", Some(1907)),
            ("1073741824", Some(1024)),
            ("64Ki", Some(0)),
            (" 1Gi ", Some(1024)),
            ("", None),
            ("abc", None),
            ("10x", None),
            ("100m", None),
            ("-1Gi", None),
        ] {
            assert_eq!(quantity_mib(q), want, "{q:?}");
        }
    }

    /// Sharing is for a dataset read many times, and without the block
    /// cache every read that is not concurrent goes to S3: a sharing CR
    /// that names no cache gets three quarters of the worker's scratch —
    /// under the limit whose overrun would evict the shared worker and
    /// strand every member. A named cache, enabled or not, is kept, and a
    /// CR that does not share gets nothing it did not ask for.
    #[test]
    fn a_sharing_cr_without_a_cache_gets_three_quarters_of_the_scratch() {
        let base = MountSpec {
            bucket: "b".into(),
            key_prefix: None,
            endpoint: None,
            region: None,
            mount_path: "/mnt/s3".into(),
            read_only: true,
            path_style: None,
            credentials_secret_ref: None,
            uid: None,
            gid: None,
            mount_options: vec![],
            cache: None,
            sharing: Some(SharingSpec { read_only: true }),
            image: None,
            consumers: None,
            identity: None,
        };
        let d = base.clone().with_default_cache("1Gi");
        assert_eq!(d.cache, Some(CacheSpec { enabled: true, max_size_mib: Some(768) }));
        assert!(d.validate().is_ok(), "the default must pass the cache validation");
        assert_eq!(base.clone().with_default_cache("512Mi").cache.unwrap().max_size_mib, Some(384));
        assert!(base.clone().with_default_cache("64Mi").cache.is_none(), "48 MiB is below the floor: the CR must say");
        assert!(base.clone().with_default_cache("").cache.is_none(), "an unreadable size defaults nothing");
        let off = MountSpec { cache: Some(CacheSpec { enabled: false, max_size_mib: None }), ..base.clone() };
        assert_eq!(off.clone().with_default_cache("1Gi").cache, off.cache, "enabled: false is the opt-out");
        let named = MountSpec { cache: Some(CacheSpec { enabled: true, max_size_mib: Some(100) }), ..base.clone() };
        assert_eq!(named.clone().with_default_cache("1Gi").cache, named.cache, "a named ceiling is kept");
        let own = MountSpec { sharing: None, ..base.clone() };
        assert!(own.with_default_cache("1Gi").cache.is_none(), "a CR that does not share gets no default");
        assert_eq!(default_shared_cache_mib("1Gi"), Some(768));
    }

    /// A shared mounter holds one credential for all its members, so the
    /// credential must be the CR's function, not a pod's: `static` (the
    /// pod's own Secret) is refused by name; broker and ambient are not.
    #[test]
    fn sharing_cannot_run_on_a_per_pod_secret() {
        let mut s = MountSpec {
            bucket: "b".into(),
            key_prefix: None,
            endpoint: None,
            region: None,
            mount_path: "/mnt/s3".into(),
            read_only: true,
            path_style: None,
            credentials_secret_ref: None,
            uid: None,
            gid: None,
            mount_options: vec![],
            cache: None,
            sharing: Some(SharingSpec { read_only: true }),
            image: None,
            consumers: None,
            identity: Some(crate::s3csi::policy::Identity { mode: "static".into() }),
        };
        assert!(!MountSpec { sharing: None, ..s.clone() }.shares_read_only());
        assert!(!MountSpec { sharing: Some(SharingSpec { read_only: false }), ..s.clone() }.shares_read_only());
        assert!(s.shares_read_only());
        let e = s.validate().expect_err("static + sharing must be refused");
        assert!(e.contains("static") && e.contains("sharing"), "{e}");
        for mode in ["broker", "ambient"] {
            s.identity = Some(crate::s3csi::policy::Identity { mode: mode.into() });
            assert!(s.validate().is_ok(), "{mode} may share");
        }
        s.identity = None;
        assert!(s.validate().is_ok(), "the default mode is broker");
    }

    /// The CRD is hand-written (this spec is plain serde, not a
    /// schemars derive), so nothing but this test stops the two from
    /// drifting — and drift is silent in both directions:
    ///
    /// - a struct field the CRD does not declare is PRUNED by the API
    ///   server before the node plugin ever sees it: a knob that exists in
    ///   the CR the user wrote and does nothing;
    /// - a CRD property the struct does not have is stored and then
    ///   hits `deny_unknown_fields`, denying every pod that opts into
    ///   the mount.
    #[test]
    fn the_crd_and_the_struct_agree_on_every_field() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../flint-passthrough-chart/crds/flintpassthroughmounts.yaml"
        );
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read the shipped CRD at {path}: {e}"));
        let doc: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
        let props = doc["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]
            .as_mapping()
            .expect("the CRD must declare spec.properties");
        let in_crd: BTreeSet<String> = props
            .keys()
            .map(|k| k.as_str().expect("property names are strings").to_string())
            .filter(|k| !TOMBSTONES.contains(&k.as_str()))
            .collect();

        // Every field populated, so serde emits every key. This is the
        // half that cannot rot: adding a field to the struct changes
        // this set without anyone remembering to update a list.
        let all = MountSpec {
            bucket: "b".into(),
            key_prefix: Some("p".into()),
            endpoint: Some("http://e:9000".into()),
            region: Some("us-east-1".into()),
            mount_path: "/mnt/s3".into(),
            read_only: true,
            path_style: Some(true),
            credentials_secret_ref: Some("s".into()),
            uid: Some(1),
            gid: Some(1),
            mount_options: vec!["--metadata-ttl".into(), "60".into()],
            cache: Some(CacheSpec { enabled: true, max_size_mib: Some(512) }),
            sharing: Some(SharingSpec { read_only: true }),
            image: Some("i".into()),
            consumers: Some(crate::s3csi::policy::MountConsumers {
                service_accounts: vec!["a".into()],
                read_only_service_accounts: vec!["r".into()],
            }),
            identity: Some(crate::s3csi::policy::Identity { mode: "broker".into() }),
        };
        let value = serde_json::to_value(&all).unwrap();
        let in_struct: BTreeSet<String> =
            value.as_object().unwrap().keys().cloned().collect();

        let pruned: Vec<&String> = in_struct.difference(&in_crd).collect();
        assert!(
            pruned.is_empty(),
            "these spec fields are NOT in the CRD, so the API server prunes them and the \
             knob does nothing: {pruned:?}"
        );
        let denied: Vec<&String> = in_crd.difference(&in_struct).collect();
        assert!(
            denied.is_empty(),
            "the CRD declares these and the struct rejects them (deny_unknown_fields), so a \
             CR using one denies every pod: {denied:?} — add the field, or list it in \
             TOMBSTONES if refusing it is the point"
        );

        // One level down, for the policy block: a consumers list the CRD
        // does not declare is pruned, and a pruned read-only list denies
        // the ServiceAccounts it names instead of narrowing them.
        let in_crd_consumers: BTreeSet<String> = props["consumers"]["properties"]
            .as_mapping()
            .expect("the CRD must declare spec.consumers.properties")
            .keys()
            .map(|k| k.as_str().unwrap().to_string())
            .collect();
        let in_struct_consumers: BTreeSet<String> =
            value["consumers"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(in_crd_consumers, in_struct_consumers, "spec.consumers: the CRD and the struct disagree");
    }

    /// A tombstone that is not actually refused is worse than no
    /// tombstone: it reads as a deliberate refusal in the CRD while
    /// the API server quietly accepts and prunes the value.
    #[test]
    fn every_tombstone_actually_refuses_its_value() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../flint-passthrough-chart/crds/flintpassthroughmounts.yaml"
        );
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let props = &doc["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]
            ["spec"]["properties"];
        for t in TOMBSTONES {
            let rules = props[*t]["x-kubernetes-validations"]
                .as_sequence()
                .unwrap_or_else(|| panic!("tombstone {t:?} carries no x-kubernetes-validations"));
            let refuses = rules.iter().any(|r| r["rule"].as_str() == Some("false"));
            assert!(refuses, "tombstone {t:?} has rules that do not refuse: {rules:?}");
            let msg = rules[0]["message"].as_str().unwrap_or("");
            assert!(
                msg.contains(t),
                "tombstone {t:?}'s message must name the field: {msg:?}"
            );
        }
    }
}

pub const CRD_GROUP: &str = "chert.us";
pub const CRD_VERSION: &str = "v1alpha1";
pub const CRD_KIND: &str = "FlintPassthroughMount";

/// Fetch a `FlintPassthroughMount` as raw JSON (the spec is plain
/// serde over a hand-written CRD, so there is no typed `Api` for it).
/// `None` when the CR does not exist in that namespace.
pub async fn get_mount(client: &kube::Client, ns: &str, name: &str) -> Result<Option<serde_json::Value>, String> {
    use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind};
    let gvk = GroupVersionKind::gvk(CRD_GROUP, CRD_VERSION, CRD_KIND);
    let ar = ApiResource::from_gvk(&gvk);
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, &ar);
    match api.get_opt(name).await {
        Ok(Some(o)) => serde_json::to_value(o).map(Some).map_err(|e| e.to_string()),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
