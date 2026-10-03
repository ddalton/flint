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
flint-lean-gateway = "0.7"
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

// A read resolves through the manifest citation.
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

// The write is CITED when `put_file` returns: the gateway commits it.
let snap = ws.snapshot().await.unwrap();
assert_eq!(snap.manifest.entries["a.txt"].etag, etag);

// An overwrite without If-Match is refused, and the refusal says so.
let err = ws.put_file("a.txt", Bytes::from("two"), &PutFile::default()).await.unwrap_err();
assert!(matches!(err, VerbError::PreconditionRequired));
assert_eq!((err.status(), err.code()), (428, "precondition-required"));
# });
```

## What a write is, and when others see it

`put_file` COMMITS. The bytes land at a fresh handle under
`<prefix>/files/<path>`, then the verb CASes the workspace's manifest
(`<prefix>/.flint/lean/current`) to cite them, and it returns only after
that CAS. That is the whole durability promise, and it is immediate: every
reader through this crate or the gateway, every agent pod that runs
`sync`, and every fresh checkout sees the write the moment `put_file`
returns, with or without a syncer running.

An agent pod's TREE takes it as it takes any other writer's publish: the
syncer's next barrier sees the pointer has moved and, in its first step,
writes the change into the tree wherever the tree is clean and the path is
in its scope (by default within one cadence tick, or sooner on a
`request_boundary`). Nothing is queued: what a tree is owed is derived from
the manifest at each barrier (P1-lite). A path the agent has dirtied is its
work, and its next publish records what it publishes over (R7).

A UI save never waits for the syncers (simplification step 5, P2). It
does not take their lease and is not held back by a barrier in progress:
a syncer mid-publish loses its CAS instead, merges again onto the save,
and retries. So the cost of heavy saving falls on the writers, never on
the person saving.

**An editor autosaves to a draft, never with `put_file`.** A commit is
about four writes (the bytes, the changed manifest chunk, the pointer,
the retire log), moves the pointer every syncer and reader checks, costs
each of them a full manifest load, makes a publishing agent lose its CAS
and retry, and keeps the version it replaced for the retire age (600 s
by default). At an editor's autosave rate that is paid by everyone
holding the workspace, for keystrokes nobody else needs to see yet. A
draft save (`put_draft`, below) is two small PUTs under the user's own
keys and touches nothing shared, and it never refuses. Commit — `put_file`,
or `promote_draft` — when the person saves, at the pace a person saves.

- `request_boundary(requestor)` asks the syncers to publish now. It
  answers `recorded`, never `done`; a syncer honours it at its next poll
  (about a second) outside its min-interval and hourly budget.
- `wait_cited(path, etag, timeout, poll)` answers at once for a save
  (it is cited when acknowledged), and `CitationPending` (HTTP 202) for a
  version nothing cites when `timeout` passes.
- `status()` reports the cited seq and the fence's state.

## When an immediate answer is not possible

- **The file changed under the user**: `PreconditionRequired` (no
  `If-Match` on an overwrite) or `FileChanged { current }` (a stale one).
  The precondition is judged again at the commit, so a version a syncer
  published after the read is named here, never overwritten unseen. A UI
  decision — re-read and reconcile — never a retry.
- **The syncers kept winning the commit**: `ConcurrentWrite`, after
  several lost CASes in a row (each re-reads and re-judges). Retryable as
  is. `VerbError::is_retryable` says which refusals are.
- **A mirror**: a workspace published by exactly one writer takes no UI
  writes, `ReadOnly` (403 `read-only`).

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
- `promote_draft(user, path, author)` publishes it as a save does — it
  COMMITS, conditioned on the recorded base, judged again at the commit:
  `DraftStale { current }` if the file moved, and the draft is kept.
- `delete_draft(user, path)` discards it.

## Delete and rename

A delete and a rename COMMIT, as a save does (P2): each is one CAS on the
workspace's manifest, and each returns once it is committed. Neither
deletes an object: a cited object deleted from outside would wedge every
checkout with "the manifest cites it but it is gone". A delete stops
citing the path (the document's tombstone names what was deleted), and
the object goes to the orphan sweep once nothing cites it. A rename is a
CITATION MOVE: the destination cites the source's handle and the source
is no longer cited, in the same generation, so no bytes move — a 10 GB
checkpoint moves as fast as a text file — and a manifest reader sees the
old name or the new, never both and never neither.

- `remove_file(path, author, if_match)` and `remove_files(pairs,
  author)`: a folder delete is ONE commit, or none — one unknown path or
  failed precondition deletes nothing.
- `rename_file(from, to, author)` and `rename_files(pairs, author)`: a
  folder move likewise. `DestinationExists` if something is already
  there, `NoSuchFile` for a source that is not.
- `Snapshot::listing()` is the file browser's list.

Neither waits on the syncers (G1). An agent whose tree holds the path
follows at its next barrier, as it follows any writer's publish. **An
agent that is EDITING a path the UI deleted keeps its work:** its next
publish brings the path back — its bytes win — and it records the delete
it overrode, naming the deleted version (a `commit-recreated-deleted`
conflict record). The other way round, an agent that DELETES a path the
UI saved since the agent last took it: the delete stands, and the UI's
version is preserved under a `commit-deleted-over-theirs` record (M3).
There is no function in this crate that deletes a cited object; what a
commit stops citing is kept for the retire age (600 s by default) and
only then collected, so a reader that loaded the manifest just before
can still fetch it.

## Every verb and its wire code

| Method | Gateway route | Refusals |
|---|---|---|
| `get_file` | `GET /files/{path}` | 404 `no-such-file`, 409 `moved`, 410 `foreign-write`, 502 `corrupt` (the bytes do not match the citation's CRC-64; never served) |
| `put_file` | `PUT /files/{path}` | 400 `bad-path` / `bad-precondition`, 403 `read-only`, 409 `concurrent-write`, 412 `file-changed`, 413 `payload-too-large`, 428 `precondition-required` |
| `remove_file`, `remove_files` | `DELETE /files/{path}` | 403 `read-only`, 404 `no-such-file`, 409 `concurrent-write`, 412 `file-changed` |
| `rename_file`, `rename_files` | `POST /rename` `{from, to}` | 403 `read-only`, 404 `no-such-file`, 409 `destination-exists` / `concurrent-write` |
| `snapshot` | `GET /snapshot` | |
| `status` | `GET /status` | |
| `request_boundary`, `request_sync` | `POST /boundary`, `POST /sync-request` | 403 `read-only` |
| `put_draft`, `get_draft`, `list_drafts`, `delete_draft`, `promote_draft` | `/drafts/{user}[/{path}]` | 400 `bad-user`, 403 `read-only` (`put_draft`, `delete_draft`, `promote_draft`), 404 `no-draft`, 409 `draft-stale` / `draft-moved` |
| `cas_manifest` | syncer-facing | 403 `stale-epoch` / `no-holder` / `read-only`, 409 `cas-miss` |
| `wait_cited` | library only | 202 `citation-pending`, 409 `superseded` |

Every verb can also fail 502 `store` (the object store said no) and
the path-taking ones 400 `bad-path`. The syncer-facing verbs are
epoch-validated per request and exist because the gateway's HTTP layer
is built on them; a backend serving a UI has no use for them.

## Read-only workspaces

For a user whose role is read access, build the workspace with
`Workspace::read_only(store, prefix)`. The reading verbs (`get_file`,
`snapshot`, `status`, `wait_cited`, `get_draft`, `list_drafts`) answer
exactly as on `Workspace::new`. Every verb that writes answers
`VerbError::ReadOnly` (403 `read-only`, not retryable) before it sends a
request: the table's `403 read-only` rows, the syncer-facing four
included. `is_read_only()` says which kind a workspace is.

The store the workspace holds is wrapped in `flint_store::ReadOnly`
(re-exported as `ReadOnly`), and that wrapper is what enforces it inside
this crate: `store()` is public, and a write sent through it is refused
with `StoreError::Auth` and never reaches the bucket. The typed refusal is
the honest answer; the wrapper is the one that holds when a caller goes
around the verbs.

Neither is the real enforcement. The credential is: build a read-only
workspace on a store whose credential cannot write (on S3, keys whose
policy grants only reads on the prefix), so that the bucket refuses
whatever this crate misses. One read-only client and one read-write
client per bucket serve every user; nothing here needs a client per user.

```rust
use std::sync::Arc;
use flint_lean_gateway::{Bytes, MemoryStore, ObjectStore, PutFile, StoreError, VerbError, Workspace};

