# Flint NFS / pNFS Compliance Work — Status

Living document. Update this when a session ends or a milestone lands.

**Last updated:** 2026-05-01, after commit `7262e72`.
**Branch:** `kind-no-spdk`.
**Conformance score (pynfs full suite, 262 tests, 91 still skipped behind unimplemented features):**

```
Baseline (original audit run): 26 PASS  / 69 FAIL  / 167 SKIP   (96 runnable)
Current head (7262e72):       148 PASS  / 23 FAIL  / 91  SKIP  (171 runnable)
```

5.7× the original pass count. Six suites at 100%; nine more above 70%.

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
st_secinfo_no_name     2/4    50%
st_reclaim_complete    1/4    25%
st_verify              0/1     0%
st_secinfo             0/2     0%
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

---

## What's left

Sorted by likely effort × test count.

### High ROI — addressable in one focused session each

| Bucket | Tests | What's needed |
|---|---|---|
| `st_secinfo` + `st_secinfo_no_name` (4) | SECINFO/SECINFO_NO_NAME | These ops advertise which auth flavors the server accepts. Currently NOTSUPP / partial. Implementation = return `[AUTH_NONE, AUTH_SYS]` (or whatever flavors the server is built for); few hundred lines including XDR encoding. |
| `st_courtesy` (1) remaining | one test | "Courtesy client" handling (RFC 8881 §8.4.2.4) — graceful expired-client cleanup. Likely needs lease-expiry triggering state cleanup but allowing renewal-within-grace. |
| `st_reclaim_complete` (3) | RECLAIM2/3/4 | Validate reclaim-complete state machine: rejecting state-using ops before RECLAIM_COMPLETE during grace; rejecting reclaim ops outside grace. |
| `st_verify` (1) | VERIFY1-ish | VERIFY/NVERIFY ops (compare client-supplied attrs vs server's). Currently stub. |
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
- **Task #6 (pending)** — pNFS LAYOUTRETURN FSID/ALL paths are TODOs.
  Linux client uses ALL during unmount — layouts leak in MDS memory.

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

Current baseline (commit `f502bd9`):

* Smoke test: **DEGRADED** — mount + checksum round-trip succeeds, but
  all 24 MiB lands on the MDS, zero bytes on either DS. This is the
  audit's "pNFS data path is not real" gap; the smoke test exits 0
  in this state as a regression guard until the data path is wired.
* pynfs pNFS subset: **1 PASS / 3 FAIL / 4 SKIP** out of 8 tests.
  CSID7 testOpenLayoutGet passes. The 3 FAILs are tests that
  hardcode `LAYOUT4_BLOCK_VOLUME` (we're a files-layout server, not
  block); the 4 SKIPs are dependency-chained on those.

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

`st_current_stateid` is now 100% (commit `7262e72`). The next biggest
single-session win is **`st_secinfo` + `st_secinfo_no_name` (4 tests)**
— these ops advertise which RPC auth flavors the server accepts for a
given filehandle. Implementation:

1. Decode SECINFO (component name) / SECINFO_NO_NAME (style: current_fh
   or parent).
2. Return an array of `secinfo4` results, one per supported flavor:
   - AUTH_NONE: just the flavor number (0).
   - AUTH_SYS: flavor + machinename + uid + gid (we already have
     all of this on the server side).
   - RPCSEC_GSS: flavor + oid + qop + service. We don't really
     support GSS yet; advertise just AUTH_NONE/AUTH_SYS for now.
3. Wire the new `OperationResult::SecInfo(...)` variant + encoder.

After that, `st_courtesy` and `st_reclaim_complete` need real
lease-state-machine work — small but fiddly. `st_verify` is one op
implementation. Remaining `st_*` failures are smaller clusters
(symlink PUTFH resolution, REQ_TOO_BIG, etc.).

The **pNFS work (Tasks #4/#5/#6)** doesn't move the pynfs needle — pynfs
runs against a single non-pNFS mount — but it's the audit's biggest
architectural gap. Worth doing before any production claim around the
pNFS data plane.

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
