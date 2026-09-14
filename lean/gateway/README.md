# flint-lean-gateway

The [flint lean](https://github.com/ddalton/flint) gateway as a library.
A backend that reads, writes, deletes, renames, drafts and publishes
files in S3-backed lean workspaces calls these verbs in-process, with one connection to
the bucket, instead of running a `flint-lean-gateway` process and
speaking HTTP to it. Ten workspaces are ten `Workspace` values, not ten
gateways.

The verbs are the gateway's verbs, and this crate is the gateway: the
`flint-lean-gateway` binary that ships in the operator image is `main`
around this library's HTTP router, and the router is a thin skin over
the same `Workspace` methods. An embedder cannot drift from what the
gateway does — same refusals, same preconditions, same order of
writes — because there is one implementation. `VerbError`
carries the status and `error` code the gateway would have answered
with, so a frontend written against the gateway keeps working when the
backend moves in-process.

```toml
[dependencies]
flint-lean-gateway = "0.4"
```

## Quick start

```rust,no_run
use flint_lean_gateway::{connect, Bytes, PutFile, VerbError, Workspace};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
// One connection to the bucket, from the ambient AWS environment.
// `Some("http://minio:9000")` for MinIO or Ozone's S3 gateway.
let store = connect("my-bucket", None).await?;

// One value per workspace: the subtree prefix the syncer was started with.
let ws = Workspace::new(store.clone(), "teams/alpha/project-1");

// A read resolves through the manifest citation, falling back to a
// write no barrier has cited yet.
let blob = ws.get_file("notes/todo.md").await?;

// An overwrite must say what it read. The tag comes back quoted, as
// S3 hands it out; send it back as is.
let etag = ws
    .put_file(
        "notes/todo.md",
        Bytes::from("- ship it\n"),
        &PutFile {
            author: Some("dilip".into()),
            if_match: Some(blob.etag.clone()),
            if_none_match: None,
        },
    )
    .await?;

// A create says the file must not exist.
match ws
    .put_file("notes/new.md", Bytes::from("hi"), &PutFile {
        if_none_match: Some("*".into()),
        ..Default::default()
    })
    .await
{
    Ok(etag) => println!("created at {etag}"),
    Err(VerbError::FileChanged { current }) => println!("someone got there first: {current:?}"),
    Err(e) => return Err(e.into()),
}
# Ok(()) }
```

Without S3, the same code runs against the in-memory store the tests
use — `cargo test` in this crate needs no credentials:

```rust
use std::sync::Arc;
use flint_lean_gateway::{Bytes, MemoryStore, ObjectStore, PutFile, VerbError, Workspace};

# tokio::runtime::Runtime::new().unwrap().block_on(async {
let store: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
let ws = Workspace::new(store, "p");

let etag = ws.put_file("a.txt", Bytes::from("one"), &PutFile::default()).await.unwrap();
assert_eq!(ws.get_file("a.txt").await.unwrap().body, Bytes::from("one"));

// The write is tracked in the inbox until the syncer's next barrier cites it.
let snap = ws.snapshot().await.unwrap();
assert_eq!(snap.inbox.entries[0].etag, etag);
assert!(snap.manifest.entries.is_empty());

// An overwrite without If-Match is refused, and the refusal says so.
let err = ws.put_file("a.txt", Bytes::from("two"), &PutFile::default()).await.unwrap_err();
assert!(matches!(err, VerbError::PreconditionRequired));
assert_eq!((err.status(), err.code()), (428, "precondition-required"));
# });
```

## What a write is, and when others see it

`put_file` returns when the object is at `<prefix>/files/<path>` and an
entry naming it is in the inbox cell at `<prefix>/.flint/lean/inbox`.
Both are in the bucket; there is no other channel. That is the whole
durability promise, and it is immediate.

Visibility has two tiers, by design:

- **Now**: any reader through this crate or through the gateway, and
  any agent pod that runs `sync`. Both overlay the tracked inbox entry
  on the manifest citation, so an overwrite of a file the manifest
  already cites reads as the new bytes the moment `put_file` returns,
  for every reader, with or without a syncer running. The cell is
  consulted only when the cited fetch fails its precondition, so a
  read of a file nobody has overwritten costs what it did before the
  overlay. (Before 0.2.1 `get_file` preferred the citation and
  answered 409 `moved` until the syncer re-cited the path.)
- **At the next barrier**: the manifest. The workspace's syncer
  consumes the inbox at the start of every barrier — on its cadence
  (default 60 s) or sooner on a `request_boundary` — fetches the object,
  verifies its CRC-64, cites it in `<prefix>/.flint/lean/current`, and
  from then on a fresh checkout sees it.

The library never edits the manifest for a HITL write. The syncer that
holds the workspace's lease is the manifest's only writer, and that is
what the protocol's model checks: a second manifest writer would
reintroduce exactly the race the barrier exists to prevent. What the
library offers instead:

- `request_boundary(requestor)` asks the syncer to cite now. It answers
  `recorded`, never `done`; the syncer honours it at its next poll
  (about a second) outside its min-interval and hourly budget.
- `wait_cited(path, etag, timeout, poll)` waits until the manifest cites
  the write, for a status view that wants to show "published", and
  answers `CitationPending` (HTTP 202) when `timeout` passes — the write
  is durable and tracked either way. Not for the request path of a UI.
- `status()` reports the cited seq, the inbox depth, whether a barrier
  window is open, the fence's state (who ran the last boundary, and
  whether a commit section is in progress).

