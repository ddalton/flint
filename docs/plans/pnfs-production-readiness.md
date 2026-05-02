# pNFS production readiness — plan for Tasks #4 + #5

**Goal**: make the pNFS data path safe to run in production, where DSes
die and MDS pods get rolled, without losing data or breaking active
mounts. Two well-scoped phases (#4 then #5), then optional Phase C
(replication) builds on top.

**Why this order**: replication (FFL mirroring) is the durability story
users will eventually want, but it physically cannot work without #4
(layout revocation) and #5 (state survives restart). Both are required
prereqs regardless of whether replication ever gets built.

**Scope guarantee**: same modularity discipline as ADR 0001. All new
code lives under `src/pnfs/` and `src/nfs/` (which the SPDK path doesn't
import). The CSI integration shipped in Phase 5 (`docs/plans/pnfs-csi-
integration.md`) doesn't change shape.

---

## Phase A — CB_LAYOUTRECALL backchannel (Task #4) — ~2 weeks

### What it solves

Today, when a DS dies mid-write the kernel client keeps issuing WRITEs
to it. They time out, the application sees I/O errors, but **the MDS
has no way to tell the kernel "stop, that DS is gone, fall back to
MDS-direct or wait for new layout"**. The data being written during
the timeout window is lost. With CB_LAYOUTRECALL, the MDS can tell the
client to give back the layout, which then triggers a fresh LAYOUTGET
that excludes the dead DS.

This is also a precondition for FFL-mirrored layouts: re-mirroring
data after DS death requires revoking the old layout first.

### What we have

- `src/pnfs/mds/callback.rs::CallbackManager` — struct exists with the
  right shape (sessionid → callback channel info). Registers and
  unregisters callback channels.
- `encode_cb_layoutrecall_compound` — works. Builds the wire bytes.
- `send_callback_rpc` — **stub**. Logs "Would send" but returns Ok(())
  without sending. Core gap.
- `BIND_CONN_TO_SESSION` decoder + dispatcher arm (`dispatcher.rs:432`)
  — **partial**. We accept the op but don't actually mark the
  connection as carrying back-channel traffic.

### What needs to be built

Five concrete pieces, in roughly this order:

1. **Mark connections as back-channel-capable.** When a client sends
   `BIND_CONN_TO_SESSION` with `conn_dir = BACKCHANNEL` or `BOTH`, we
   need to remember that this TCP connection is the path back to the
   client. Today the `CallbackManager` registers an "address" string;
   in v4.1 the back-channel often runs on the **same** TCP connection
   as the forward channel (RFC 8881 §2.10.3.1). We need to store
   either (a) a handle to the existing connection's writer, or (b) a
   re-connect hint, depending on `conn_dir`.
   - **Effort**: ~3 days. Touches `server_v4.rs::handle_tcp_connection`
     to plumb a writer-handle into the dispatcher's session state.

2. **CB-side RPC encode/decode.** The callback channel uses the same
   ONC RPC framing as the forward channel, just with the program/vers
   the client advertised at `CREATE_SESSION` time (already captured in
   `cb_program`). Encode the call, send, await reply, parse status.
   - **Effort**: ~3 days. Mostly mechanical because we already have
     XDR + RPC encoders for the forward channel.

3. **Wire `send_callback_rpc` to the connection writer.** Replace the
   stub with: look up the writer for the session's back-channel
   connection, send the encoded compound, wait for reply with timeout
   (default 10s), surface errors as a typed `CallbackError`.
   - **Effort**: ~3 days. Includes retry policy (single retry on
     transient errors, fail-fast on protocol errors) and timeout
     handling.

4. **Hook DS-death detection into `recall_layouts_for_device`.** The
   `DeviceRegistry` already heartbeats DSes and marks them offline
   after 3 missed beats. Extend the heartbeat-timeout handler to call
   `LayoutManager::recall_layouts_for_device(dead_ds)`, which returns
   the affected stateids; for each one, fire CB_LAYOUTRECALL via the
   `CallbackManager`.
   - **Effort**: ~2 days. Requires plumbing the `CallbackManager` into
     the heartbeat loop's context.

5. **Layout revocation on recall timeout.** If the kernel doesn't
   `LAYOUTRETURN` within the deadline, the MDS forcibly revokes the
   layout (sets a tombstone, increments the layout stateid's seqid so
   subsequent client uses error with NFS4ERR_BAD_STATEID). RFC 5661
   §12.5.5.2 says we MAY revoke after sending CB_LAYOUTRECALL_LATER.
   - **Effort**: ~2 days.

### Verification

Three new tests, each catches a different failure mode:

- **Unit**: encode/decode round-trip for `CB_LAYOUTRECALL4args`. Cheap,
  catches wire-format regressions.
- **Integration** (in-process tonic-style mock client + real
  CallbackManager): `recall_layouts_for_device` issues exactly one
  CB_LAYOUTRECALL per affected layout, and on mock-client ack the
  layout is freed.
- **Lima e2e**: kill DS1 mid-write, verify the kernel client gets the
  recall (visible in `/proc/self/mountinfo` change or in the MDS log),
  and the in-flight `dd` either succeeds via MDS-direct fallback or
  fails cleanly with EIO — not silently corrupts.

### pynfs coverage map

The pynfs suite is the public NFSv4.1 conformance harness; it lives at
`/opt/pynfs` in the Lima VM and runs via `make test-nfs-protocol`. We
can't add tests to pynfs (it's external code) but we *can* enable
existing tests that today SKIP because the server doesn't yet support
the right behavior. Phase A unlocks one whole pynfs subgroup as a
side effect:

