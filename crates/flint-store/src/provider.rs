//! Which object store a bucket lives in, and the object names it takes.
//!
//! Lean refuses to publish a path whose key the store would refuse
//! (L-145): the barrier HOLDS the path, as it holds a name that is not
//! UTF-8, and the gateway refuses to save or move a file there. On S3
//! the only rule is the length. Azure and GCS add rules of their own,
//! and a rule the barrier does not know is a PUT that fails at every
//! barrier (docs/plans/lean-azure-gcs-backends-plan-2026-11-10.md, C8).

/// The longest key every provider takes: 1,024 bytes of UTF-8 on S3
/// (`KeyTooLongError`) and on GCS, and 1,024 characters on Azure, which
/// a 1,024-byte key never exceeds.
pub const MAX_KEY_BYTES: usize = 1024;

/// Most path segments an Azure blob name may have on an account without
/// a hierarchical namespace.
pub const AZURE_MAX_SEGMENTS: usize = 254;

/// The object store behind a bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Provider {
    /// Amazon S3, and every store that speaks its API (MinIO, Ozone's
    /// S3 gateway, Ceph RGW) through an endpoint override.
    #[default]
    S3,
    /// Azure Blob Storage.
    Azure,
    /// Google Cloud Storage.
    Gcs,
}

impl Provider {
    /// `s3`, `azure` or `gcs`, in any case. Empty is S3, so a deployment
    /// that never set the variable keeps the store it always had.
    pub fn parse(s: &str) -> Result<Provider, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "s3" => Ok(Provider::S3),
            "azure" => Ok(Provider::Azure),
            "gcs" => Ok(Provider::Gcs),
            other => Err(format!("unknown object store provider {other:?}: want s3, azure or gcs")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Provider::S3 => "s3",
            Provider::Azure => "azure",
            Provider::Gcs => "gcs",
        }
    }

    /// Why this store would refuse `key` as an object name, or `None`
    /// when it takes it.
    ///
    /// Azure's rules here are its naming guidance taken as rules: it
    /// tells callers to avoid a name ending in `/` or `\` and a path
    /// segment ending in `.`. A name the service rewrites (a trailing dot
    /// trimmed, a backslash read as `/`) would alias another path's
    /// object, which is worse than holding the file. They are refused
    /// until the contract suite shows a real account keeps such names
    /// byte for byte.
    pub fn key_refusal(self, key: &str) -> Option<&'static str> {
        if key.is_empty() {
            return Some("an empty key");
        }
        if key.len() > MAX_KEY_BYTES {
            return Some("longer than 1,024 bytes");
        }
        match self {
            Provider::S3 => None,
            Provider::Azure => {
                if key.split('/').count() > AZURE_MAX_SEGMENTS {
                    Some("more than 254 path segments")
                } else if key.contains('\\') {
                    Some("a backslash")
                } else if key.ends_with('/') {
                    Some("a trailing '/'")
                } else if key.split('/').any(|seg| seg.ends_with('.')) {
                    Some("a path segment ending in '.'")
                } else {
                    None
                }
            }
            Provider::Gcs => {
                if key.contains('\r') || key.contains('\n') {
                    Some("a carriage return or line feed")
                } else if key == "." || key == ".." {
                    Some("the name '.' or '..'")
                } else if key.starts_with(".well-known/acme-challenge/") {
                    Some("the reserved prefix .well-known/acme-challenge/")
                } else {
                    None
                }
            }
        }
    }

    /// Whether this store takes `key` as an object name.
    pub fn key_ok(self, key: &str) -> bool {
        self.key_refusal(key).is_none()
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_takes_the_three_names_in_any_case_and_empty_as_s3() {
        assert_eq!(Provider::parse("s3"), Ok(Provider::S3));
        assert_eq!(Provider::parse(" Azure "), Ok(Provider::Azure));
        assert_eq!(Provider::parse("GCS"), Ok(Provider::Gcs));
        assert_eq!(Provider::parse(""), Ok(Provider::S3));
        assert!(Provider::parse("gcp").is_err(), "a near miss is refused, not guessed");
        for p in [Provider::S3, Provider::Azure, Provider::Gcs] {
            assert_eq!(Provider::parse(p.as_str()), Ok(p));
        }
    }

    #[test]
    fn every_provider_refuses_an_empty_key_and_one_past_1024_bytes() {
        let at = "k".repeat(MAX_KEY_BYTES);
        let past = "k".repeat(MAX_KEY_BYTES + 1);
        for p in [Provider::S3, Provider::Azure, Provider::Gcs] {
            assert!(p.key_ok(&at), "{p}: exactly 1,024 bytes fits");
            assert!(!p.key_ok(&past), "{p}");
            assert!(!p.key_ok(""), "{p}");
        }
        // Bytes, not characters: 342 three-byte characters are 1,026 bytes.
        assert!(!Provider::Azure.key_ok(&"€".repeat(342)));
    }

    #[test]
    fn s3_takes_what_azure_and_gcs_refuse() {
        let names = ["p/files/a./b@u", "p/files/a\\b@u", "p/files/dir/", "p/files/a\nb@u", "p/files/a\rb@u"];
        for k in names {
            assert!(Provider::S3.key_ok(k), "S3 takes {k:?}: the control");
        }
    }

    #[test]
    fn azure_refuses_dot_ended_segments_backslashes_trailing_slashes_and_255_segments() {
        assert!(Provider::Azure.key_ok("p/files/a.b/c@u"), "a dot inside a segment is fine");
        assert!(!Provider::Azure.key_ok("p/files/a./b@u"));
        assert!(!Provider::Azure.key_ok("p/files/b."));
        assert!(!Provider::Azure.key_ok("p/files/../b@u"));
        assert!(!Provider::Azure.key_ok("p/files/a\\b@u"));
        assert!(!Provider::Azure.key_ok("p/files/dir/"));
        let segs = |n: usize| vec!["s"; n].join("/");
        assert!(Provider::Azure.key_ok(&segs(254)));
        assert!(!Provider::Azure.key_ok(&segs(255)));
        assert!(Provider::Azure.key_ok("p/files/a\nb@u"), "a line feed is GCS's rule, not Azure's");
    }

    #[test]
    fn gcs_refuses_cr_lf_dot_names_and_the_acme_prefix() {
        assert!(Provider::Gcs.key_ok("p/files/a./b@u"), "a dot-ended segment is Azure's rule, not GCS's");
        assert!(!Provider::Gcs.key_ok("p/files/a\nb@u"));
        assert!(!Provider::Gcs.key_ok("p/files/a\rb@u"));
        assert!(!Provider::Gcs.key_ok("."));
        assert!(!Provider::Gcs.key_ok(".."));
        assert!(Provider::Gcs.key_ok("p/../b"), "only the whole name '..' is refused");
        assert!(!Provider::Gcs.key_ok(".well-known/acme-challenge/x"));
    }
}
