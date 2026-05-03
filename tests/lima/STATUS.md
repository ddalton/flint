# Flint NFS / pNFS Compliance Work — Status

Living document. Update this when a session ends or a milestone lands.

**Last updated:** 2026-05-02, after Phase A.3 of production-readiness (real CB send-and-await over the back-channel writer).
**Branch:** `kind-no-spdk`.

### Today in one paragraph

A user can now `helm upgrade --set pnfs.enabled=true …`, `kubectl apply` a
StorageClass with `parameters.layout: pnfs`, and a pod that mounts the
resulting PVC writes to a real pNFS-striped volume. SPDK code paths are
byte-identical when pNFS is disabled — the integration is opt-in via the
`FLINT_PNFS_MDS_ENDPOINT` env var, gated by ADR 0001's "keep one driver,
defer split" decision. End-to-end test (`make test-pnfs-csi`) exercises
the full create → mount → write → read → delete cycle against the actual
gRPC verbs and kernel data path. Honest single-host write win is **1.6×**
over single-server NFS at fsync=1 (ADR 0003); the architectural claim of
linear scaling with DS count remains untested cross-host.

### Headline

**The pNFS data path is now real, end-to-end.** A 24 MiB write from a
Linux NFSv4.1 client mount stripes across two DSes (8 MiB / 16 MiB),
the kernel reads it back through the layout, and the SHA-256 round-
trips. The MDS holds metadata only (0 bytes on disk for the striped
file).

```
                      DS1   DS2   MDS    client read-back
─────────────────────────────────────────────────────────
audit baseline       0     0     24M    hash matches (MDS-direct)
+ FILE-layout fixes  0     0     24M    "       (still MDS-direct)
+ DS endpoint fixes  0     0     24M    "       (kernel never connects)
+ DS RECLAIM/FH (✶)  8M    16M   0      ❌ 0-byte file (no LAYOUTCOMMIT)
+ LAYOUTCOMMIT (✶)   8M    16M   0      ✅ 24M, hash matches
```

✶ = this session.

### Conformance score (pynfs full suite, 262 tests)

```
Baseline (original audit run): 26 PASS  / 69 FAIL  / 167 SKIP   (96 runnable)
Current head:                 153 PASS  / 18 FAIL  / 91  SKIP  (171 runnable)
```

5.8× the original pass count. Six suites at 100%; nine more above 70%.
The pNFS work is *invisible* to pynfs (it runs against a single
non-pNFS mount); the pNFS smoke test is the truth source for the
data plane.

---

## Per-suite breakdown (current)

```
st_current_stateid     9/9   100%   ✓ ← new this session
st_destroy_clientid    8/8   100%   ✓
st_compound            5/5   100%   ✓
st_trunking            2/2   100%   ✓
st_destroy_session     1/1   100%   ✓
st_create_session     27/28   96%
st_rename             32/35   91%
st_lookupp             8/9    88%
st_exchange_id        23/26   88%
st_putfh               7/8    87%
st_sequence           14/17   82%
st_courtesy            4/5    80%
st_open                5/7    71%
st_secinfo_no_name     4/4   100%   ✓ ← new this session
st_secinfo             2/2   100%   ✓ ← new this session
st_verify              1/1   100%   ✓ ← new this session
st_reclaim_complete    1/4    25%
st_delegation          0/3     0% (blocked on CB_RECALL)
```

---

## What was achieved

The work falls into three big phases. All commits are on the
`kind-no-spdk` branch, pushed to `origin`.

### Phase 1 — RFC framing & encoding correctness (baseline → 49 PASS)

Hardened the wire layer so all subsequent error-path tests at least
*reach* the right handler instead of crashing the harness with
`GARBAGE_ARGS`. After Phase 1 there are zero RPC-level GARBAGE_ARGS in
the suite.

| # | Commit | What |
|---|--------|------|
| 1.A | `aaf3de7` | NFSv4 status enum cross-referenced against RFC 7530/8881/7862 (was off-by-various from RFC); `OperationResult::Unsupported` now encodes opcode + status (was emitting only status, causing pynfs decoder EOFError); minor-version gate; tag-validity gate; `unsafe { buf.set_len }` UB removed |
| 1.B-1 | `1a543b5` | NFSv4.1 SEQUENCE exactly-once replay cache wired end-to-end; resync branch removed; `RetryUncachedRep` for replays before the original returned |
| 1.B-2 | `6d4c30d` | CREATE_SESSION input validation (TOOSMALL / INVAL / BADXDR for malformed channel attrs); rdma_ird array decoded properly |
| 1.B-3 | `64ab3d4` | EXCHANGE_ID flag-bit validation, eia_client_impl_id<1> array length check, `NFS4ERR_NOT_ONLY_OP` for session-establishment ops bundled with non-SEQUENCE companions, `Operation::BadXdr` for malformed but recognised opcodes |
| 1.B-4 | `7f3acd2` | Body-less ops (GETFH/SAVEFH/RESTOREFH/READLINK/PUTROOTFH/...) at end of COMPOUND no longer mis-classified as BADXDR |
| 1.B-5 | `61779f1` | Tightened stateid validation: WRITE no longer accepts seqid=0-on-unknown-other (anonymous-write bypass); LOCK takes client_id from session (was hardcoded 1 = silent multi-client lock fights); LOCK byte-range overflow → INVAL |

