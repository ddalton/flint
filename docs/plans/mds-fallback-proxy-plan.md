# MDS fallback-I/O proxy — closing the straggler-EIO hole (F66)

Status: DESIGNED 2026-08-02, not yet implemented.
Gate: `make test-pnfs-fsx` (seed 42 — currently RED at HEAD, fails in
seconds; this plan is done when it is green and stays green).

## 1. The defect this closes

A pNFS client that holds no layout is entitled to route I/O through the
MDS — RFC 8881 treats the metadata server as a full NFS server for the
file, and the Linux files-layout client uses that entitlement routinely.
flint's MDS structurally cannot serve striped-file data (it holds a
sparse stub), and the DELAY-livelock fix made the healthy-fleet answer a
fast `NFS4ERR_IO` on the theory that fallback I/O against a healthy fleet
means a client stuck in its MDS-fallback trap, where only a fatal error
springs it.

That theory has a counterexample we shipped ourselves, found by fsx and
diagnosed on 2026-08-02 (see `project_runaz_cluster` memory and the
`cebb139`-era commits):

    fsx truncates → client RETURNS its layouts before the size SETATTR
      (PNFS_LAYOUTRET_ON_SETATTR — standard files-layout behavior)
    → MDS truncates, recalls (client: NoMatchingLayout), parks, clears
      in ~1 ms
    → ONE writeback page, queued to the MDS during the brief no-layout
      window, arrives as a STRAGGLER — the debug repro shows the
      client's LAYOUTGET ReadWrite SUCCEEDING 200 µs BEFORE the refused
      WRITE
    → healthy fleet ⇒ FailFast NFS4ERR_IO ⇒ msync(2) returns EIO to the
      application.

The client is healthy and layout-holding; the EIO is not a spring, it is
data-path failure surfaced to userspace by our own policy. First-bad era
is the v1.23.0 F65/truncate wave (v1.22.0 passes, v1.23.0 fails,
verified back-to-back on one rig); it shipped unseen because the lima
gates were passing on stale data at the time (`04b46d1`).

## 2. Why the cheap fixes are refuted — read this before "simplifying"

- **Delay (with or without a post-recall grace window).** An MDS-bound
  WRITE retried under `NFS4ERR_DELAY` is re-sent to the MDS forever; the
  kernel never re-drives an in-flight MDS write through a layout. Delay
  defers the EIO or livelocks — the exact livelock the FailFast arm was
  built to break. Terminal answers only.
