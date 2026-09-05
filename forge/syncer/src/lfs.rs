//! Git LFS, so a repository of large binaries stops paying git's
//! object model for bytes that will never delta (design §14 phase 6).
//!
//! This is the multi-modal case. A pack is delta-compressed and
//! rewritten WHOLE by `repack -a`, so images, audio, video and model
//! weights committed as ordinary blobs make every clone, every repack
//! and every restore pay for them again. LFS keeps a small pointer file
//! in git and puts the bytes at `<prefix>/lfs/objects/<oid>` —
//! immutable, content-named, which is the layout §3 already uses for
//! packs.
//!
//! **The bytes never cross the server.** The batch response hands the
//! client a presigned URL, so an agent uploading a 4 GB checkpoint
//! talks to the object store directly and the repository pod sees a
//! few hundred bytes of JSON. That is the same lever bundle URIs give
//! for the pack (§8), applied to the bytes that dominate a multi-modal
//! repository — and it is why the batch API lives HERE, in the process
//! that already holds the bucket credentials, rather than in the door,
//! which deliberately holds none.
//!
//! ## What is deliberately not built
//!
//! **Nothing sweeps LFS objects.** An object is referenced by a
//! pointer file inside some tree of some commit, so deciding one is
//! unreferenced means walking every reachable tree looking for pointer
//! files — expensive, and wrong the first time a ref the walk did not
//! know about still names it. Lean's own `sweep_chunks` is the
//! cautionary tale: safe against one reference set and unsafe the
//! moment a second appeared. An unreferenced LFS object costs storage
//! and nothing else, so the honest answer is to leave it, and to say
//! so rather than ship a reaper that is right most of the time.

use std::collections::BTreeMap;

use flint_store::{ObjectStore, StoreError};

/// The media type the whole protocol speaks. A response that does not
/// carry it is not parsed by git-lfs at all.
pub const LFS_MEDIA_TYPE: &str = "application/vnd.git-lfs+json";

/// How long a transfer URL is good for. Long enough for a multi-gigabyte
/// object on a slow link, short enough that a leaked URL is not a
/// standing grant: it is a bearer token for one object.
pub const DEFAULT_TTL_SECS: u64 = 3600;