A workspace no syncer ever runs on takes writes and keeps them; the
manifest catches up when a syncer next starts.

## When an immediate answer is not possible

- **A barrier window is open** (the syncer is mid-publish, usually
  well under a second): `VerbError::WindowOpen` with `retry_after_secs`,
  before anything is written. A frontend retries after the hint, or the
  backend sets `Workspace::with_window_wait(Some(duration))` and the
  verb polls the cell until the window closes or the bound passes.
- **The file changed under the user**: `PreconditionRequired` (no
  `If-Match` on an overwrite) or `FileChanged { current }` (a stale one).
  A UI decision — re-read and reconcile — never a retry.
- **The object moved inside the HEAD-to-PUT window**:
  `ConcurrentWrite`, retryable as is. `VerbError::is_retryable` says
  which refusals are.

## Drafts

A draft is a durable edit that is deliberately not published: the bytes
sit under the workspace's reserved namespace where no scan, no
checkout, no manifest and no sweep can see them. Per user, per path.

- `put_draft(user, path, body, author, base_etag)` saves, always. The
  base etag — what the editor read — is recorded, never enforced at
  save time: refusing a save would destroy the edit the feature exists
  to keep.
- `list_drafts(user)` is the resume view; each row says whether the
  file has moved since (`stale`).
- `get_draft(user, path)` returns the bytes, the recorded base, and
  whether the draft is incomplete (a body whose meta never landed).
- `promote_draft(user, path, author)` publishes it as a HITL write
  conditioned on the recorded base: `DraftStale { current }` if the
  file moved, and the draft is kept.
- `delete_draft(user, path)` discards it.

## Delete and rename

A caller outside the pod cannot touch the agent's tree and must never
delete an object itself: a cited object deleted from outside wedges
every checkout with "the manifest cites it but it is gone". So a delete
is DECLARED. `remove_file` records the intent in the inbox cell and
returns; the syncer performs it at its next barrier — unlink, cite out,
GC — and the listing hides the path from the moment it is recorded,
whatever the syncer's cadence. A read of the path by name still answers
until then: the object and its citation are untouched, and a removal
the syncer refuses leaves the file exactly as it was, with nothing to
undo in a reader that never saw it vanish. `rename_file` is a server-side copy to
the destination (the bytes never traverse your process, so a 10 GB
checkpoint moves without a download) followed by one CAS that records
the destination entry and the source removal together, so the cell
never holds half a rename and the barrier cites both halves in ONE
manifest generation: a manifest reader sees the old name or the new,
never both and never neither.

- `remove_file(path, author, if_match)` and `remove_files(pairs,
  author)`: a folder delete is one transaction, recorded whole or not
  at all.
