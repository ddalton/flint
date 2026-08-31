//! The prefix OWNER object — B12 (prefix-reuse adoption).
//!
//! Delete a flint-lite project and create another on the same bucket
//! prefix, and the new one reached `Ready` serving the old project's
//! files: import-on-start is the DR feature, a fresh PVC means fresh
//! state, and nothing in the bucket said who the bytes belonged to.
//! The epoch object cannot say it — its holder id is the persisted
//! `server_id`, minted per state.db, so every hibernate/resume cycle
//! legitimately arrives with a new one; the epoch is a LIVENESS lease
//! (a dead foreign holder is superseded), not an ownership claim.
//!
//! This module is the ownership claim: one small JSON object at
//! `<prefix>.flint/owner` stamped with an identity that survives state
//! loss but NOT project deletion — the operator passes the FlintShare's
//! `metadata.uid`. Hibernate/resume keeps the CR, so the same uid
//! returns and the restore proceeds; delete-and-recreate (or a
//! conflict loser promoted after the winner's deletion) arrives with a
//! different uid and is REFUSED AT STARTUP, before the epoch claim —
//! never supersede an epoch on a prefix that is not ours — and long
//! before the import could materialize a stranger's namespace.
//!
//! Taking over a prefix deliberately is one explicit knob:
//! `adoptData: true` rewrites the owner object to the new identity
//! (guarded by If-Match, so a concurrent owner change wins) and
//! proceeds. Set it for the migration, then remove it.
//!
//! Why not "prefix retirement" (the other design that was on the
//! table): retirement needs someone to write a tombstone at DELETE
//! time, and the operator holds no bucket credentials by design — and
//! deletion may never run at all (crash, kubectl bypass). The owner
//! stamp is written by the hub, which already writes the epoch.
//!
//! A hub with NO configured identity (hand-rolled config, chart
//! without the value) skips enforcement and writes nothing — the
//! pre-B12 posture, unchanged.