/// git-lfs's own cap on a batch. Anything larger is a client that has
/// been configured to ask for more than the protocol expects, and
/// answering it would mean an unbounded number of HEADs per request.
pub const MAX_BATCH: usize = 1000;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRequest {
    pub operation: String,
    #[serde(default)]
    pub transfers: Vec<String>,
    #[serde(default)]
    pub objects: Vec<ObjectSpec>,
    /// `sha256` in every version of the protocol so far. A client that
    /// names another is refused rather than served objects keyed by an
    /// algorithm this server does not use.
    #[serde(default)]
    pub hash_algo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ObjectSpec {
    pub oid: String,
    pub size: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BatchResponse {
    pub transfer: String,
    pub objects: Vec<ObjectResponse>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ObjectResponse {
    pub oid: String,
    pub size: u64,
    /// The client already authenticated to the door, so it must not be
    /// asked to negotiate again for the transfer URL.
    pub authenticated: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub actions: BTreeMap<String, Action>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ObjectError>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Action {
    pub href: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub header: BTreeMap<String, String>,
    pub expires_in: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ObjectError {
    pub code: u16,
    pub message: String,
}

/// A refusal of the whole request, in the shape git-lfs prints.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BatchError {
    pub message: String,
}

/// `<prefix>/lfs/objects/<oid>`.
///
/// Flat, not sharded into `ab/cd/`. That layout exists because a
/// filesystem directory with a million entries is slow to read; an S3
/// prefix is an index and does not care, and a flat key is one fewer
/// place for two implementations to disagree about where an object
/// lives.
pub fn object_key(prefix: &str, oid: &str) -> String {
    format!("{}/lfs/objects/{oid}", prefix.trim_end_matches('/'))
}

/// A SHA-256 in lower-case hex, and nothing else.
///
/// The oid is caller-supplied and becomes an S3 KEY, so this is the
/// boundary that stops `../` or a newline from reaching one. It is
/// also why the layout can be flat and unescaped: the only characters
/// that ever appear are hex.
pub fn valid_oid(oid: &str) -> bool {
    oid.len() == 64 && oid.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn refuse(oid: &str, size: u64, code: u16, message: impl Into<String>) -> ObjectResponse {
    ObjectResponse {
        oid: oid.to_string(),
        size,
        authenticated: true,
        actions: BTreeMap::new(),
        error: Some(ObjectError { code, message: message.into() }),
    }
}

/// Answer one batch request.
///
/// `Err` is a refusal of the WHOLE request; a per-object problem is an
/// `error` on that object, which is what lets a client fetch nine of
/// ten objects and be told precisely which one is missing.
pub async fn batch(
    store: &dyn ObjectStore,
    prefix: &str,
    req: &BatchRequest,
    ttl_secs: u64,
) -> Result<BatchResponse, BatchError> {
    if let Some(algo) = req.hash_algo.as_deref() {
        if algo != "sha256" {
            return Err(BatchError {
                message: format!("this server keys objects by sha256, not {algo:?}"),
            });
        }
    }
    if !req.transfers.is_empty() && !req.transfers.iter().any(|t| t == "basic") {
        // `basic` is the only transfer that works against presigned
        // URLs. Saying so beats negotiating down to something this
        // server cannot do.
        return Err(BatchError {
            message: format!(
                "this server speaks the `basic` transfer; the client offered {:?}",
                req.transfers
            ),
        });
    }
    if req.objects.len() > MAX_BATCH {
        return Err(BatchError {
            message: format!("{} objects in one batch; the limit is {MAX_BATCH}", req.objects.len()),
        });
    }
    let upload = match req.operation.as_str() {
        "download" => false,
        "upload" => true,
        other => {
            return Err(BatchError {
                message: format!("unknown operation {other:?} (want download or upload)"),
            })
        }
    };

    let mut out = Vec::with_capacity(req.objects.len());
    for spec in &req.objects {
        if !valid_oid(&spec.oid) {
            out.push(refuse(&spec.oid, spec.size, 422, "oid is not a sha256 in lower-case hex"));
            continue;
        }
        let key = object_key(prefix, &spec.oid);
        let present = match store.head(&key).await {
            Ok(meta) => Some(meta),
            Err(StoreError::NotFound(_)) => None,
            Err(e) => {
                // The store is having a moment. Refusing this object
                // is honest; claiming it is absent would make a client
                // re-upload bytes that are already there, and claiming
                // it is present would hand out a URL to nothing.
                out.push(refuse(&spec.oid, spec.size, 503, format!("cannot reach the object store: {e}")));
                continue;
            }
        };

        let mut actions = BTreeMap::new();
        let mut error = None;
        if upload {
            match present {
                // Already there: NO actions, which is how the protocol
                // says "you already have this". It is the dedupe that
                // makes LFS cheap — a rebased branch re-pushing the
                // same checkpoint uploads nothing.
                Some(_) => {}
                None => match store.presign_put(&key, ttl_secs).await {
                    Ok(href) => {
                        actions.insert(
                            "upload".to_string(),
                            Action { href, header: BTreeMap::new(), expires_in: ttl_secs },
                        );
                        // `verify` turns "the client says it uploaded"
                        // into "the server HEADed it". Without it a
                        // failed PUT is silently accepted, because the
                        // bytes never came past us to be counted.
                        actions.insert(
                            "verify".to_string(),
                            Action {
                                href: String::new(),
                                header: BTreeMap::new(),
                                expires_in: ttl_secs,
                            },
                        );
                    }
                    Err(e) => error = Some(ObjectError { code: 503, message: e.to_string() }),
                },
            }
        } else {
            match present {
                Some(meta) if meta.size != spec.size && spec.size > 0 => {
                    error = Some(ObjectError {
                        code: 422,
                        message: format!(
                            "the stored object is {} bytes and the pointer says {}",
                            meta.size, spec.size
                        ),
                    });
                }
                Some(_) => match store.presign_get(&key, ttl_secs).await {
                    Ok(href) => {
                        actions.insert(
                            "download".to_string(),
                            Action { href, header: BTreeMap::new(), expires_in: ttl_secs },
                        );
                    }
                    Err(e) => error = Some(ObjectError { code: 503, message: e.to_string() }),
                },
                None => {
                    error = Some(ObjectError { code: 404, message: "no such object".into() });
                }
            }
        }
        out.push(ObjectResponse {
            oid: spec.oid.clone(),
            size: spec.size,
            authenticated: true,
            actions,
            error,
        });
    }
    Ok(BatchResponse { transfer: "basic".to_string(), objects: out })
}

/// The `verify` action: did the object the client claims to have
/// uploaded actually land, at the size it claimed?
///
/// The check is worth making precisely because the bytes did not come
/// through here. A presigned PUT is a grant to write at a key, and
/// nothing about it proves the write happened or finished.
pub async fn verify(
    store: &dyn ObjectStore,
    prefix: &str,
    spec: &ObjectSpec,
) -> Result<(), (u16, String)> {
    if !valid_oid(&spec.oid) {
        return Err((422, "oid is not a sha256 in lower-case hex".into()));
    }
    match store.head(&object_key(prefix, &spec.oid)).await {
        Ok(meta) if spec.size == 0 || meta.size == spec.size => Ok(()),
        Ok(meta) => Err((
            422,
            format!("uploaded object is {} bytes and the pointer says {}", meta.size, spec.size),
        )),
        Err(StoreError::NotFound(_)) => {
            Err((404, "the object did not arrive; the upload did not complete".into()))
        }
        Err(e) => Err((503, format!("cannot reach the object store: {e}"))),
    }
}