# tokio::runtime::Runtime::new().unwrap().block_on(async {
let store: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
let editor = Workspace::new(store.clone(), "p");
editor.put_file("a.txt", Bytes::from("one"), &PutFile::default()).await.unwrap();

let viewer = Workspace::read_only(store, "p");
assert_eq!(viewer.get_file("a.txt").await.unwrap().body, Bytes::from("one"));

let err = viewer.put_file("b.txt", Bytes::from("two"), &PutFile::default()).await.unwrap_err();
assert!(matches!(err, VerbError::ReadOnly));
assert_eq!((err.status(), err.code()), (403, "read-only"));

// Around the verbs, the store refuses too.
let refused = viewer.store().delete("p/files/a.txt").await.unwrap_err();
assert!(matches!(refused, StoreError::Auth(_)));
# });
```

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

## Explicit credentials, for tests

`connect` reads the ambient AWS environment. A test rig that holds a
key pair for MinIO, Ozone's S3 gateway or localstack passes it instead;
nothing is read from the environment, and the bucket is addressed
path-style on the endpoint:

```rust,no_run
use flint_lean_gateway::{connect_with_credentials, Workspace};

# fn run() -> Result<(), Box<dyn std::error::Error>> {
let store = connect_with_credentials(
    "test-bucket",
    "http://localhost:9000",
    "us-east-1",
    "test-access-key",
    "test-secret-key",
)?;
let ws = Workspace::new(store, "teams/alpha/project-1");
# let _ = ws; Ok(()) }
```

The same constructor is `S3Store::with_credentials` for a caller that
wants the store's own builder methods before wrapping it.

## Serving the wire yourself

The `http` feature (on by default) carries the gateway's warp router
(`http::routes`, `http::GatewayCore`) and the binary, for a process
that wants to keep serving the exact HTTP surface `flint-lean-gateway`
serves — same routes, same bearer, same codes — inside its own server.
A backend on another web stack turns it off and never compiles warp:

```toml
flint-lean-gateway = { version = "0.7", default-features = false, features = ["s3"] }
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
