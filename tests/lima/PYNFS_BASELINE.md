# pynfs baseline run — 2026-05-01

First end-to-end NFSv4.1 conformance run against `flint-nfs-server` from a
Linux kernel client (Lima Ubuntu 24.04 → kernel `nfs4` mount → `host.lima.internal:20490`).

Results: `262 tests, 26 PASS, 69 FAIL, 167 SKIP`.

The 167 skipped tests need test infra (lookup tree, set-up state, etc.) that
`--noinit` skipped — they should be re-run with init enabled once the
init-time crashes (PUTFH GARBAGE_ARGS) are fixed.

## What passes
- Empty COMPOUND (`COMP1`)
- Standard tag (`COMP2`)
- Basic EXCHANGE_ID happy path (`EID1`, `EID1b`, `EID2`, `EID5a`, `EID5b`, `EID5h`, `EID6h`)
- Basic CREATE_SESSION happy path (`CSESS1`, `CSESS3`, `CSESS16`, `CSESS17`, `CSESS18`,
  `CSESS22`, `CSESS24`, `CSESS200`, etc.)
- pNFS layout discovery happy path (`PUTFH9` succeeded — root FH).

## What fails — by suite

| Suite              | Fail | Pattern |
|--------------------|------|---------|
| `st_exchange_id`   | 20   | Returns `NFS4_OK` for nearly every error case (bad flags, missing record updates, conflicting principals, long arrays). |
| `st_create_session`| 17   | TOOSMALL/INVAL/BADXDR/SEQ_MISORDERED all swallowed as OK; replay results don't match (DRC broken). |
| `st_sequence`      | 12   | TOOMANYOPS / REQ_TOO_BIG / replay-cache mismatches. |
| `st_putfh`         | 7    | Every PUTFH variant returns `GARBAGE_ARGS` — the server is mangling its reply for non-trivial filehandles. |
| `st_destroy_session`|6    | DESTROY_SESSION returns `OK` instead of `CONN_NOT_BOUND_TO_SESSION`; structural GARBAGE_ARGS on most variants. |
| `st_compound`      | 4    | `MINOR_VERS_MISMATCH` not enforced; unknown opcode (`COMP5`) emits malformed XDR. |
| `st_delegation`    | 3    | CB-related encoding broken (callback-channel construction bug). |

## Failure categories (all 69)

1. **Wrong status code** (~45 tests) — the server returns `NFS4_OK` instead of
   the specific `NFS4ERR_*` the RFC mandates. Pure error-path correctness work.

2. **`GARBAGE_ARGS` from RPC layer** (~18 tests) — pynfs' XDR decode rejects
   the *server's reply*. Maps to:
   - PUTFH echoing back malformed FH bytes.
   - DESTROY_SESSION reply encoding bug.
   - Compound-tag handling for non-UTF8 tags.

3. **`xdrlib.Error: value=10072 not in enum nfs_opnum4`** (~3 tests) — server
   echoes an unknown opcode straight into the reply instead of substituting
   `OP_ILLEGAL` / `NFS4ERR_OP_ILLEGAL`. Direct violation of RFC 5661 §15.2.

4. **Replay cache mismatches** (`CSESS5/5a/5b`, `SEQ9c-e`, `SEQ10b`) —
   replayed COMPOUNDs don't return the cached reply. Confirms the
   `// TODO: Return cached response from slot` finding from the audit
   (`nfs/v4/operations/session.rs:402`).

## How to reproduce

```bash
make lima-up                                       # one-time
make nfs-server-bg
limactl shell flint-nfs-client -- bash -lc \
  'cd /opt/pynfs/nfs4.1 && python3 ./testserver.py \
     host.lima.internal:20490/ \
     --json=/tmp/pynfs.json --noinit --nocleanup \
     compound putfh getfh sequence exchange_id create_session \
     destroy_session lookup'
limactl cp flint-nfs-client:/tmp/pynfs.json /tmp/flint-pynfs-results.json
make nfs-server-stop
```

## Next runs to do

1. Fix the GARBAGE_ARGS class first (those crash the harness during `--init`,
   blocking the 167 skipped tests).
2. Re-run **with** `--init` and `--cleanup` to actually exercise the lookup/
   open/read/write/lock/access suites — those are the ones that map to the
   audit's stateid and lock findings (T6, T7, T8 from the test plan).
3. Add `all` flag for a full sweep once Phase 1 fixes are in.

---

## Phase 1.A — fixes landed (results: `pynfs-after-phase1a-2026-05-01.json`)

| Metric | Baseline | After 1.A |
|---|---|---|
| PASS | 26 | **30** |
| FAIL | 69 | **65** |
| SKIP | 167 | 167 |
| `GARBAGE_ARGS` (RPC-layer crashes) | 18 | **0** |
| Tests now reporting wrong-but-truthful status | many | all |

Specific tests flipped to PASS:
- `COMP4a testInvalidMinor` — minor-version gate added in dispatcher.
- `COMP4b testInvalidMinor2` — gate fires before op-array decode.
- `COMP5 testUndefined` — `OperationResult::Unsupported` now emits the opcode
  before the status, with `OP_ILLEGAL` substitution at the dispatcher.
- `SEQ9e testReplayCache005` — lenient operation-decoder lets the test reach
  the SEQUENCE handler.

Behind the scenes (no new pass, but no longer crashes the harness):
- **18 GARBAGE_ARGS failures eliminated.** Tag UTF-8 validation moved out of
  XDR decode, so non-UTF-8 tags now produce `NFS4ERR_INVAL` per RFC instead
  of breaking RPC framing.
- **All 100+ NFS4 status codes corrected against RFC 7530 §13 / RFC 8881 §15.**
  The internal enum had `NotSupp = 10072` (wire collision with HASH_ALG_UNSUPP),
  the entire 10049+ block was shifted, and `Nfs4Status::OpIllegal` was the
  only top-of-COMPOUND status the broken table happened to encode correctly.
- **`unsafe { buf.set_len(length) }` removed from `server_v4.rs`** — replaced
  with `resize`, eliminating the UB risk on any short read.

Files changed:
- `spdk-csi-driver/src/nfs/server_v4.rs` — drop UB on RPC fragment read.
- `spdk-csi-driver/src/nfs/v4/protocol.rs` — full status-enum / opcode audit.
- `spdk-csi-driver/src/nfs/v4/compound.rs` — lenient tag/op decode, encode the
  op discriminant for `Unsupported` results.
- `spdk-csi-driver/src/nfs/v4/dispatcher.rs` — minor-version gate, tag-validity
  gate, choose `OP_ILLEGAL` vs `NOTSUPP` based on the request opcode.

What still needs work (next runs are still blocked here):
- `pynfs --maketree` requires functioning OPEN/CREATE, which currently returns
  `NFS4ERR_NOTSUPP`. That's the gate for the 167 skipped tests, and the path
  the audit's stateid / locking / I/O findings live behind.
- COMP3 still fails: `\xef\xbf\xbe` is technically valid UTF-8 to Rust's
  `from_utf8` (it's the noncharacter U+FFFE). RFC says servers MUST detect
  noncharacters and return INVAL. Need an explicit codepoint-range check.
- 17 EXCHANGE_ID tests still fail at the semantics layer (Task #7).
- 17 CREATE_SESSION input-validation tests still fail (Task #9).
- 12 SEQUENCE tests still fail — replay cache TODO unfixed (Task #2).