use crate::tier::epoch::RESERVED_DIR;
use crate::tier::store::{
    crc64_nvme, GenerationStamps, ObjectStore, PutCondition, StoreError,
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

pub const OWNER_VERSION: u32 = 1;

/// Where a prefix's owner object lives.
pub fn owner_key(key_prefix: &str) -> String {
    format!("{}{}/owner", key_prefix, RESERVED_DIR)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnerDoc {
    pub version: u32,
    /// The owning share's identity, compared verbatim (the operator
    /// passes `metadata.uid`; a standalone deployment may pin any
    /// stable string through the chart).
    pub identity: String,
    pub written_unix: u64,
}

/// What the gate decided. Every arm except `Foreign` lets startup
/// proceed; `Foreign` is a startup refusal upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerVerdict {
    /// No identity configured — enforcement skipped, nothing written.
    Unenforced,
    /// No owner object existed; ours is now stamped.
    FirstClaim,
    /// The owner object names us.
    Held,
    /// The owner object named someone else and `adoptData` rewrote it.
    Adopted { previous: String },
    /// The owner object names someone else: the prefix is not ours.
    Foreign { holder: String },
}

fn doc_bytes(identity: &str) -> Bytes {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let doc = OwnerDoc {
        version: OWNER_VERSION,
        identity: identity.to_string(),
        written_unix: now,
    };
    // Infallible for this shape (same posture as Manifest::to_bytes).
    Bytes::from(serde_json::to_vec(&doc).unwrap_or_default())
}

fn stamps() -> GenerationStamps {
    GenerationStamps {
        generation: 0,
        epoch: 0,
        flush_uuid: "owner".into(),
        boundary_source: None,
        posix: None,
    }
}

/// The gate. Called at tier startup after the bucket posture check and
/// BEFORE the epoch claim; a `Foreign` verdict (or any error) refuses
/// startup up there.
///
/// Store errors abort startup rather than degrade: "I could not read
/// who owns this prefix" must never resolve to "so it is mine" — the
/// same fail-closed posture as the unreadable-manifest import refusal.
pub async fn enforce(
    store: &dyn ObjectStore,
    key_prefix: &str,
    identity: Option<&str>,
    adopt: bool,
) -> Result<OwnerVerdict, String> {
    let identity = match identity {
        Some(id) if !id.is_empty() => id,
        _ => return Ok(OwnerVerdict::Unenforced),
    };
    let key = owner_key(key_prefix);

    // Two passes: a lost If-None-Match race re-reads once and judges
    // the winner's object like any pre-existing one.
    for attempt in 0..2 {
        match store.get_whole(&key, None).await {
            Ok((meta, bytes)) => {
                let holder = match serde_json::from_slice::<OwnerDoc>(&bytes) {
                    Ok(doc) => doc.identity,
                    Err(e) if adopt => {
                        // Unreadable + explicit adoption: overwriting is
                        // exactly what the knob authorizes.
                        format!("<unreadable owner object: {}>", e)
                    }
                    Err(e) => {
                        return Err(format!(
                            "the owner object {} exists but cannot be parsed ({}) — \
                             refusing to guess who owns this prefix. Fix or remove \
                             the object, or set adoptData to take the prefix over",
                            key, e
                        ));
                    }
                };
                if holder == identity {
                    return Ok(OwnerVerdict::Held);
                }
                if !adopt {
                    return Ok(OwnerVerdict::Foreign { holder });
                }
                let body = doc_bytes(identity);
                let crc = crc64_nvme(&body);
                return match store
                    .put_whole(&key, body, &PutCondition::IfMatch(meta.etag.clone()), &stamps(), crc)
                    .await
                {
                    Ok(_) => Ok(OwnerVerdict::Adopted { previous: holder }),
                    Err(StoreError::PreconditionFailed(_)) => Err(format!(
                        "owner object {} changed while adopting it — a concurrent \
                         owner change wins; retry",
                        key
                    )),
                    Err(e) => Err(format!("owner adoption write failed: {}", e)),
                };
            }
            Err(StoreError::NotFound(_)) => {
                let body = doc_bytes(identity);
                let crc = crc64_nvme(&body);
                match store
                    .put_whole(&key, body, &PutCondition::IfNoneMatchAny, &stamps(), crc)
                    .await
                {
                    Ok(_) => return Ok(OwnerVerdict::FirstClaim),
                    // Raced by another first-claimer: loop re-reads and
                    // judges their object.
                    Err(StoreError::PreconditionFailed(_)) if attempt == 0 => continue,
                    Err(e) => return Err(format!("owner first-claim write failed: {}", e)),
                }
            }
            Err(e) => return Err(format!("owner object read failed: {}", e)),
        }
    }
    Err("owner claim raced twice — the store is not settling; retry".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::store::memory::MemoryStore;
    use std::sync::Arc;

    const PREFIX: &str = "proj1/";

    async fn owner_on_store(store: &MemoryStore) -> Option<String> {
        match store.get_whole(&owner_key(PREFIX), None).await {
            Ok((_, b)) => Some(serde_json::from_slice::<OwnerDoc>(&b).unwrap().identity),
            Err(_) => None,
        }
    }

    #[tokio::test]
    async fn no_identity_means_no_enforcement_and_no_object() {
        let store = Arc::new(MemoryStore::new());
        let v = enforce(store.as_ref(), PREFIX, None, false).await.unwrap();
        assert_eq!(v, OwnerVerdict::Unenforced);
        let v = enforce(store.as_ref(), PREFIX, Some(""), false).await.unwrap();
        assert_eq!(v, OwnerVerdict::Unenforced, "empty string is not an identity");
        assert_eq!(owner_on_store(&store).await, None, "skip must write NOTHING");
    }

    #[tokio::test]
    async fn first_claim_stamps_then_holds() {
        let store = Arc::new(MemoryStore::new());
        let v = enforce(store.as_ref(), PREFIX, Some("uid-a"), false).await.unwrap();
        assert_eq!(v, OwnerVerdict::FirstClaim);
        assert_eq!(owner_on_store(&store).await.as_deref(), Some("uid-a"));
        // The hibernate/resume shape: same CR, fresh state, same uid.
        let v = enforce(store.as_ref(), PREFIX, Some("uid-a"), false).await.unwrap();
        assert_eq!(v, OwnerVerdict::Held);
    }

    /// THE B12 hole, closed: a recreated share (new uid) on a reused
    /// prefix is refused. The control arm right above (same uid →
    /// Held) proves the refusal is the MISMATCH, not the code path.
    #[tokio::test]
    async fn a_reused_prefix_is_refused() {
        let store = Arc::new(MemoryStore::new());
        enforce(store.as_ref(), PREFIX, Some("uid-a"), false).await.unwrap();
        let v = enforce(store.as_ref(), PREFIX, Some("uid-b"), false).await.unwrap();
        assert_eq!(v, OwnerVerdict::Foreign { holder: "uid-a".into() });
        assert_eq!(
            owner_on_store(&store).await.as_deref(),
            Some("uid-a"),
            "a refusal must not touch the owner object"
        );
    }

    #[tokio::test]
    async fn adoption_rewrites_the_owner_and_reverses_the_roles() {
        let store = Arc::new(MemoryStore::new());
        enforce(store.as_ref(), PREFIX, Some("uid-a"), false).await.unwrap();
        let v = enforce(store.as_ref(), PREFIX, Some("uid-b"), true).await.unwrap();
        assert_eq!(v, OwnerVerdict::Adopted { previous: "uid-a".into() });
        // The rewrite stuck: B now holds WITHOUT the knob…
        let v = enforce(store.as_ref(), PREFIX, Some("uid-b"), false).await.unwrap();
        assert_eq!(v, OwnerVerdict::Held);
        // …and the deposed owner is now the foreigner.
        let v = enforce(store.as_ref(), PREFIX, Some("uid-a"), false).await.unwrap();
        assert_eq!(v, OwnerVerdict::Foreign { holder: "uid-b".into() });
    }

    #[tokio::test]
    async fn an_unreadable_owner_object_fails_closed_unless_adopting() {
        let store = Arc::new(MemoryStore::new());
        store
            .put_whole(
                &owner_key(PREFIX),
                Bytes::from_static(b"not json"),
                &PutCondition::IfNoneMatchAny,
                &stamps(),
                crc64_nvme(b"not json"),
            )
            .await
            .unwrap();
        let err = enforce(store.as_ref(), PREFIX, Some("uid-a"), false)
            .await
            .unwrap_err();
        assert!(err.contains("cannot be parsed"), "got: {}", err);
        // adoptData is the operator's answer to a mangled owner object.
        let v = enforce(store.as_ref(), PREFIX, Some("uid-a"), true).await.unwrap();
        assert!(matches!(v, OwnerVerdict::Adopted { .. }), "got: {:?}", v);
        assert_eq!(owner_on_store(&store).await.as_deref(), Some("uid-a"));
    }

    /// The doc round-trips and carries its version — the field future
    /// evolutions (S12's epoch ancestry) will discriminate on.
    #[test]
    fn owner_doc_shape() {
        let b = doc_bytes("uid-x");
        let doc: OwnerDoc = serde_json::from_slice(&b).unwrap();
        assert_eq!(doc.version, OWNER_VERSION);
        assert_eq!(doc.identity, "uid-x");
        assert!(doc.written_unix > 0);
    }
}