- `rename_file(from, to, author)` and `rename_files(pairs, author)`: a
  folder move likewise. The destination is readable at once;
  `DestinationExists` if something is already there.
- `withdraw_removal(path)` takes a recorded removal back, best effort
  against a barrier already performing it.
- `Snapshot::listing()` is the file browser's list: citations, overlaid
  by tracked writes, minus pending removals. `pending_removals()` and
  `refused_removals()` are the rest of the story.

**A removal can be refused.** If the agent has unpublished edits on the
path, or created a file there, the syncer applies nothing, keeps the
agent's work, and writes the reason back into the cell — the same rule
it applies to a write over dirty bytes. A refused removal is never
retried (a retry that waited for the agent to publish would delete the
very edit the refusal protected); it stays readable with its `refused`
reason until a newer removal of the path supersedes it or you withdraw
it. `status()` counts pending and refused removals. There is no
function in this crate that deletes a cited object.

## Every verb and its wire code

| Method | Gateway route | Refusals |
|---|---|---|
| `get_file` | `GET /files/{path}` | 404 `no-such-file`, 409 `moved`, 410 `foreign-write` |
| `put_file` | `PUT /files/{path}` | 400 `bad-path` / `bad-precondition`, 409 `barrier-window-open` / `concurrent-write`, 412 `file-changed`, 413 `payload-too-large`, 428 `precondition-required` |
| `remove_file`, `remove_files` | `DELETE /files/{path}` | 404 `no-such-file`, 412 `file-changed` |
| `rename_file`, `rename_files` | `POST /rename` `{from, to}` | 404 `no-such-file`, 409 `destination-exists` / `barrier-window-open` / `concurrent-write` |
| `withdraw_removal` | `DELETE /removals/{path}` | 404 `no-removal` |
| `snapshot` | `GET /snapshot` | |
| `status` | `GET /status` | |
| `request_boundary`, `request_sync` | `POST /boundary`, `POST /sync-request` | |
| `put_draft`, `get_draft`, `list_drafts`, `delete_draft`, `promote_draft` | `/drafts/{user}[/{path}]` | 400 `bad-user`, 404 `no-draft`, 409 `draft-stale` / `draft-moved` |
| `open_window`, `clear_window`, `drop_inbox`, `cas_manifest` | syncer-facing | 403 `stale-epoch` / `no-holder` / `fenced`, 409 `cas-miss` |
| `wait_cited` | library only | 202 `citation-pending`, 409 `superseded` |

Every verb can also fail 502 `store` (the object store said no) and
the path-taking ones 400 `bad-path`. The syncer-facing verbs are
epoch-validated per request and exist because the gateway's HTTP layer
is built on them; a backend serving a UI has no use for them.

## Many workspaces, one process

`Workspace` is an `Arc` and a config. Build one per prefix on a shared
store and keep them in whatever map the backend already has:

```rust,no_run
use std::collections::HashMap;
use flint_lean_gateway::{connect, Workspace};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let store = connect("my-bucket", None).await?;
let mut workspaces: HashMap<String, Workspace> = HashMap::new();
for (id, prefix) in [("alpha", "teams/alpha"), ("beta", "teams/beta")] {
    workspaces.insert(id.into(), Workspace::new(store.clone(), prefix));
}
# Ok(()) }
```

Workspaces in different buckets, or under different credentials, take
different stores; `Workspace::new` takes any `Arc<dyn ObjectStore>`.

## Serving the wire yourself

The `http` feature (on by default) carries the gateway's warp router
(`http::routes`, `http::GatewayCore`) and the binary, for a process
that wants to keep serving the exact HTTP surface `flint-lean-gateway`
serves — same routes, same bearer, same codes — inside its own server.
A backend on another web stack turns it off and never compiles warp:

```toml
flint-lean-gateway = { version = "0.4", default-features = false, features = ["s3"] }
```

## What this crate is not

Not a syncer. It performs no barrier, holds no lease, never touches a
local tree, and never deletes a cited object; `flint-sync` does those,
in the agent's pod, and a delete asked for here is performed there. Not
a `rescope` door either: that verb unlinks local files by scope, and a
library caller has no more business triggering it remotely than the
gateway did.

## License

MIT.