- **Serve (write the stub).** The stub then disagrees with the stripes.
  This is the runn 2026-07-06 corruption ("client immediately read the
  stub — wrong data, no error") that `stub_io_disposition` exists to
  prevent. Never.
- **Don't recall/park on truncate.** Reopens F65, whose closure took a
  TLA+ module and nine defects. The truncate machinery is correct; its
  *interaction* with the fallback policy is the bug.
- **Fix the client.** The straggler scheduling is kernel behavior,
  legal per RFC. Not ours to change, and every deployed kernel does it.

What remains is making fallback I/O actually work: the MDS applies it to
the stripes itself. This also retires flint's standing deviation from
RFC 8881's expectation that MDS I/O works — the deviation this bug is a
symptom of.

## 3. Design

### 3.1 The shape

Two new RPCs on the existing `DsControl` service (the token-authed gRPC
listener every DS already runs for `TruncateStripeFile`):

```proto
service DsControl {
  rpc TruncateStripeFile(...) returns (...);          // existing
  rpc ReadStripe(ReadStripeRequest) returns (ReadStripeResponse);
  rpc WriteStripe(WriteStripeRequest) returns (WriteStripeResponse);
}

message ReadStripeRequest {
  string device_id = 1;   // identity guard, same as truncate
  string rel_path  = 2;   // stripe file, DS-data-dir relative
  uint64 offset    = 3;   // FILE offset — stripe files are
  uint32 count     = 4;   //   sparse-addressed (see 3.2)
}
message ReadStripeResponse {
  bool ok = 1; string message = 2;
  bytes data = 3;         // may be SHORT: hole/absent-file semantics
}                         //   are the MDS's to resolve (3.4)

message WriteStripeRequest {
  string device_id = 1; string rel_path = 2;
  uint64 offset = 3; bytes data = 4;
}
message WriteStripeResponse { bool ok = 1; string message = 2; }
```

The MDS side is a small `fallback_proxy` module on
`PnfsOperationHandler`, reusing the exact plumbing `truncate_fanout`
already proved: the `ds_control_clients` DashMap (dial-once, evict on
transport error), `device_registry` → `control_endpoint` resolution, the
2 s dial / 3 s RPC timeouts, and `placement.stripe_rel_path(slot)` /
`legacy_stripe_rel_path` for the two pin generations.

### 3.2 Striping arithmetic — sparse, so it is range-splitting only

Stripe files are **sparse-addressed**: a stripe file's offsets are FILE
offsets, holes where other slots own the bytes. This is already a
load-bearing invariant — `truncate_fanout` passes the SAME `new_size` to
every slot's `set_len`, which is only correct under sparse addressing.
The proxy therefore never repacks anything:

```
for each chunk of [offset, offset+len) split at stripe_size boundaries:
    u    = chunk.start / stripe_size          // stripe unit index
    slot = unit_slot(placement, u)            // the SAME slot mapping
                                              // layoutget encodes —
                                              // factor it out of
                                              // layout.rs and call the
                                              // one function from both
    rel  = placement.stripe_rel_path(slot)    // (or legacy_rel)
    ReadStripe/WriteStripe(device_id[slot], rel, chunk.start, ...)
```

IMPLEMENTATION NOTE: the slot mapping (`(u + first_stripe_index) % len`,
with `first_stripe_index` derived per file) MUST NOT be re-implemented in
the proxy. Extract the existing mapping from the LAYOUTGET encode path
into one shared function with a unit test asserting proxy and layout
agree for asymmetric cases (width 3, first_stripe_index ≠ 0). Two copies
of that formula is how stripe corruption happens.

Fallback RPCs are client-sized (≤ ~1 MiB by `SERVER_MAX_REQUEST`), so a
proxied op touches at most `len/stripe_size + 1` chunks — with the 8 MiB
default stripe unit, almost always exactly one. Chunks are issued
sequentially; this is a correctness path for stragglers, not a data
plane, and simplicity wins.

### 3.3 WRITE semantics — always durable, never UNSTABLE

The DS-side `WriteStripe` handler opens the stripe file (creating it if
absent — a fallback write can precede any DS-path write), `write_all_at`
the payload at the FILE offset, then `fdatasync`. The MDS's NFS WRITE
reply claims `FILE_SYNC`, and that claim is honest HERE, unlike on the
DS data path (see the DATA_SYNC comment in `pnfs/ds/server.rs`): data is
durable on the DS before we reply, and the size authority is the MDS
itself, which updates it in the same operation (3.5). No verifier
bookkeeping, no COMMIT re-drive: a client never needs to COMMIT what was
FILE_SYNC.

Fallback COMMIT (a client committing UNSTABLE data it wrote through the
MDS): cannot happen for data written through this proxy (nothing is
UNSTABLE), but the op must still answer for mixed histories — return the
MDS's own write verifier over the (durable) state, which is a no-op.

### 3.4 READ semantics — the MDS resolves holes, the DS reports them

`ReadStripe` returns exactly what `read_at` produced, short reads
included, and `ok=true` with empty data for an ABSENT stripe file (the
same explicit-hole semantics `IoOperationHandler::read` already has for
the DS data path, io.rs:205-221). The MDS then applies what it alone
knows — the authoritative stub size:

```
n     = bytes returned for the chunk
want  = min(chunk.len, size.saturating_sub(chunk.start))
if n < want: zero-fill n..want        // hole in a sparse stripe file
eof   = offset + returned >= size
```

The DS must NOT zero-fill or infer EOF: it does not know the file size
(that is the whole point of the split MDS/DS design), and guessing is
how the tar --sparse class of bugs happened.

### 3.5 Size, times, and the stub

A proxied WRITE that extends past the stub's current size `set_len`s the
stub to `offset + len` in the same dispatch, exactly what LAYOUTCOMMIT
would have done after a DS-path write. Without this, stat() serves the
stale stub size — the same bug class the DS WRITE arm's FILE_SYNC
comment records (filelayout_set_layoutcommit skipping). mtime/ctime
follow from the stub touch. The stub remains sparse; `space_used`
correctness is preserved by the existing st_blocks handling.

### 3.6 The disposition ladder, amended

`fallback_io_disposition_bounded` currently: parked → Delay (ceiling) →
FailFast; healthy fleet → FailFast; outage → Delay under ceiling →
FailFast. The amendment touches ONLY the healthy arm:

```
parked (truncate-dirty)      → Delay under ceiling, then FailFast   (unchanged)
every pinned DS reachable    → PROXY                                (was FailFast)
proxy chunk fails transient  → Delay under ceiling, then FailFast   (new)
DS outage                    → Delay under ceiling, then FailFast   (unchanged)
```

The park still wins — a parked file's fallback I/O keeps waiting for
the truncate confirmation, because a proxied WRITE racing an
unconfirmed stripe truncation could resurrect bytes past the new EOF.
Order of checks in code: park first, proxy second. The FailFast arm
does not disappear; it becomes the floor under the proxy instead of the
first answer.

### 3.7 Security and containment

`WriteStripe`/`ReadStripe` get the same guards as `TruncateStripeFile`,
verbatim: token auth (the existing `AuthedDsControlClient` /
`FLINT_PNFS_CONTROL_TOKEN` interceptor), `device_id` identity match
(refuse foreign devices — serving DS-B's volume as DS-A corrupts
silently), and rel-path containment (no traversal, no absolute paths,
non-empty). The truncate handler's tests are the template; every one of
them is replicated for both new RPCs.

### 3.8 Kill switch

`FLINT_MDS_FALLBACK_PROXY` (default ON). OFF restores today's FailFast
behavior verbatim — one env var, checked once at the disposition site.
The default is ON because the OFF behavior is the bug; the switch exists
because this path writes data via a new channel and an operator
diagnosing corruption must be able to remove it from the suspect list
in one restart.

## 4. What this deliberately does not do

- **No streaming, no parallel chunk fan-out, no proxy read-ahead.** The
  traffic is stragglers; per-op latency is irrelevant next to
  correctness and reviewability.
- **No proxying while parked.** The truncate gate's reason to exist is
  exactly that window.
- **No attempt to make the MDS a general data server.** LAYOUTGET
  remains the fast path; the proxy is the escape hatch that makes the
  slow path lawful.

## 5. Test plan

1. **Unit — slot mapping:** proxy and LAYOUTGET agree on
   (offset → slot, rel_path) across widths 1/2/3/5, first_stripe_index
   0 and nonzero, legacy and v2 pins. One shared function, one table
   test.
2. **Unit — hole resolution:** short/absent stripe reads zero-fill to
   stub size, eof exactly at size, no zero-fill past size.
3. **Unit — disposition ladder:** parked beats proxy; proxy failure
   degrades to Delay-then-FailFast; kill switch restores FailFast.
4. **DS handler tests:** identity refusal, traversal refusal,
   create-on-write, fdatasync-before-ok (the truncate suite's shape).
5. **The gate:** `make test-pnfs-fsx` — RED today at seed 42 within ~25
   ops, GREEN after, run ≥3 consecutive times (the failure is
   deterministic, but the fix must also survive fsstress's truncate
   storm which runs in the same drill).
6. **Regression floor:** full `cargo test --lib`, `make
   test-pnfs-smoke`, pynfs 171/171, the lima 4.2 suite with
   `--minorversion 2`, and `test-pnfs-fallback` (the DELAY-livelock
   drill — the proxy must not have broken the dead-fleet escalation it
   sits on top of).

## 6. Sizing

proto + regen ~half day; DS handlers + tests ~1 day; MDS proxy module +
disposition amendment + tests ~1.5 days; gate runs and the inevitable
surprise ~1 day. Call it 4 days of careful work. The fsx repro loop is
seconds, which is the difference between this being a slog and a joy.
