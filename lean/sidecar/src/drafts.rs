//! Per-user DRAFTS: a durable edit that is deliberately NOT published.
//!
//! The gap this closes. `PUT /files/{path}` is a LIVE write — the
//! object lands at `files/<path>`, the real key, and the sidecar
//! integrates it at step 1 of the next barrier (default cadence 60 s).
//! Nothing gates it and nobody approves it, so there was no way to say
//! "I edited this, keep it for me, don't show it to anyone yet."
//! Keeping it in the browser is not an answer: the requirement is that
//! it survives closing the laptop.
//!
//! The whole design is one key choice — drafts live under `LEAN_DIR`,
//! the reserved namespace. `classify` reads the local tree and the
//! baseline, `checkout` materialises manifest citations, and both
//! sweeps are prefix-scoped (`manifests/`, `chunks/`). A draft is
//! therefore durable in the bucket and simultaneously invisible to the
//! agent, to the manifest, to every other user, and to the collector.
//! No barrier change, no `classify` change, no inbox change.
//!
//! ## The base etag is RECORDED, never enforced at save time
//!
//! A draft carries the etag of `files/<path>` as the editor read it.
//! Enforcing it on save would be exactly wrong: if a sibling published
//! between open and save, refusing the save DESTROYS the user's edit —
//! the one outcome the feature exists to prevent. So a save always
//! succeeds, and the staleness surfaces twice where it is actionable:
//! advisory (`stale` in the listing, so the resume view can say so) and
//! binding (`promote` conditions the publish on that etag and is
//! refused 409 if it moved).
//!
//! ## Two objects, and the order is load-bearing
//!
//! Body first, meta second. The only crash residue is a body with no
//! meta — an INCOMPLETE draft, which `promote` refuses by name. The
//! other order leaves a meta naming bytes that do not exist, which
//! reads as a draft the user can open and cannot.
//!
//! Delete removes meta first for the same reason: the intermediate
//! state is the one already-legal incomplete shape.
//!
//! ## What a draft is NOT
//!
//! Not a branch, not a proposal, not an approval gate. Promote
//! publishes immediately under the author's own authority; there is no
//! reviewer. `docs/plans/flint-lean-branching-design.md` §4.4 is the
//! design for *that*, and it is a different and much larger build.
//!
//! Not merged, either. A 409 from `promote` says the file moved; it
//! does not say how to reconcile. `inst_base` is the three-way base for
//! the AGENT's tree and does not cover drafts. The caller shows both
//! versions and the human redoes the edit.
//!
//! Not reaped. Nothing collects drafts — by construction, since the
//! sweeps are prefix-scoped and that is what keeps them safe. Retention
//! is the caller's, and this is the same orphan shape the preserved
//! conflict bytes under `conflicts/<uuid>/` already have.

use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use warp::http::StatusCode;
use warp::Reply;

use flint_store::{crc64_nvme, GenerationStamps, ObjectStore, PutCondition, StoreError};

use super::gateway::{err_reply, normalize_etag, ok_json, path_ok, EtagResp, GatewayCore};
use super::inbox::{self, InboxEntry};
use super::{now_unix, LeanConfig, LeanError};

/// What a draft was edited against, and by whom.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DraftMeta {
    pub path: String,
    /// The etag of `files/<path>` when the editor opened it. `None`
    /// means the editor claims the file did not exist — promote then
    /// conditions on create-if-absent, so a file that appeared in the
    /// meantime refuses instead of being clobbered. A caller that
    /// simply forgot the header lands in that same fail-closed arm.
    pub base_etag: Option<String>,
    pub author: String,
    pub updated_unix: u64,
    /// The body object's etag as written. Carried so `promote` can
    /// guard the COPY SOURCE: a second tab that re-saved the draft
    /// between this read and the copy would otherwise publish bytes
    /// this request never saw.
    pub body_etag: String,
    pub size: u64,
}

/// One row of the resume view.
#[derive(Debug, Serialize)]
struct DraftRow {
    path: String,
    base_etag: Option<String>,
    author: String,
    updated_unix: u64,
    size: u64,
    /// `files/<path>` has moved since this draft was taken: promoting
    /// it will be refused until the user reconciles. Advisory — it is
    /// a HEAD taken now, and the binding check is promote's own.
    stale: bool,
}

