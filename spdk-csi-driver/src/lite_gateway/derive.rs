//! The credential the gateway presents to a hub.
//!
//! Every hub checks ONE opaque bearer token and does no parsing of it
//! (`pnfs/mds/fileapi/mod.rs`), so the gateway is free to choose how the
//! value is produced. It has two ways, and the choice is about blast
//! radius rather than about what the hub accepts.
//!
//! ## Why not just read the Secret
//!
//! Because of where those Secrets live. A share's token Secret sits in
//! the tenant's namespace beside `credentialsSecretRef` — the tenant's
//! S3 credentials. Granting the gateway `get secrets` across the
//! workspace namespaces to read 3000 tokens hands it every tenant's
//! bucket credentials as a side effect, and it is exactly the grant
//! `docs/plans/file-api-fleet-auth.md` §5 refuses. The gateway
//! therefore has NO secrets RBAC at all, in any mode.
//!
//! ## Derived (the default)
//!
//! ```text
//! token(share) = base64url_nopad(
//!     HMAC-SHA256(root, "flint-fileapi/v1:" || endpoint || ":" || bucket
//!                       || ":" || keyPrefix || ":" || version))
//! ```
//!
//! One key produces every hub's token on demand, so there is nothing to
//! store and nothing to fan out. `keyPrefix` rather than the CR name
//! because the prefix is immutable while names are reusable, and
//! `bucket`/`endpoint` because a prefix is only unique inside a bucket.
//! `version` is an annotation on the CR that nothing else interprets;
//! bumping it revokes ONE project.
//!
//! **Whoever provisions a share must derive identically.** That is a
//! contract between two components, so the gateway binary can print a
//! token (`--derive-token`) and be the oracle rather than the docs
//! being the oracle. [`derive`] is the single implementation both use.
//!
//! ## Shared
//!
//! One token for the whole fleet. Honest about what it gives up and
//! what it does not: per-hub secrets protect a project from OTHER
//! callers of the API, and once the gateway is the only caller there
//! are none — a compromise of the gateway opens every project by
//! construction in both modes. What `shared` really loses is
//! single-project revocation, which is the one thing `version` buys.
//!
//! ## What is never done here
//!
//! The gateway does not WRITE the Secret a hub reads. Minting on
//! provision and minting on call are the same function of the same key;
//! splitting them across two components would put the root in two
//! places, and giving the gateway `create` on Secrets in every tenant
//! namespace to avoid that is a worse trade than a documented
//! derivation both sides run.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Domain separation. Keeps this key's outputs from colliding with any
/// other use of the same root, and gives the scheme a version to change
/// under without changing the annotation's meaning.
const DOMAIN: &str = "flint-fileapi/v1:";

/// The annotation carrying a share's token version.
///
/// Read by the gateway and by whatever provisions shares. NOT read by
/// the operator — it renders nothing from it and must not, or a
/// revocation would roll the hub and cost every mounted client an NFS
/// stall to change an HTTP credential.
pub const ANN_TOKEN_VERSION: &str = "flint.io/api-token-version";

/// The identity a token is bound to.
///
/// Borrowed rather than owned so the caller can build one from a CR
/// without cloning; the derivation copies nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding<'a> {
    /// `spec.endpoint` — empty for real S3.
    pub endpoint: &'a str,
    /// `spec.bucket`.
    pub bucket: &'a str,
    /// `spec.keyPrefix`.
    pub key_prefix: &'a str,
    /// The `flint.io/api-token-version` annotation, 1 when absent.
    pub version: u64,
}

impl<'a> Binding<'a> {
    /// The exact bytes the MAC is taken over.
    ///
    /// Public because it is the thing a KMS-backed implementation
    /// (`GenerateMac`, Vault Transit) signs instead — moving the root
    /// out of this process changes who computes the MAC, never what it
    /// is computed over.
    pub fn message(&self) -> String {
        format!(
            "{DOMAIN}{}:{}:{}:{}",
            self.endpoint, self.bucket, self.key_prefix, self.version
        )
    }