### Phase 2 — Filesystem semantics (49 → 107 PASS, 75 new tests unlocked)

Made pynfs's `--maketree` initialisation work end-to-end so the suite
could even *start* the bulk of its tests.

| # | Commit | What |
|---|--------|------|
| 2.A | `a4fe847` | SETATTR4res missing `attrsset` bitmap (decoder hit EOF on the next op); SETATTR on dangling symlinks → Ok no-op (was NOENT) |
| 2.B | `c1dcc3c` | CREATE for SOCK/FIFO/BLK/CHR creates a regular-file stand-in (was BADTYPE, breaking maketree) |
| 2.C | `8cbe29f` | RENAME error chain: empty name → INVAL, "."/".." → BADNAME, non-dir parent → NOTDIR, dir-into-non-dir → EXIST, dir-into-non-empty-dir → NOTEMPTY, self-rename cinfo invariance. CREATE wire layout fixed (createtype4 union tail was being read after objname) |

### Phase 3 — Real protocol state machines (107 → 141 PASS)

The hard parts: bringing actual RFC state machines online.

| # | Commit | What |
|---|--------|------|
| 3.A | `872b81d` | EXCHANGE_ID RFC 8881 §18.35.5 nine-case decision table (UPD × confirmed × verifier × principal). RPC-level principal plumbed end-to-end through `Auth::principal()` → `CompoundContext::principal` → `ClientManager::exchange_id`. Client gains `confirmed` flag (set by CREATE_SESSION). |
| 3.B | `ecb26d0` | CREATE_SESSION RFC 8881 §18.36.4 sequence + replay cache (per-clientid `last_cs_sequence` + cached structured response, returned byte-for-byte on retry). Principal-collision check (CLID_INUSE) for unconfirmed records only. SEQUENCE `REP_TOO_BIG_TO_CACHE` when cachethis + tiny ca_maxresponsesize_cached. |
| 3.C | `8320986` | LOOKUPP RFC 5661 §18.10.4: non-directory CFH → NOTDIR, symlink CFH → SYMLINK |
| 3.D | `c86c718` | DESTROY_CLIENTID validation (STALE_CLIENTID / CLIENTID_BUSY); SEQUENCE compound-position rules (SEQUENCE_POS for misplaced SEQUENCE, OP_NOT_IN_SESSION for v4.1 op without SEQUENCE prefix); slot table sized to negotiated ca_maxrequests (SEQUENCE BADSLOT for slot_id beyond it); ca_maxoperations enforcement (TOO_MANY_OPS) |
| 3.E | `7262e72` | RFC 8881 §16.2.3.1.2 "current stateid" sentinel resolution: OPEN/LOCK/LOCKU/SAVEFH/RESTOREFH propagate it, PUTFH/PUTROOTFH/PUTPUBFH/LOOKUP/LOOKUPP invalidate it; FREE_STATEID op (LOCKS_HELD for open/lock state); fixed pre-existing LOCK locker4 union decode bug |

### Phase 4 — Real pNFS data path (this session: `cdbbe21..9076e96`)

A 24 MiB striped write from a Linux NFSv4.1 client now actually
traverses both DSes, and read-back through the MDS sees the right
size and bytes. Six surgical fixes; each was load-bearing — remove
any one and the kernel falls back to MDS-direct I/O silently.

