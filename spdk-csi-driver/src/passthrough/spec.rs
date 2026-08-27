//! The `FlintPassthroughMount` spec — pure configuration, read at
//! admission time and never reconciled.
//!
//! There is no controller behind this CR and no status subresource,
//! because there is nothing to converge: a passthrough mount owns no
//! bucket state, takes no claim, keeps no manifest and holds no lease.
//! The whole product is "an S3 prefix appears as a directory in this
//! pod", so the CR is the argument list for a `mount-s3` command and
//! the webhook is the only reader. Anything that needs a control loop
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
//! The type is plain serde over the CR's `spec` object (the webhook
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
    /// concatenated into a shell string. See `inject::sidecar_command`.
    #[serde(default)]
    pub mount_options: Vec<String>,
    /// Per-mount image override (the chart's default otherwise).
    #[serde(default)]
    pub image: Option<String>,
    // NO per-mount `resources`. There was a field here and it was dead:
    // the injector takes the sidecar's resources from the CHART
    // (`sidecarResources` → `InjectDefaults`) and never looked at the
    // CR's, so a spec that set it would have been accepted and ignored.
    // It is also the right answer on purpose — the CR is writable by
    // tenants and the container it configures is privileged, so the
    // limits are the cluster operator's to set, not the mount author's.
    // Found by `the_crd_and_the_struct_agree_on_every_field`.
}

impl MountSpec {
    /// True when requests should use path-style addressing.
    pub fn use_path_style(&self) -> bool {
        self.path_style.unwrap_or_else(|| self.endpoint.is_some())
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

    /// The CRD is hand-written (this spec is plain serde, not a
    /// schemars derive), so nothing but this test stops the two from
    /// drifting — and drift is silent in both directions:
    ///
    /// - a struct field the CRD does not declare is PRUNED by the API
    ///   server before the webhook ever sees it: a knob that exists in
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
            image: Some("i".into()),
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