    /// The same binding one version back, for the rotation retry.
    ///
    /// `None` at version 1: there is no previous version, and retrying
    /// at version 0 would present a token no hub has ever held.
    pub fn previous(&self) -> Option<Binding<'a>> {
        (self.version > 1).then(|| Binding { version: self.version - 1, ..*self })
    }
}

/// A share with no bucket has no prefix to bind to.
///
/// Such a share's PVC is the only copy of its data, and there is no
/// immutable identity to key on — the CR name is reusable, so a token
/// derived from it would follow a recreated project. Rather than invent
/// a weaker binding silently, this is an error the caller reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoBinding;

/// HMAC the binding under `root`, base64url-nopad.
///
/// 32 bytes in, 43 characters out. Nothing in the hub cares about the
/// shape; the encoding exists so the value survives a header, a `kubectl
/// create secret --from-literal` and a YAML round-trip unquoted.
pub fn derive(root: &[u8], b: &Binding<'_>) -> String {
    // `new_from_slice` is infallible for HMAC: any key length is legal,
    // short keys are zero-padded and long ones are hashed first.
    let mut mac = HmacSha256::new_from_slice(root).expect("hmac accepts any key length");
    mac.update(b.message().as_bytes());
    base64url_nopad(&mac.finalize().into_bytes())
}

/// URL-safe base64 with no padding, written out rather than pulled in.
///
/// A dependency for twenty lines that has to agree byte-for-byte with
/// whatever the provisioner uses is not worth adding; what matters is
/// that it is the RFC 4648 §5 alphabet, which every language's
/// `base64url` already is.
fn base64url_nopad(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(A[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(A[n as usize & 63] as char);
        }
    }
    out
}

/// How the gateway produces the credential for a hub.
#[derive(Debug, Clone)]
pub enum Minter {
    /// One key, one token per share. See the module doc.
    Derived(Vec<u8>),
    /// One token for every hub.
    Shared(String),
}

impl Minter {
    /// The token to present to this share's hub.
    ///
    /// `Err(NoBinding)` only in `Derived` mode and only for a share with
    /// no bucket — reported to the caller as a 503 naming the share,
    /// never as a request sent without a credential.
    pub fn token_for(&self, b: Result<Binding<'_>, NoBinding>) -> Result<String, NoBinding> {
        match self {
            // A shared token needs no binding, so a bucketless share is
            // perfectly serveable here. Resolving the binding is still
            // attempted by the caller so the two modes take the same
            // path and the `Derived` failure is not a special case that
            // only fires in production.
            Minter::Shared(t) => Ok(t.clone()),
            Minter::Derived(root) => Ok(derive(root, &b?)),
        }
    }