| pynfs group | Today | After Phase A.1–.5 |
|---|---|---|
| `st_delegation` (10 tests: DELEG1-9, DELEG23) | All SKIP — server never grants delegations, so the test framework can't exercise the back-channel from the client side | Currently still SKIP (requires *delegation grants* on top of CB; that's its own follow-up). The CB infrastructure built in Phase A is one of the two prerequisites. |
| pNFS `LAYOUTRECALL` flow | No public pynfs test exercises CB_LAYOUTRECALL against FILES layout (the existing pNFS pynfs tests hard-code block layout) | Same — pynfs has no FILES-layout recall test. **The verification path is the new Lima e2e test described below**, not pynfs. |

This is the honest part of "make sure pynfs has tests for it": **pynfs
covers Phase A only indirectly**. Direct verification needs:

1. **A new `make test-pnfs-recall` Lima script** (built in Phase A.4
   below). Stands up MDS+2DS, mounts, starts a `dd`, kills DS1, scans
   the MDS log for the recall message and asserts `/proc/self/mountinfo`
   re-acquires a fresh layout. This is the load-bearing integration
   test for Phase A.

2. **Per-piece unit tests** (already shipping with each sub-PR):
   - A.1: `nfs::v4::back_channel::tests` (3 tests, already in tree).
   - A.2: CB compound encode/decode round-trip.
   - A.3: send/receive round-trip against a mock client TCP listener.
   - A.4: `recall_layouts_for_device` fans out exactly N times for N
     affected layouts.
   - A.5: layout seqid increments on forced revocation.

3. **Re-running pynfs after every sub-PR** as a regression gate. Score
   must stay ≥ 153/18/91; regressions block the PR. This is what
   protects the conformance investment we already have.

If we later wire delegation *grants* (separate work, not in scope),
Phase A's CB infrastructure unblocks the 10 `st_delegation` pynfs tests
as a free side-effect. Worth tracking but not blocking.

### Done when

- `make test-pnfs-smoke` still green.
- New `make test-pnfs-recall` Lima script: starts MDS+2DS, mounts,
  starts a `dd`, kills DS1, asserts recall was issued and `dd` either
  completed or failed cleanly.
- 5 `st_delegation` tests in pynfs would all pass once delegations
  themselves are wired (free side-effect; not in scope here).

---

## Phase B — State persistence (Task #5) — ~1 week

### What it solves

Today the MDS holds **all** state in `DashMap`s in process memory. On
restart (pod roll, `kubectl rollout restart`, OOM kill, anything), the
state evaporates. Active clients see:

- `STALE_CLIENTID` on next op (their `client_id` was wiped).
- `BAD_STATEID` on any in-flight write (the OPEN/LAYOUT stateids the
  client is using are unknown to the new instance).
- `STALE_DEVICEID` on cached layouts (instance ID counter restarts).

A pod with a long-running pNFS PVC effectively has its mount destroyed
by an MDS restart. This is unacceptable for any production deployment.

### Design

A `StateBackend` trait with two implementations:

```rust
trait StateBackend: Send + Sync {
    async fn put_client(&self, c: &ClientRecord) -> Result<()>;
    async fn get_client(&self, id: ClientId) -> Result<Option<ClientRecord>>;
    async fn list_clients(&self) -> Result<Vec<ClientRecord>>;
    async fn delete_client(&self, id: ClientId) -> Result<()>;
    // ...analogous for Session, StateId, Layout, InstanceCounter.
}
```

- `MemoryBackend` — wraps the existing `DashMap`s. No-op persistence;
  same behaviour as today. Default for tests, Lima dev work, anyone
  who doesn't care about restart survival.
- `SqliteBackend` — single-file `*.sqlite` under a configured path.
  Atomic writes via `BEGIN/COMMIT`. Crash-safe; ships in production.