#[derive(Debug, Serialize)]
struct DraftList {
    drafts: Vec<DraftRow>,
}

/// A user id is one path segment of the key, so it gets the same
/// hygiene the workspace-relative path gets — and it must not be empty,
/// which would collapse `drafts//body/x` into a key no listing prefix
/// can separate from another user's.
pub(crate) fn user_ok(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= 128
        && user != "."
        && user != ".."
        && !user.contains('/')
        && !user.contains('\\')
}

fn stamps(author: &str) -> GenerationStamps {
    GenerationStamps {
        generation: 0,
        // A draft is not a publish and holds no lease. `epoch: 0`
        // matches the gateway's own HITL write, and for the same
        // reason: this is the second writer, not the holder.
        epoch: 0,
        flush_uuid: format!("draft-{author}"),
        boundary_source: None,
        posix: None,
    }
}

async fn load_meta(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    user: &str,
    path: &str,
) -> Result<Option<DraftMeta>, StoreError> {
    match store.get_whole(&cfg.draft_meta_key(user, path), None).await {
        Ok((_, body)) => Ok(serde_json::from_slice(&body).ok()),
        Err(StoreError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

// ── handlers ─────────────────────────────────────────────────────────

/// `PUT /lean/v1/{ws}/drafts/{user}/{path}` — save, overwriting any
/// previous draft of the same path by the same user.
///
/// Unconditional on purpose (see the module note): the save is the
/// durability promise, and a save that can be refused is not one.
pub async fn handle_draft_put(
    core: Arc<GatewayCore>,
    ws: String,
    user: String,
    path: String,
    author: Option<String>,
    base_etag: Option<String>,
    body: Bytes,
) -> warp::reply::Response {
    let Some(cfg) = core.cfg(&ws) else {
        return err_reply(StatusCode::NOT_FOUND, "unknown-workspace", ws);
    };
    if !user_ok(&user) {
        return err_reply(StatusCode::BAD_REQUEST, "bad-user", user);
    }
    if !path_ok(&path) {
        return err_reply(StatusCode::BAD_REQUEST, "bad-path", path);
    }
    let size = body.len() as u64;
    let author = author.unwrap_or_else(|| user.clone());
    // Recorded VERBATIM. An entity-tag is an opaque token, and the one
    // thing this must never do is invent a second normalisation rule —
    // `gateway::normalize_etag` is the crate's, applied at COMPARISON,
    // and the first version of this line stripped the quotes here
    // instead, so a stored bare tag never matched the store's quoted
    // one and every promote came back `draft-stale`.
    let base_etag = base_etag.map(|e| e.trim().to_string());

    // Body FIRST. A crash here leaves nothing; a crash after it leaves
    // an incomplete draft, which promote names and refuses.
    //
    // `Unconditional` against that variant's own warning, and this is
    // the justification. A save must never be refused — that is the
    // whole durability promise — so a second tab cannot be handed a 412
    // here, and last-write-wins is what an editor's autosave means
    // anyway. The distinction the warning protects is not lost, only
    // MOVED: the meta records the resulting `body_etag`, promote guards
    // the copy source with it, and a promote racing a re-save is
    // refused `draft-moved` rather than publishing bytes it never saw.
    let crc = crc64_nvme(&body);
    let body_meta = match core
        .store
        .put_whole(
            &cfg.draft_body_key(&user, &path),
            body,
            &PutCondition::Unconditional,
            &stamps(&author),
            crc,
        )
        .await
    {
        Ok(m) => m,
        Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    };

    let meta = DraftMeta {
        path: path.clone(),
        base_etag,
        author,
        updated_unix: now_unix(),
        body_etag: body_meta.etag.clone(),
        size,
    };
    let doc = match serde_json::to_vec(&meta) {
        Ok(d) => Bytes::from(d),
        Err(e) => return err_reply(StatusCode::INTERNAL_SERVER_ERROR, "encode", e.to_string()),
    };
    let crc = crc64_nvme(&doc);
    match core
        .store
        .put_whole(
            &cfg.draft_meta_key(&user, &path),
            doc,
            &PutCondition::Unconditional,
            &stamps(&meta.author),
            crc,
        )
        .await
    {
        Ok(_) => ok_json(&EtagResp { etag: body_meta.etag }),
        Err(e) => err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    }
}

/// `GET /lean/v1/{ws}/drafts/{user}/{path}` — the saved bytes, with the
/// recorded base on `x-flint-base-etag` so the caller can compare it
/// against a fresh read of `files/{path}` itself.
pub async fn handle_draft_get(
    core: Arc<GatewayCore>,
    ws: String,
    user: String,
    path: String,
) -> warp::reply::Response {
    let Some(cfg) = core.cfg(&ws) else {
        return err_reply(StatusCode::NOT_FOUND, "unknown-workspace", ws);
    };
    if !user_ok(&user) {
        return err_reply(StatusCode::BAD_REQUEST, "bad-user", user);
    }
    if !path_ok(&path) {
        return err_reply(StatusCode::BAD_REQUEST, "bad-path", path);
    }
    let meta = match load_meta(core.store.as_ref(), &cfg, &user, &path).await {
        Ok(m) => m,
        Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    };
    match core.store.get_whole(&cfg.draft_body_key(&user, &path), None).await {
        Ok((m, body)) => {
            let mut res = warp::reply::Response::new(body.into());
            if let Ok(v) = warp::http::HeaderValue::from_str(&m.etag) {
                res.headers_mut().insert("etag", v);
            }
            if let Some(b) = meta.as_ref().and_then(|m| m.base_etag.as_ref()) {
                if let Ok(v) = warp::http::HeaderValue::from_str(b) {
                    res.headers_mut().insert("x-flint-base-etag", v);
                }
            }
            // An incomplete draft is readable — the bytes are the
            // user's work and withholding them helps nobody — but the
            // caller is told, because promote will refuse it.
            if meta.is_none() {
                res.headers_mut()
                    .insert("x-flint-draft-incomplete", warp::http::HeaderValue::from_static("1"));
            }
            res
        }
        Err(StoreError::NotFound(_)) => {
            err_reply(StatusCode::NOT_FOUND, "no-draft", format!("{user}: {path}"))
        }
        Err(e) => err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    }
}

/// `GET /lean/v1/{ws}/drafts/{user}` — the resume view.
///
/// One HEAD of `files/<path>` per draft to fill `stale`. That is N+1
/// requests for N drafts, taken deliberately: the whole point of the
/// listing is the user coming back days later, and "which of these can
/// still be published" is the question they have.
pub async fn handle_draft_list(
    core: Arc<GatewayCore>,
    ws: String,
    user: String,
) -> warp::reply::Response {
    let Some(cfg) = core.cfg(&ws) else {
        return err_reply(StatusCode::NOT_FOUND, "unknown-workspace", ws);
    };
    if !user_ok(&user) {
        return err_reply(StatusCode::BAD_REQUEST, "bad-user", user);
    }
    let prefix = cfg.draft_meta_prefix(&user);
    let listed = match core.store.list(&prefix).await {
        Ok(l) => l,
        Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    };
    let mut drafts = vec![];
    for o in listed {
        let Some(path) = o.key.strip_prefix(&prefix) else { continue };
        let Ok(Some(meta)) = load_meta(core.store.as_ref(), &cfg, &user, path).await else {
            continue;
        };
        let current = match core.store.head(&cfg.file_key(path)).await {
            Ok(m) => Some(m.etag),
            Err(StoreError::NotFound(_)) => None,
            Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
        };
        let stale = match (current.as_deref(), meta.base_etag.as_deref()) {
            (None, None) => false,
            (Some(c), Some(b)) => normalize_etag(c) != normalize_etag(b),
            _ => true,
        };
        drafts.push(DraftRow {
            path: meta.path,
            stale,
            base_etag: meta.base_etag,
            author: meta.author,
            updated_unix: meta.updated_unix,
            size: meta.size,
        });
    }
    ok_json(&DraftList { drafts })
}

/// `DELETE /lean/v1/{ws}/drafts/{user}/{path}` — discard.
///
/// Meta first: the window leaves the one incomplete shape the rest of
/// this module already handles, rather than a meta pointing at bytes
/// that are gone.
pub async fn handle_draft_delete(
    core: Arc<GatewayCore>,
    ws: String,
    user: String,
    path: String,
) -> warp::reply::Response {
    let Some(cfg) = core.cfg(&ws) else {
        return err_reply(StatusCode::NOT_FOUND, "unknown-workspace", ws);
    };
    if !user_ok(&user) {
        return err_reply(StatusCode::BAD_REQUEST, "bad-user", user);
    }
    if !path_ok(&path) {
        return err_reply(StatusCode::BAD_REQUEST, "bad-path", path);
    }
    if let Err(e) = core.store.delete(&cfg.draft_meta_key(&user, &path)).await {
        return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string());
    }
    match core.store.delete(&cfg.draft_body_key(&user, &path)).await {
        Ok(()) => warp::reply::with_status("", StatusCode::NO_CONTENT).into_response(),
        Err(e) => err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    }
}

/// `POST /lean/v1/{ws}/drafts/{user}/{path}` — publish it.
///
/// POST is the verb; there is no `/promote` suffix (see the router note
/// on why a suffix would be ambiguous).
///
/// This IS a HITL write and takes the whole HITL discipline: the
/// barrier window gate, the conditional publish, the inbox entry
/// second. The one addition is that the precondition comes from the
/// DRAFT's recorded base rather than from a header the caller still
/// remembers — which is the entire point, since the caller may be a
/// browser that was closed for a week.
pub async fn handle_draft_promote(
    core: Arc<GatewayCore>,
    ws: String,
    user: String,
    path: String,
    author: Option<String>,
) -> warp::reply::Response {
    let Some(cfg) = core.cfg(&ws) else {
        return err_reply(StatusCode::NOT_FOUND, "unknown-workspace", ws);
    };
    if !user_ok(&user) {
        return err_reply(StatusCode::BAD_REQUEST, "bad-user", user);
    }
    if !path_ok(&path) {
        return err_reply(StatusCode::BAD_REQUEST, "bad-path", path);
    }

    let meta = match load_meta(core.store.as_ref(), &cfg, &user, &path).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            // Either no draft at all, or a body whose meta never
            // landed. Both refuse here rather than guessing a base:
            // promoting an incomplete draft would have to choose
            // between clobbering unconditionally and creating blindly,
            // and each is wrong in exactly the case the other is right.
            return err_reply(
                StatusCode::NOT_FOUND,
                "no-draft",
                format!(
                    "{user} has no complete draft of {path}; if the body exists its base was \
                     never recorded — re-save the draft to publish it"
                ),
            );
        }
        Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    };

    // The window check, exactly as `handle_files_put` does it: every
    // stateless replica reads the CELL.
    let loaded = match inbox::load(core.store.as_ref(), &cfg).await {
        Ok(l) => l,
        Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    };
    if !inbox::admits_hitl(&loaded.doc) {
        let retry = loaded
            .doc
            .window
            .as_ref()
            .map(|w| w.deadline_unix.saturating_sub(now_unix()).max(1))
            .unwrap_or(2);
        let mut res = err_reply(
            StatusCode::CONFLICT,
            "barrier-window-open",
            "a publish barrier is in flight; retry after the window".into(),
        );
        if let Ok(v) = warp::http::HeaderValue::from_str(&retry.to_string()) {
            res.headers_mut().insert("retry-after", v);
        }
        return res;
    }

    // Resolve the base against what is there NOW, and compare through
    // the crate's one normalisation rule. Deciding staleness HERE
    // rather than inferring it from a 412 means the answer names both
    // versions, and the condition below carries the STORE's own form of
    // the etag — never the caller's, which may or may not be quoted.
    let current = match core.store.head(&cfg.file_key(&path)).await {
        Ok(m) => Some(m.etag),
        Err(StoreError::NotFound(_)) => None,
        Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    };
    let cond = match (current.as_deref(), meta.base_etag.as_deref()) {
        // Edited against this exact version: publish over it.
        (Some(cur), Some(b)) if normalize_etag(cur) == normalize_etag(b) => {
            PutCondition::IfMatch(cur.to_string())
        }
        // Edited as a new file, and still new: create it.
        (None, None) => PutCondition::IfNoneMatchAny,
        // Everything else is stale, and each shape is a real one: the
        // file moved under the draft, it was deleted under the draft,
        // or it appeared where the draft expected nothing.
        (cur, b) => {
            let mut res = err_reply(
                StatusCode::CONFLICT,
                "draft-stale",
                match (cur, b) {
                    (None, Some(b)) => format!(
                        "{path} was DELETED since this draft was taken (draft base {b}); \
                         the draft is KEPT — re-create the file or discard the draft"
                    ),
                    (Some(cur), None) => format!(
                        "{path} was CREATED since this draft was taken, which expected no \
                         file (now {cur}); the draft is KEPT — re-read and reconcile"
                    ),
                    (Some(cur), Some(b)) => format!(
                        "{path} changed since this draft was taken (draft base {b}, now \
                         {cur}); the draft is KEPT — re-read the file and reconcile"
                    ),
                    (None, None) => unreachable!("matched by the create arm above"),
                },
            );
            if let Some(v) = cur.and_then(|c| warp::http::HeaderValue::from_str(c).ok()) {
                res.headers_mut().insert("x-flint-current-etag", v);
            }
            return res;
        }
    };

    let author = author.unwrap_or_else(|| meta.author.clone());
    let publish_stamps = GenerationStamps {
        generation: 0,
        epoch: 0,
        flush_uuid: format!("draft-promote-{}", uuid::Uuid::new_v4()),
        boundary_source: None,
        posix: None,
    };

    // Server-side copy: the bytes never traverse the gateway. The
    // source guard is the body etag the meta recorded, so a second tab
    // that re-saved this draft after we read the meta publishes
    // nothing — it fails here instead of silently shipping bytes this
    // request never saw.
    let published = match core
        .store
        .copy_object(
            &cfg.draft_body_key(&user, &path),
            Some(&meta.body_etag),
            &cfg.file_key(&path),
            &cond,
            &publish_stamps,
        )
        .await
    {
        Ok(m) => m,
        Err(StoreError::PreconditionFailed(_)) => {
            // Staleness was already ruled out above, so this is the
            // SOURCE guard: the draft body moved under us (a second tab
            // re-saved it), or the destination moved inside the
            // HEAD-to-copy window. Both are retryable, which is the
            // opposite of the advice `draft-stale` carries.
            return err_reply(
                StatusCode::CONFLICT,
                "draft-moved",
                format!(
                    "the draft of {path} or the file under it moved during this promote; retry"
                ),
            );
        }
        Err(StoreError::NotFound(_)) => {
            return err_reply(
                StatusCode::NOT_FOUND,
                "no-draft",
                format!("{user}: {path} has meta but no body — re-save the draft"),
            )
        }
        Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    };

    // Inbox entry second — the object-first ordering every HITL write
    // uses. A crash between leaves an orphan object, never a
    // tracked-but-absent entry.
    let entry = InboxEntry {
        path: path.clone(),
        etag: published.etag.clone(),
        author,
        added_unix: now_unix(),
    };
    if let Err(e) = inbox::gateway_append(core.store.as_ref(), &cfg, entry).await {
        return match e {
            LeanError::State(m) => err_reply(StatusCode::CONFLICT, "barrier-window-open", m),
            e => err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
        };
    }

    // Best-effort cleanup. If it fails the draft survives with a base
    // etag that no longer matches what we just published, so a second
    // promote is refused `draft-stale` rather than republishing — the
    // failure degrades to a refusal, never to a double write.
    let _ = core.store.delete(&cfg.draft_meta_key(&user, &path)).await;
    let _ = core.store.delete(&cfg.draft_body_key(&user, &path)).await;

    ok_json(&EtagResp { etag: published.etag })
}