    /// The retry credential for a hub that answered 401 mid-rotation.
    ///
    /// `None` when there is nothing to fall back to: a shared token has
    /// no versions, and version 1 has no predecessor.
    pub fn previous_token_for(&self, b: Result<Binding<'_>, NoBinding>) -> Option<String> {
        match self {
            Minter::Shared(_) => None,
            Minter::Derived(root) => Some(derive(root, &b.ok()?.previous()?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(version: u64) -> Binding<'static> {
        Binding { endpoint: "", bucket: "tenant-bucket", key_prefix: "proj-a/", version }
    }

    #[test]
    fn the_output_is_43_url_safe_characters() {
        let t = derive(b"root", &b(1));
        assert_eq!(t.len(), 43, "32 bytes base64url-nopad is 43 chars: {t}");
        assert!(
            t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "must survive a header and an unquoted YAML scalar: {t}"
        );
        assert!(!t.contains('='), "nopad");
    }

    /// THE ANTI-VACUITY GUARD FOR THIS WHOLE MODULE.
    ///
    /// Every property below ("a different X gives a different token")
    /// would pass trivially against a function that ignored its inputs
    /// and returned a random string. So this pins the ONE thing such a
    /// function cannot do: produce the same answer twice. Without it
    /// the rest of the file tests nothing.
    #[test]
    fn the_same_binding_always_gives_the_same_token() {
        assert_eq!(derive(b"root", &b(1)), derive(b"root", &b(1)));
        // And it is a KNOWN answer, not merely a stable one. A change to
        // the message format, the alphabet or the hash is a fleet-wide
        // credential change: every hub would start rejecting the
        // gateway at once, with no rollback short of a redeploy. Pinning
        // the literal makes that break here instead.
        assert_eq!(derive(b"root", &b(1)), "rUAfpRygCejxS2kQJIq9pqrhEqsR_Q_jICr7q6BaH1w");
    }

    #[test]
    fn every_component_of_the_binding_changes_the_token() {
        let base = derive(b"root", &b(1));
        let vary = [
            Binding { endpoint: "https://minio.local", ..b(1) },
            Binding { bucket: "other-bucket", ..b(1) },
            Binding { key_prefix: "proj-b/", ..b(1) },
            b(2),
        ];
        for v in vary {
            assert_ne!(base, derive(b"root", &v), "{v:?} must not collide with {:?}", b(1));
        }
        assert_ne!(base, derive(b"other-root", &b(1)), "the root must matter");
    }

    /// A prefix is only unique WITHIN a bucket, which is why the bucket
    /// is in the message. Without a separator, ("ab", "c/") and
    /// ("a", "bc/") would be the same byte string and two unrelated
    /// tenants would share a credential.
    #[test]
    fn concatenation_is_unambiguous_across_field_boundaries() {
        let left = Binding { endpoint: "", bucket: "ab", key_prefix: "c/", version: 1 };
        let right = Binding { endpoint: "", bucket: "a", key_prefix: "bc/", version: 1 };
        assert_ne!(derive(b"root", &left), derive(b"root", &right));
    }

    #[test]
    fn version_1_has_no_previous_and_never_retries_at_zero() {
        assert_eq!(b(1).previous(), None);
        assert_eq!(b(3).previous().map(|p| p.version), Some(2));
        let m = Minter::Derived(b"root".to_vec());
        assert_eq!(m.previous_token_for(Ok(b(1))), None);
        assert_eq!(m.previous_token_for(Ok(b(2))), Some(derive(b"root", &b(1))));
    }

    #[test]
    fn a_shared_token_is_the_same_for_every_share_and_never_retries() {
        let m = Minter::Shared("one-token".to_string());
        assert_eq!(m.token_for(Ok(b(1))).unwrap(), "one-token");
        assert_eq!(m.token_for(Err(NoBinding)).unwrap(), "one-token");
        assert_eq!(m.previous_token_for(Ok(b(9))), None, "there are no versions to fall back to");
    }

    #[test]
    fn a_bucketless_share_is_refused_in_derived_mode_rather_than_bound_to_something_weaker() {
        // The alternative — falling back to namespace/name — silently
        // binds the credential to a REUSABLE identity, so a deleted and
        // recreated project inherits the old token. Refusing is louder
        // and the caller turns it into a 503 naming the share.
        let m = Minter::Derived(b"root".to_vec());
        assert_eq!(m.token_for(Err(NoBinding)), Err(NoBinding));
    }

    #[test]
    fn base64url_matches_rfc4648_on_the_lengths_that_exercise_padding() {
        // The three chunk remainders, and the two characters that are
        // the whole reason for the URL-safe alphabet.
        assert_eq!(base64url_nopad(b""), "");
        assert_eq!(base64url_nopad(b"f"), "Zg");
        assert_eq!(base64url_nopad(b"fo"), "Zm8");
        assert_eq!(base64url_nopad(b"foo"), "Zm9v");
        assert_eq!(base64url_nopad(b"foob"), "Zm9vYg");
        // The two characters the URL-safe alphabet exists for: index
        // 62 is `-` (not `+`) and index 63 is `_` (not `/`), so a token
        // is safe in a header, a path segment and a query string.
        assert_eq!(base64url_nopad(&[0xfb, 0xef]), "--8");
        assert_eq!(base64url_nopad(&[0xff, 0xef]), "_-8");
    }
}