We pick SQLite over etcd because (a) the MDS is already a single
process and adding etcd is operational weight users won't want, (b)
SQLite gives us crash-safe atomic writes for free, (c) reproducing
issues is easier when the state is a file you can `sqlite3` into.

### What gets persisted

| Record | Why |
|---|---|
| `ClientRecord { client_id, verifier, principal, confirmed, cb_addr }` | Without this, EXCHANGE_ID after restart sees a "new" client; client gets STALE_CLIENTID and has to redo session establishment. |
| `SessionRecord { session_id, client_id, channel_attrs, cb_program }` | Same — sessions need to survive restart for clients to keep using their existing slots. |
| `StateIdRecord { stateid, owner, type, file_path, seqid }` | OPEN, LOCK, LAYOUT stateids. Without this, a client's in-flight WRITE after restart errors with BAD_STATEID. |
| `LayoutRecord { stateid, owner, segments, fsid, fh, range }` | The pNFS layout itself. Without it the kernel issues fresh LAYOUTGETs (fine but disruptive). |
| `InstanceCounter` | Monotonic, persisted, increments on each MDS start. New device IDs use a new prefix so old client caches see STALE_DEVICEID and re-fetch — better than silent identity collision. |

### What deliberately doesn't get persisted

- **Slot replay cache contents.** RFC 8881 §15.1.10.4 permits a server
  to lose these on restart; clients re-issue operations. Persisting
  every COMPOUND reply byte-for-byte would be expensive and isn't
  needed.
- **Per-connection state.** TCP connections drop and re-establish on
  restart anyway.
- **In-flight RPCs.** They time out client-side and get retried.

### Effort breakdown

- **Day 1**: trait definition + `MemoryBackend` impl that wraps the
  existing maps. Behaviour parity check: all existing pNFS tests pass
  unchanged.
- **Days 2-3**: `SqliteBackend` impl. Schema, prepared statements,
  test that writes survive a process restart.
- **Day 4**: integration — replace direct `DashMap` access in
  `ClientManager`, `SessionManager`, `StateIdManager`, `LayoutManager`
  with `StateBackend` calls. Either keep DashMap as in-memory cache
  with write-through, or just call backend on every op (probably the
  former for hot-path perf — measure with bench-sweep before deciding).
- **Day 5**: configuration. New `state` section in `mds.yaml`:
  ```yaml
  state:
    backend: sqlite              # memory | sqlite
    config:
      path: /var/lib/flint-pnfs/state.db
  ```

### Verification

- All existing pNFS unit tests pass under both backends (parameterized
  test rig).
- New Lima e2e: `make test-pnfs-restart` — start MDS, mount, write
  100 MiB, `kill -TERM` the MDS, restart it, verify the mount keeps
  working, no errors visible to the client.
- Bench-sweep numbers under SQLite shouldn't regress >10%. If they do,
  the in-memory cache layer is the answer.

### Done when

- Both backends pass the same test suite.
- MDS rolling restart during a Spark / fio workload doesn't fail tasks.
- `state.db` is small enough (~few MB for thousands of clients) to be
  trivially backed up.

---

## Phase C (optional, after demand) — FFL mirrored replication

Builds on A + B. Roughly the timeline from STATUS.md's "HDFS replication
factor 3" section, **substantially shorter** because A + B are no longer
on the critical path:

1. Re-enable FFLv4 layout encoding for mirrors. ~3 days.
2. `LayoutPolicy::MirroredStripe { factor: N }`. ~2 days.
3. Topology-aware DS selection. ~3 days.
4. Re-mirror coordinator (the new subsystem). ~2 weeks.

**Total Phase C**: ~3-4 weeks once A + B are in.

Don't start Phase C until a customer specifically asks for replication
*and* has signed off on the perf cost (every write goes Nx over the
network). Until then, "S3 spillover for ML datasets, pNFS perf for
write-heavy workloads, SPDK for OLTP" is the cleaner three-tier story.

---

## Total wall time

| Phase | Effort | What's unblocked |
|---|---:|---|
| A — CB_LAYOUTRECALL | ~2 weeks | DS-failure tolerance; `st_delegation` pynfs tests; FFL prereq |
| B — state persistence | ~1 week | MDS-restart tolerance; FFL prereq |
| **Production-ready pNFS** | **~3 weeks** | Ship to first customer |
| C — FFL replication | +3-4 weeks | HDFS-grade durability per-volume |

Phase A first — it's the bigger lift and the one that's gated several
other things (pynfs `st_delegation`, FFL layout revocation, ADR 0003's
production caveat). Then Phase B layered on top. Then evaluate Phase C
based on actual customer demand.

---

## What I'd do this week

Start Phase A item 1: plumb the connection writer into the dispatcher's
session state so `BindConnToSession` can register a real callback-
capable connection. Small, mechanical change that sets up the rest of
Phase A.