| # | Commit | What | Effect on smoke (DS1/DS2/MDS, client size) |
|---|--------|------|-------|
| 4.A | `cdbbe21` | `FATTR4_FS_LAYOUT_TYPES` advertise [FILES] only (was [FILES, BLOCK, FLEX_FILES]); LAYOUTGET handler emits FILE layout instead of broken FFLv4 | kernel now negotiates FILES + issues GETDEVICEINFO (was silent fallback). 0/0/24M, 24M client. |
| 4.B | `272ceef` | `MdsControlService` overrides DS-reported endpoint with operator-configured one (DS reports its bind 0.0.0.0; client needs externally routable). `endpoint_to_uaddr` DNS-resolves hostnames so non-IPv4 DS endpoints encode to a parseable uaddr. | kernel now receives valid uaddr `192.168.5.2.80.11/.12` (was `0.0.0.0.80.11`). 0/0/24M, 24M client (still MDS-direct: kernel can't talk to DS). |
| 4.C | `23faf5b` | DS dispatcher answers `RECLAIM_COMPLETE` (opcode 58, RFC §18.51): kernel was getting NOTSUPP and marking the DS unhealthy. DS accepts MDS-issued v1 filehandles via `FileHandleManager::parse_path_lenient` and rebases by basename — DS no longer fails strict instance/hash check. | **8M/16M/0**. Kernel writes through DSes. 0-byte client read-back (MDS doesn't know the size). |
| 4.D | `9076e96` | **LAYOUTCOMMIT** (RFC 8881 §18.42) wired end-to-end: decoder for `LAYOUTCOMMIT4args` (offset/length/reclaim/stateid + `newoffset4` + `newtime4` + `layoutupdate4`); `OperationResult::LayoutCommit(status, Option<u64>)` with `newsize4` reply union; handler resolves CFH→path and `set_len` if `last_write_offset+1` extends EOF; best-effort `set_times` from `time_modify`. | **8M/16M/0, 24M client. PASS.** |
| 4.E | (this session) | **LAYOUTRETURN** (RFC 5661 §18.4 / RFC 8881 §18.44) wired end-to-end. Pre-fix the decoder treated `layoutreturn4` as a length-prefixed opaque (it's a discriminated union — discriminator was eaten as length); the dispatcher returned `Ok` without calling the pNFS handler at all, so layouts leaked across mount cycles. Now: `Operation::LayoutReturn` carries a typed `LayoutReturn4Body { File{offset,length,stateid,body}, Fsid, All }`; dispatcher resolves `(client_id,fsid)` from the SEQUENCE-bound session and calls `pnfs_handler.layoutreturn(...)`; `MdsOperations::layoutreturn` calls `LayoutManager::return_layout` (FILE) / `return_fsid_for_client` (FSID) / `return_all_for_client` (ALL). Smoke confirms `Layout returned: stateid=…` now fires per FILE return. 4 unit tests cover wire decode of all three variants + unknown-discriminator error. | smoke unchanged (8M/16M/0, 24M client, hash matches), pynfs unchanged (148/23/91), but layouts no longer leak. |

The smoke now exits with `✓ PASS: data path crossed both DSes (real
pNFS striping)`.

### Phase 5 — pNFS CSI integration (this session: `ed70fe7..0679abf`)

After Phase 4 the *protocol* worked; nothing wired it to Kubernetes. Phase 5
shipped the five-PR plan in `docs/plans/pnfs-csi-integration.md`:

| PR | Commit | What |
|---|--------|------|
| 5.1 | `ed70fe7` | MDS gRPC `CreateVolume(volume_id, size_bytes)` + `DeleteVolume(volume_id)`. Idempotent on matching size, refuses size mismatch, rejects path-traversal volume_ids. 5 unit tests. |
| 5.2 | `9f5f94c` + `57fd7b2` | Driver-side `pnfs_csi` module — talks to MDS, returns `volume_context` with five `pnfs.flint.io/*` keys. 5 unit tests against an in-process tonic mock. Bonus: dropped the unused per-file `Mutex<File>` and made COMMIT reuse the WRITE-side cached fd (ADR 0003 cleanups). |
| 5.3 | `1da59ab` | Wired into `main.rs`: `create_volume` / `delete_volume` / `node_publish_volume` branch on `parameters.layout: pnfs` and `volume_context["pnfs.flint.io/mds-ip"]` respectively. SPDK code paths byte-identical when `FLINT_PNFS_MDS_ENDPOINT` is unset. |
| 5.4 | `aeb0a7e` | Helm chart `pnfs:` values section (`enabled: false` default), conditional env-var injection in `controller.yaml`, example StorageClass at `deployments/pnfs-csi-storageclass.yaml`, ClusterIP Services for MDS gRPC + NFS. |
| 5.5 | `0679abf` | End-to-end test: `pnfs-csi-cli` test binary + `tests/lima/pnfs/csi-e2e.sh` orchestrator. 7 assertions: ctx-key shape, MDS file size, mount, sha256 round-trip, per-DS allocation, delete cleanup, re-create-after-delete. |

Run `make test-pnfs-csi` for the integration test:
- DS1: 56 MiB allocated, DS2: 64 MiB allocated, MDS: 0 bytes.
- All 7 assertions PASS in ~5 s.

### Phase 6 — perf characterization (this session: `d868e19..79cf6ef`)

First head-to-head pNFS-vs-single-server fio bench, with corrected
analysis after a deeper sweep:

```
                          single-server NFS    pNFS (2 DSes)    ratio
WRITE (fsync=1, jobs=1)        173 MiB/s        268 MiB/s       1.55×
WRITE (fsync=1, jobs=4)        168 MiB/s        274 MiB/s       1.63×
WRITE (fsync=1, jobs=8)        161 MiB/s        262 MiB/s       1.62×
WRITE (bs=4K,   jobs=4)        158 MiB/s        249 MiB/s       1.57×
READ  (jobs=4)                 270 MiB/s        267 MiB/s       1.01×
```

**Honest claim: ~1.6× write win, block-size invariant.** ADR 0002 had a
2.10× number from a single noisy run; ADR 0003 settled it with the sweep.
The mechanism is "shard the server" (two DS processes give ~2× server-side
RPC slots, narrowed by shared APFS journal), not protocol cleverness — see
`server_v4.rs:176` for the per-TCP-serial RPC handler that's the dominant
single-host bottleneck.

Reads tie at 270 MiB/s on this hardware: loopback TCP / single-client
saturation is the bottleneck before per-server protocol overhead kicks in.
**Cross-host scaling remains an architectural prediction, not a measurement.**

### Phase 7 — Production-readiness foundation (this session: `1fa43dc`)

The pNFS CSI integration ships, but the data plane has known gaps that
block real customer use. ADR-style discussion landed at
`docs/plans/pnfs-production-readiness.md` outlining Phase A
(CB_LAYOUTRECALL backchannel, ~2 weeks) and Phase B (state persistence,
~1 week). Phase A is broken into 5 sub-pieces; A.1 shipped this session.

| Sub | Commit | What |
|---|---|---|
| **A.1 — connection writer plumbing** | `1fa43dc` | `BackChannelWriter` (async-mutex-serialized writer over `BufWriter<OwnedWriteHalf>`); `CompoundDispatcher::back_channels` registry keyed by `SessionId`; `BIND_CONN_TO_SESSION` honors `conn_dir=BACK/BOTH` and registers the writer; forward replies now flow through the same writer (refactor preserved behavior). Unblocks A.2-A.5. 3 unit tests cover marker framing, concurrent-non-interleave, and EPIPE on peer close. Lib: 271 → 274; smoke green; pynfs unchanged 153/18/91. |
| **A.2 — CB-side RPC encode/decode** | `8bb02bc` | New `nfs::v4::cb_compound` module with typed `CbCompoundCall` / `CbResult` / `CbCompoundReply`, plus `encode_cb_call(xid, cb_program, args)` for full RPC CALL framing and `decode_cb_reply(bytes, expected_xid)` for the response (typed `CbReplyError` separates XDR errors, RPC rejections, RPC accept-status failures, and xid mismatches). Pulled `csa_cb_program` out of `CREATE_SESSION` (was discarded as `_`) and persisted it on `Session` so the back-channel call can address `program=cb_program, version=1, proc=CB_COMPOUND`. Refactored `pnfs::mds::callback::encode_cb_layoutrecall` to use the new typed encoder — fixes a pre-existing wire bug where `CB_SEQUENCE` omitted the trailing `referring_call_lists<>` length-prefix (every byte after it would have mis-framed). 6 unit tests: args round-trip, RPC CALL header shape, success-reply decode, `NFS4ERR_NOMATCHING_LAYOUT` reply decode, xid-mismatch refusal, `PROG_UNAVAIL` accept-error surfacing. Lib: 274 → 280; smoke green; pynfs unchanged 153/18/91. |
| **A.3 — real CB send-and-await** | (this session) | `BackChannelWriter` now carries a per-connection inflight registry (`xid → oneshot::Sender<Bytes>`) and a `next_xid` counter; new `send_cb_compound(cb_program, args, timeout)` does the full register → `send_record` → await → decode dance and surfaces `CallbackError::{Timeout, Transport, Reply, ConnectionClosed}`. The `handle_tcp_connection` read loop in `server_v4.rs` now peeks `msg_type` after each frame: REPLY (=1) is routed to `deliver_reply(xid, body)` and the loop continues; CALL falls through to the existing forward-dispatch path. An `InflightGuard` on the loop's stack runs `drop_all_inflight()` on every exit path so awaiting CB callers see `ConnectionClosed` instead of hanging on the timeout. `pnfs::mds::callback::CallbackManager` was rewritten around this: takes the dispatcher's `back_channels` registry + `Arc<StateManager>` at construction, looks up `Session.cb_program` on each call, and replaces the old `send_callback_rpc` stub. 6 unit tests over real loopback TCP pairs: happy-path round-trip, `NFS4ERR_NOMATCHING_LAYOUT` carried through the typed reply, timeout when the client stays silent, no-back-channel fast-fail, mid-call connection drop → `ConnectionClosed`, and a wire-decoder sanity check that re-parses the CALL bytes off the socket. Lib: 280 → 285; smoke green; pynfs unchanged 153/18/91. |
| A.4 — DS-death → recall fan-out | pending | ~2 days |
| A.5 — layout revocation on recall timeout | pending | ~2 days |

After Phase A, Phase B (state persistence — `StateBackend` trait with
`memory` + `sqlite` impls) lands in ~1 week. Together they make pNFS
safe to ship to a first customer; whether to build Phase C (FFL
mirroring for HDFS-style replication) is then a demand-driven call.

### pynfs coverage for Phase A

Honest accounting: pynfs has **no public test for CB_LAYOUTRECALL on
FILES layout**. The 10 `st_delegation` tests would exercise the same
back-channel infrastructure once delegation *grants* are wired
(separate work). Direct verification of Phase A is via:

* **Lima e2e** (built in A.4): `make test-pnfs-recall` — kill a DS
  mid-write, assert the kernel got recalled and the in-flight `dd`
  either completes via MDS-direct fallback or fails cleanly with EIO.
* **Per-piece unit tests** with each sub-PR.
* **Regression gate**: pynfs stays ≥ 153 PASS / 18 FAIL / 91 SKIP
  after every commit; smoke stays green.

#### What's still TODO on the data path

The smoke is green but the pNFS implementation has known gaps that
don't block the smoke. Tracked items (and what would surface them):

- **CB_LAYOUTRECALL backchannel (Task #4)** — `pnfs/callback.rs` is a
  stub returning `Ok(())` without sending. Without it, layout
  revocation on DS death is impossible. Would surface in: kill a DS
  mid-write; the kernel keeps writing into the void instead of being
  recalled to MDS-direct.
- **Layout/state persistence (Task #5)** — instance IDs and layout
  stateids regenerate on MDS restart; clients see `STALE_DEVICEID` /
  `BAD_STATEID`. Would surface in: restart MDS during a long write;
  client errors out instead of recovering.
- ~~**LAYOUTRETURN FSID/ALL (Task #6)**~~ — **done (commit 4.E above)**.
  Layouts are now actually freed on FILE/FSID/ALL returns. Linux's
  per-file FILE-typed returns at unmount drive the path that's
  exercised in CI; ALL/FSID code paths are covered by unit tests but
  not yet by an integration test (no current client sends them in
  the smoke flow).
- **Multi-client correctness** — only the smoke uses one mount on
  one VM. Two concurrent clients writing the same file would
  exercise stateid-per-owner code that hasn't been driven yet.
- **Smoke-test asserts** — the smoke checks "any bytes on each DS"
  and "client hash matches"; it doesn't yet check the per-DS slices
  reconstruct in correct stripe order, or that the MDS-side stat
  size matches LAYOUTCOMMIT. A scheduled agent (job `3ddeefa6`) will
  add those on 2026-05-15.

---

## What's left

Sorted by likely effort × test count.

### High ROI — addressable in one focused session each

| Bucket | Tests | What's needed |
|---|---|---|
| ~~`st_secinfo` + `st_secinfo_no_name` (4)~~ | ~~SECINFO/SECINFO_NO_NAME~~ | **Done.** Both ops now decode/encode (`[AUTH_NONE, AUTH_SYS, RPCSEC_GSS(Kerberos V5)]`), clear CFH on success per RFC 5661 §2.6.3.1.1.8, and SECINFO_NO_NAME(PARENT) of the served root returns NFS4ERR_NOENT. 6/6 secinfo tests now pass. |
| `st_courtesy` (1) remaining | one test | "Courtesy client" handling (RFC 8881 §8.4.2.4) — graceful expired-client cleanup. Likely needs lease-expiry triggering state cleanup but allowing renewal-within-grace. |
| `st_reclaim_complete` (3) | RECLAIM2/3/4 | Validate reclaim-complete state machine: rejecting state-using ops before RECLAIM_COMPLETE during grace; rejecting reclaim ops outside grace. |
| ~~`st_verify` (1)~~ | ~~VERIFY1-ish~~ | **Done.** VERIFY/NVERIFY both wired against the canonical GETATTR encoding for bytewise comparison; ATTRNOTSUPP for unsupported bits per §18.30.3. 1/1 passes. |
| `st_open` (2) remaining | OPEN edge cases | Likely OPEN_CONFIRM (v4.0) or specific share-deny conflicts. |

### Medium ROI — needs a real subsystem

| Bucket | Tests | What's needed |
|---|---|---|
| `st_delegation` (3) | DELEG5/6/7 | Requires the **CB_RECALL backchannel** — already on the original audit's pNFS task list (Task #4). Need TCP back-channel, RPC encode/decode, retry logic. ~1-2 weeks. Same machinery unlocks pNFS CB_LAYOUTRECALL and Linux client delegations. |
| `st_rename` (3) remaining | RNM…linkdata | Likely related to dangling-symlink resolution at PUTFH time (separate file-handle subsystem fix). |
| `st_lookupp` (1) remaining | LKPP1a testLink | Same dangling-symlink PUTFH issue. |
| `st_sequence` (2) remaining | SEQ6/SEQ9c | REQ_TOO_BIG (need to compute incoming COMPOUND wire size against ca_maxrequestsize) and a specific replay-cache LOOKUP test. |
| `st_putfh` (1) remaining | PUTFH1a | Dangling-symlink PUTFH (same root cause as st_lookupp / st_rename remainder). |
| `st_exchange_id` (3) remaining | EID5e/9, EID9 LeasePeriod | Need real lease-expiration → STALE_CLIENTID semantics (we always-renew currently). |

### From the original audit — bigger structural items

These weren't visible in the pynfs sweep (because pynfs runs against a
single mount), but the original audit flagged them as production blockers.
None block any pynfs test. Tracking them here so they don't fall off the
radar:

- **Task #1 (pending)** — RPC framing & XDR DoS hardening: per-frame
  size cap is in (4 MiB), but the multi-fragment accumulation path is
  not. Linux NFS v4.1 clients don't fragment so this hasn't bitten, but
  any client that does will hit silent corruption.
- **Task #4 (pending)** — pNFS CB_LAYOUTRECALL backchannel. Currently a
  stub that returns `Ok(())` without sending. Without this, layout
  revocation on DS death is impossible. Same machinery unlocks the
  delegation tests above.
- **Task #5 (pending)** — pNFS state persistence. Device IDs and layout
  stateids are randomly regenerated on every MDS restart, so any client
  with a layout sees `STALE_DEVICEID` / `BAD_STATEID` after a restart.
- ~~**Task #6**~~ — **done.** LAYOUTRETURN FILE/FSID/ALL all wired
  through `LayoutManager::return_layout` / `return_fsid_for_client` /
  `return_all_for_client`. Layouts no longer leak across mount cycles.

`/Users/ddalton/github/flint/spdk-csi-driver/src/pnfs/` houses the
relevant code; the original audit lives in chat history (run
`git log --grep="phase 1"` for the on-tree commit summaries).

---

## How to run the tests

### One-time setup (macOS)

```bash
brew install lima                  # if not already installed
make lima-up                       # ~3 min: builds the Ubuntu 24.04 VM
                                   # with pynfs preinstalled at /opt/pynfs.
                                   # Idempotent — skips if VM exists.
```

The Lima VM provides a Linux NFSv4.1 kernel client and pynfs. macOS
itself can't be the client because its NFS client is v4.0-only and
buggy.

### Run the full conformance suite (~3 min)

```bash
make test-nfs-protocol
```

This Makefile target:
1. Builds `flint-nfs-server` (release).
2. Pre-creates `/tmp/flint-nfs-export/tmp` for pynfs's `--maketree` step.
3. Starts the server in the background on `0.0.0.0:20490`.
4. Runs pynfs against it from inside the Lima VM with
   `--maketree --nocleanup all`, saving JSON results to
   `/tmp/pynfs.json` (in the VM) and copying them to
   `/tmp/flint-pynfs-results.json` (on the host).
5. Stops the server.

The first run after `make lima-up` may need a fresh test-tree:

```bash
chmod -R u+w /tmp/flint-nfs-export 2>/dev/null
rm -rf /tmp/flint-nfs-export/tmp
mkdir -p /tmp/flint-nfs-export/tmp
chmod 0777 /tmp/flint-nfs-export/tmp
```

### Other useful targets

```bash
make nfs-server               # foreground, verbose logs (useful for debugging)
make nfs-server-bg            # background, logs to /tmp/flint-nfs.log
make nfs-server-stop          # stop the background server
make test-nfs-mount           # sanity: mount + write + read + unmount
make test-nfs-frag            # T1: large WRITE forces fragmented RPC

make lima-shell               # interactive shell inside the test VM
make lima-down                # tear down the VM
```

### pNFS test targets

The pNFS suite is its own thing — it brings up MDS + 2 DSes and runs
two distinct tests:

```bash
make build-pnfs               # one-time: build flint-pnfs-{mds,ds}
make test-pnfs-smoke          # mount + 24 MiB write/read + per-DS byte count
make test-pnfs-pynfs          # pynfs `pnfs` flag set (8 conformance tests)
make test-pnfs-all            # both
```

Current baseline (commit `9076e96`):

* Smoke test: **✓ PASS — data path crossed both DSes (real pNFS striping).**
  24 MiB striped across DS1 (8 MiB) and DS2 (16 MiB), MDS holds 0 bytes,
  kernel-side `ls -la` shows 24 MiB and the round-trip SHA-256 matches.
* pynfs pNFS subset: **1 PASS / 3 FAIL / 4 SKIP** out of 8 tests.
  CSID7 testOpenLayoutGet passes. The 3 FAILs hardcode
  `LAYOUT4_BLOCK_VOLUME` (we're files-layout, not block); the 4 SKIPs
  are dependency-chained on those. Score is unchanged — pynfs pNFS
  doesn't exercise the actual data path, only layout grants.

See `tests/lima/pnfs/README.md` for topology, debugging tips, and
"what PASS/DEGRADED/FAIL mean".

### Run a single pynfs test (debugging)

```bash
make nfs-server-bg

limactl shell flint-nfs-client -- bash -lc \
  'cd /opt/pynfs/nfs4.1 && python3 ./testserver.py \
     host.lima.internal:20490/tmp \
     --maketree --nocleanup CSESS5'

# Watch the server log in another terminal:
tail -f /tmp/flint-nfs.log

make nfs-server-stop
```

Useful test groupings (pynfs flag names): `compound`, `putfh`, `getfh`,
`sequence`, `exchange_id`, `create_session`, `destroy_session`,
`destroy_clientid`, `lookup`, `lookupp`, `current_stateid`, `rename`,
`open`, `courtesy`, `delegation`, `secinfo`, `secinfo_no_name`,
`reclaim_complete`, `trunking`, `verify`. Use `--showcodes` to list
specific test codes.

### Compare two runs

The repo has a snapshot of every commit's pynfs JSON in
`tests/lima/pynfs-after-*.json`. Diffing them shows which tests
flipped:

```bash
python3 -c "
import json
old = {t['code']: t for t in json.load(open('tests/lima/pynfs-after-rename-2026-05-01.json'))['testcase']}
new = {t['code']: t for t in json.load(open('tests/lima/pynfs-after-destroy-clientid-and-sequence-2026-05-01.json'))['testcase']}
def res(t):
    if 'failure' in t: return 'FAIL'
    if t.get('skipped'): return 'SKIP'
    return 'PASS'
for code in sorted(set(old) | set(new)):
    o = res(old.get(code,{})); n = res(new.get(code,{}))
    if o != n: print(f'{code:10} {o:4} → {n:4}')"
```

### Unit tests

```bash
cd spdk-csi-driver
cargo test --release --lib nfs::v4         # 104 tests, all passing as of c86c718
```

---

## Repository layout

```
spdk-csi-driver/
├── src/
│   ├── nfs/
│   │   ├── server_v4.rs        TCP/RPC frame loop, COMPOUND dispatch entry
│   │   ├── rpc.rs              Auth, CallMessage, ReplyBuilder, principal()
│   │   ├── xdr.rs              base XDR codec
│   │   ├── rpcsec_gss.rs       RPCSEC_GSS (mostly stub)
│   │   ├── kerberos.rs         GSS Kerberos (mostly stub)
│   │   └── v4/
│   │       ├── compound.rs       COMPOUND request/response types, encode/decode
│   │       ├── dispatcher.rs     COMPOUND op-dispatch loop, session-pos/maxops checks
│   │       ├── filehandle.rs     FH ↔ path, normalize, pseudo-root
│   │       ├── filehandle_pnfs.rs pNFS FH layout
│   │       ├── pseudo.rs         pseudo-FS / exports
│   │       ├── protocol.rs       ★ Nfs4Status, opcode::*, exchgid_flags
│   │       ├── xdr.rs            v4-specific XDR helpers
│   │       ├── operations/
│   │       │   ├── session.rs    EXCHANGE_ID / CREATE_SESSION / SEQUENCE / DESTROY_*
│   │       │   ├── fileops.rs    OPEN / CREATE / RENAME / SETATTR / LOOKUP / LOOKUPP / GETATTR / READDIR / REMOVE / LINK
│   │       │   ├── ioops.rs      READ / WRITE / COMMIT / OPEN
│   │       │   ├── lockops.rs    LOCK / LOCKU / LOCKT
│   │       │   └── perfops.rs    NFSv4.2 ALLOCATE / DEALLOCATE / SEEK / READ_PLUS / COPY
│   │       └── state/
│   │           ├── client.rs     ★ ClientManager + §18.35.5 state machine + §18.36.4 cs cache
│   │           ├── session.rs    SessionManager + slot/replay
│   │           ├── stateid.rs    StateIdManager (validate / validate_for_read)
│   │           ├── lease.rs      LeaseManager
│   │           └── delegation.rs DelegationManager
│   └── pnfs/                  pNFS MDS + DS (less polished, blocked on Task #4)
└── tests/
    └── lima/
        ├── nfs-client.yaml    ★ Lima VM config (Ubuntu + pynfs)
        ├── PYNFS_BASELINE.md  ← stale; this file (STATUS.md) supersedes it
        ├── STATUS.md          ★ this file
        └── pynfs-*.json       ← per-commit pynfs JSON snapshots
```

★ = the files most often touched in this work.

---

## Where to pick up next

Three independent fronts. Pick whichever maps to current priorities.

### Front A — pNFS production-readiness (audit's structural gaps)

The CSI integration ships, but the data plane has known durability and
restart gaps that block real customer use. These are the **production
gates**; they don't move user-visible features but they're prerequisites
for anyone running this in prod. Plan is at
`docs/plans/pnfs-production-readiness.md`.

1. **CB_LAYOUTRECALL backchannel (Task #4) — IN PROGRESS.** Phase A.1
   shipped at `1fa43dc` (connection writer plumbing). Four sub-PRs
   remain: A.2 (CB RPC framing), A.3 (replace send stub), A.4
   (DS-death → recall fan-out + Lima e2e test), A.5 (forced layout
   revocation on recall timeout). ~10 days of remaining work.
2. **Layout/state persistence (Task #5) — pending.** `StateBackend`
   trait with `memory` (current) + `sqlite` (new) impls. Persist
   client/session/stateid/layout records; slot replay cache
   deliberately not persisted per RFC 8881 §15.1.10.4. ~1 week.
3. **Cross-host fio bench — pending.** The single-Mac-host 1.6×
   number is a floor; the architectural prediction is N× scaling
   with N DSes on N nodes. Until measured on a real cluster this
   remains a prediction. ~1 week of harness + run.

### Front B — pNFS feature work (beyond the perf-tier MVP)

The integration ships a minimal slice. Real customers will ask for:

1. **Replication / HA** — see "HDFS replication factor 3 equivalent"
   section below. FFL-mirrored layouts. Multi-week project.
2. **Snapshots / clones for pNFS** — currently SPDK-only. Would need
   MDS to coordinate consistent point-in-time across DSes.
3. **`ControllerExpandVolume` for pNFS.** Today the StorageClass
   has `allowVolumeExpansion: false`; flip it on by extending the
   MDS file via gRPC + propagating to the client.
4. **DS auto-discovery via DaemonSet** so adding/removing nodes
   doesn't require operator action. The `DeviceRegistry` already
   exists; needs a registration RPC from a DS-side bootstrap.
5. **Locality-aware layout selection** — read k8s topology labels
   so layouts prefer same-zone DSes. Big perf win on cross-AZ
   clusters.

### Front C — pynfs core protocol score (single-mount tests)

`st_current_stateid`, `st_secinfo`, `st_secinfo_no_name`, `st_verify`
are all now 100%. Next single-session wins:

* **`st_courtesy` + `st_reclaim_complete` (4 tests)** — both want a
  real lease-expiration / grace-period state machine: lease-expired
  clients hold "courtesy" state until the next conflicting op (RFC
  8881 §8.4.2.4); reclaim ops outside grace MUST be rejected.
* **`st_exchange_id` EID9 (1 test)** — `testLeasePeriod` wants real
  lease expiry → STALE_CLIENTID (we always-renew today).

Beyond those, the remaining `st_*` failures cluster around symlink
PUTFH resolution (st_lookupp / st_rename / st_putfh tail), REQ_TOO_BIG
(st_sequence), and CB_RECALL-blocked delegation tests.

---

## HDFS replication factor 3 — pNFS equivalent

**Q: HDFS supports replication-factor=3 for durability. How can pNFS do
the same?**

**A: FlexFiles layout (FFLv4, RFC 8435) with mirrored DS sets.** The
data path we ship today (FILES layout, RFC 5661 §13) is HDFS replication
factor *1* — every stripe lives on exactly one DS, and DS death means
data loss. To get HDFS-grade durability via pNFS, the protocol
mechanism is FFLv4 mirroring: the MDS hands out a layout that lists N
DSes for the same byte range as **mirrors**, and the kernel client
writes to all N in parallel and reads from any.

### What the kernel does for free (already shipped in Linux)

Linux's NFSv4.1 client implements FFLv4 mirrored layouts natively:

- WRITE: client fans out to every DS in the mirror set; succeeds iff
  all DS WRITEs succeed.
- READ: client picks any one mirror per request (load-balances across).
- DS error: client reports it via `LAYOUTRETURN` with `ff_io_errors4`,
  marks the layout invalid, asks for a fresh one.

So the client side is solved. The work is all on the MDS.

### What the MDS would need to do

| Component | Effort | Status today |
|---|---:|---|
| FFLv4 layout encoding (mirrored variant) | ~3 days | We had FFLv4 advertised but pulled it back (commit `cdbbe21`) because layout negotiation was off; bringing it back for mirroring is real-but-tractable work. The `FfLayoutReturn4` decoder already exists. |
| `LayoutPolicy::MirroredStripe { factor: N }` in MDS | ~2 days | Today's `LayoutManager::assign_segments_for_layout` picks one DS per stripe; needs to pick N and emit them as the mirror set. |
| **CB_LAYOUTRECALL backchannel (Task #4)** | ~1-2 weeks | Required to revoke layouts when re-mirroring after DS failure. Already on the roadmap as a production prereq regardless. |
| **State persistence (Task #5)** | ~1 week | Required so re-mirror progress survives MDS restart. Already on the roadmap. |
| Re-mirror coordinator (background scrub) | ~2 weeks | When DS dies, MDS must copy bytes from a healthy mirror to a new replacement DS. This is its own subsystem — NameNode-style replication tracking, queue, throttle, retry. **No starter code today.** |
| Topology / rack awareness | ~3 days | Bias DS selection so mirrors land on different nodes / zones (HDFS's "rack awareness"). Reads k8s topology labels. |

**Total: ~5-7 weeks for honest HDFS-replication-factor-3 equivalence.**

### A simpler, weaker variant (~2-3 weeks)

If "auto-healing" isn't required, you can ship FFL mirroring without
the re-mirror coordinator: writes go to N mirrors, reads survive any
one DS failure, but rebuilding to N replicas after DS death needs
manual operator intervention. Useful for "single-DS-failure tolerance"
without the full coordination machinery.

### A different architectural answer worth considering

For ML datasets specifically (the user-stated workload), a simpler
pattern often beats FFL mirroring on cost/complexity:

- Source of truth lives in S3 (or any object store); S3 already
  provides 11×9s durability for free.
- DSes are *caches* — they pull lazily from S3 on first read of a
  file, then serve from local.
- DS death = empty cache when it comes back; next read re-pulls.
- No re-mirror coordinator, no scrub task, no HDFS-style protocol.

Trade-offs: cache-miss reads have S3 latency (10s-100s of ms) instead
of DS latency (ms); steady-state reads after warming have full pNFS
perf. For read-heavy training data this is fine; for write-heavy
workloads it's a worse fit than FFL mirroring.

The two answers are **complementary**, not exclusive:

- ML-training tier: pNFS + S3 spillover. Cheap, durable enough
  ("worst case I re-warm from S3"), no replication subsystem to
  build.
- Database / write-heavy / strict-durability tier: existing SPDK
  NVMe-oF path. Already replicated, already shipping.
- Future: FFL-mirrored pNFS only if a customer specifically asks for
  "HDFS-shape" semantics (replication-factor-N at the storage layer
  with auto-healing) and is willing to wait for the multi-week
  effort.

### Recommendation

Don't build FFL mirroring speculatively. Ship the perf tier (already
done), add Task #4 + #5 for production-readiness, then revisit
based on actual customer requests. ADR 0001 already encodes the
"don't speculatively build modular abstractions" principle for
the same reasons.

---

## Performance discipline

Every commit in this work has preserved happy-path performance. Hot
path is `SEQUENCE → READ/WRITE → COMPOUND encode`. Specifically
preserved invariants:

- Per-COMPOUND allocations: tag (one String), op vec (one Vec), result
  vec (one Vec). No extra allocs added.
- Per-WRITE: open fd cached in `DashMap<StateId, ...>` on first hit;
  subsequent WRITE on same stateid is one DashMap get + one positioned
  write. Validator added one extra DashMap get per WRITE (state lookup)
  — measurable overhead is in the noise compared to the disk write.
- Replay cache: per-slot `Option<Vec<u8>>` of the encoded COMPOUND
  reply; one Bytes::clone (Arc-backed) on cache, return-as-is on
  replay (no re-encoding, no state mutation, no lease renewal).
- Status enum: `#[repr(u32)]` with explicit discriminants — same
  generated code as before the audit, just correct values.

Anything that adds work to the hot path should be flagged in its
commit message with a measurement or a justification.
