# NFS proxy — step 0 census (2026-09-27)

Step 0 of `flint-lite-nfs-proxy-design.md` §8 has two halves:
- **(a)** every place a hub mints a stateid `other`, for H2;
- **(b)** the Linux client's COMPOUND shapes against a pseudo-root with
  two exports, for §3's one-target rule.

Evidence is in `tests/lima/nfs-proxy-census/`: the capture script, one
pcap per scenario, and `compounds.txt` with the op lists and statuses.

## (a) Stateid minting — H2

Three arms, so a single spelling cannot hide a site:
- `let mut other =` / `other: [` literals;
- `StateId {` struct literals with a non-literal `other`;
- `StateId::new(` callers.

Each hit was placed inside or outside the file's `#[cfg(test)]` region.

**Client-visible stateids come from exactly two functions**, and both
draw from one counter (`StateIdManager::next_stateid`):

| Site | Layout | Persisted |
|---|---|---|
| `allocate` (`nfs/v4/state/stateid.rs:693`), used for open and lock stateids | `[0..8]` counter, `[8..12]` `client_id as u32` | yes; the counter is restored as the max of `other[0..8]` (`stateid.rs:656-670`) |
| `allocate_delegation` (`stateid.rs:746`) | `[0..8]` counter, `[8..12]` `client_id ^ boot epoch` | no, by design |

Everything else is not a client-visible mint:
- **Internal lock-table keys:** `0xFC 'l' 'k'` plus a counter
  (`operations/lockops.rs:472`). These are row identities that never
  reach the wire.
- **The breaker marker row:** `BREAKER_MARKER_TAG = 0xFC` plus a
  timestamp (`state/mod.rs:242`). This is a database row.
- **Special stateids** (all-zero, all-ones, the current stateid): these
  always travel with a filehandle.
- **Stateid rewrites:** seqid bumps (`lockops.rs:1217,1412`,
  `ioops.rs:2135`) keep `other` unchanged.
- The remaining hits are test code or wire decoding (`xdr.rs:117`).

**`other[8..12]` is written and never read in production.** Nothing
parses the client id back out of it. That decides H2's shape:

- **T1: tag in the counter's top 32 bits.** Rejected. The counter drops
  to 32 bits and is persisted across restarts, so it wraps after 2³²
  mints. At 1,000 opens/s, which a build-heavy agent can reach, that
  is about 50 days, and the wrap reuses values that long-lived
  stateids may still hold.
- **T2: tag in `other[8..12]`, replacing `client_id as u32`.** Chosen.
  The counter keeps 64 bits, and `other[0]` stays 0 for any realistic
  count, so a tagged stateid can never look like the `0xFC` internal
  keys. **The cost:** the delegation mint's anti-reuse epoch lives in
  `[8..12]` today and must move, for example into `[0..4]`, since
  delegation ids are never restored. Delegations are off in lite, but
  the property must survive the move.
- **The tag is ASSIGNED, not hashed.** The operator allocates a unique
  u32 per share and records it in the share's status, and the hub reads
  it from env. A hash over 10,000–20,000 hubs gives certain collisions
  somewhere. An assigned tag gives none, and the proxy maps tag → hub
  from the FlintShare, not by learning it.

**Stateid-carrying ops with no filehandle.** The hub decodes these, and
they are the only ones the current FH cannot route:
- **`TEST_STATEID` and `FREE_STATEID`** — these are why H2 exists;
- **`LAYOUTRETURN` FSID/ALL and `DELEGPURGE`** — FH-less, but they
  carry no live state in lite (no layouts, delegations off). The proxy
  answers them itself.

## (b) Linux client COMPOUND shapes

**Why knfsd and not a flint hub:** the flint hub serves exactly one
export, so it cannot present the pseudo-root-with-two-exports shape
this half exists to capture. knfsd can. **The cost:** any shape that
depends on what the *server* advertises (xattrs, below) is knfsd's,
not flint's. Only the crossing shapes transfer. Within-workspace shapes
must be re-captured against a real hub.

Client and server were Linux 6.8.0 (Ubuntu 24.04), in the
`flint-nfs-client` Lima VM, over loopback. knfsd exported a pseudo-root
(`fsid=0`, `crossmnt`) with `ws-a` and `ws-b` as **separate
filesystems** (tmpfs, `fsid=11/12`). Mount options:
`nfsvers=4.2,proto=tcp,hard`.

**Control, and a rig defect caught by it.** The first run exported
`ws-a` and `ws-b` as plain subdirectories of the root export's
filesystem. The client then saw **one device for all three paths**,
and a cross-workspace `rename()` **went over the wire and succeeded**.
Such a rig can say nothing about crossing. The rerun, with separate
filesystems, shows devices 57/61/62. The first run doubles as the
control: it shows the capture sees a cross-workspace RENAME when one
is sent.

| # | Scenario | What the wire shows | Consequence for the proxy |
|---|---|---|---|
| s1 | mount `/ws-a` (the PV shape) | `EXCHANGE_ID`, `CREATE_SESSION`, `RECLAIM_COMPLETE`; `[SEQ, PUTROOTFH, SECINFO_NO_NAME]`; `[SEQ, PUTROOTFH, GETFH, GETATTR]`; then **`[SEQ, PUTFH(root), LOOKUP ws-a, GETFH, GETATTR]`** | **The crossing is never `PUTROOTFH, LOOKUP` in one compound.** It is a `PUTFH` of the pseudo-root's handle followed by `LOOKUP`. §3's rule is generalised to "LOOKUP while the current FH is the pseudo-root", however it got there. |
| s2 | a second mount, `/ws-b`, same client | **No `EXCHANGE_ID`, no `CREATE_SESSION`**; it reuses the session | Confirms §1: every workspace from one node shares one session, so routing must be per COMPOUND. |
| s3 | mount `/`, `ls -la` | `GETATTR`, `ACCESS`, **`LISTXATTRS`** and `READDIR` on the pseudo-root | The proxy answers all four on its root. `LISTXATTRS` gets `NFS4ERR_NOTSUPP`. |
| s1–s4 | ordinary `ls -la` inside a workspace | **`[SEQ, PUTFH, LISTXATTRS]` (op 74)** on every listing | **Server-dependent — likely a knfsd artifact.** The client sends xattr ops only to a server that advertises `xattr_support`. knfsd does; the flint hub has no `xattr_support` attribute at all (0 hits in `spdk-csi-driver/src`). So against a flint hub the client should not send op 74. **Re-capture against the real hub.** The proxy's pseudo-root must not advertise it either, or the proxy invites ops no hub can serve. |
| s4 | `ls` into `ws-a` and `ws-b` from the `/` mount | `[SEQ, PUTFH(root), LOOKUP, GETFH, GETATTR]`, then plain `[SEQ, PUTFH, GETATTR]` on the new handle | Same crossing shape as s1. |
| s5 | rename within `ws-a` | `[SEQ, PUTFH, SAVEFH, PUTFH, RENAME]` | Both handles are on one hub: routable. |
| s6 | rename `ws-a` → `ws-b` | **Nothing on the wire.** `rename()` fails locally with `EXDEV` | **H1 is load-bearing.** The client refuses the cross-workspace rename only because the fsids differ. The proxy's `XDEV` arm is a backstop. |
| s7 | `..` from a subdirectory after `drop_caches` | **No `LOOKUPP`** | `LOOKUPP` at a hub root is rare. The proxy handles it anyway. |
| s8 | open + `lockf` just after a server restart | `OPEN` = `[SEQ, PUTFH, OPEN, ACCESS, GETATTR]`; `LOCK` returned `NFS4ERR_GRACE` 5 times, then succeeded | The proxy passes `GRACE` through; the client retries. |
| s10 | server restart with an open file | `SEQUENCE` → `BADSESSION`; `DESTROY_SESSION` → `BADSESSION`; `CREATE_SESSION` → `STALE_CLIENTID`; `EXCHANGE_ID`; `CREATE_SESSION`; `OPEN CLAIM_PREVIOUS`; **`[SEQ, RECLAIM_COMPLETE]` with no FH** | This is the reclaim shape the proxy must never force on a client (§4). `RECLAIM_COMPLETE` is a client-level op that the proxy terminates. |

**Inconclusive, and why:**
- **s9 (admin revocation).** `unlock_filesystem` on the 6.8 server
  returned 0 but set **no** `SEQ4_STATUS_ADMIN_STATE_REVOKED`, so the
  client never ran revocation recovery. Revoking NFSv4 state through
  `unlock_filesystem` needs a newer knfsd. **Design §9's first open
  question is still open.**
- **s10's reclaim `PUTFH`** returned `NFS4ERR_STALE`, because the
  tmpfs exports did not keep their handles across the server restart.
  That is a rig artifact. The Python holder's later `EBADF` comes from
  it, not from client behaviour.
- **No `TEST_STATEID` or `FREE_STATEID` appeared in any scenario.**
  They are still reachable (they are the RFC's revocation-recovery
  path), so H2 still stands.

## Changes made to the design

- **§3 crossing:** the rule is keyed on "current FH is the
  pseudo-root", not on a `PUTROOTFH, LOOKUP` pair.
- **§3:** the pseudo-root does not advertise `xattr_support`, matching
  the hubs. (`LISTXATTRS` was a knfsd artifact; see Part 2.)
- **§5 H2:** T2 layout, tag assigned by the operator, and the
  delegation epoch moved.
- **§2:** the persistence claim is measured, with defect D2 as the
  exception (Part 2).
- **§4:** the hibernate row no longer uses
  `SEQ4_STATUS_ADMIN_STATE_REVOKED` (Part 3).
- **§7:** a new cost — one client for every workspace on a node.
- **§8:** hibernation requires zero leases; D1 and D2 are fixed first.
- **§9:** answered.

---

## Part 2 — against a REAL flint hub (box, Linux 6.12)

This part exists because Part 1 used knfsd. A flint lite hub
(`flint-pnfs-mds`, `mode: standalone`, config as
`lite_operator::render::mds_yaml` renders it, `FLINT_FH_KERNEL=1`,
`FLINT_NFS_ENFORCE_PERMISSIONS=1`, no tier) was built at `2edbb51` on
the build box. The kernel client ran on the same host. Script:
`tests/lima/nfs-proxy-census/capture-flint-hub.sh`. Results:
`results-box-6.12-flint-hub/` (`restart-0` is the control arm,
`restart-1` the restart arm, each with the hub log).

| # | Scenario | Result |
|---|---|---|
| h1 | mount | Same shape as knfsd: `EXCHANGE_ID`, `CREATE_SESSION`, `RECLAIM_COMPLETE`, `[PUTROOTFH, SECINFO_NO_NAME]`, `[PUTROOTFH, GETFH, GETATTR]`. |
| h2 | create, write, `ls -la` | `CREATE`, `OPEN`+`GETFH`, `WRITE`, `CLOSE`, `SETATTR`, `READDIR`. **Zero `LISTXATTRS` in every capture**, which confirms Part 1's op 74 came from knfsd advertising `xattr_support`. |
| h3 | rename | `[SEQ, PUTFH, SAVEFH, PUTFH, RENAME]`, the same as knfsd. |
| h5 | hub restart with an open + lock held | `SEQUENCE` → `BADSESSION`; `DESTROY_SESSION` → `BADSESSION`; **`CREATE_SESSION` succeeds on the SAME clientid** (no `STALE_CLIENTID`, no reclaim); `READ`/`WRITE` with the pre-restart lock stateid succeed; `LOCKU` succeeds. **Design §2's persistence claim holds for this sequence.** |

### Two hub defects the capture found (2 of 2 runs each)

- **D1 — `FREE_STATEID` after the last `LOCKU` returns
  `NFS4ERR_LOCKS_HELD`.** This happens with and without a restart, so the
  control arm shows it is not restart-related. The sequence is: the
  client unlocks its only range (`LOCKU` OK), then frees the lock
  stateid. The hub refuses, the lock stateid leaks, and at unmount
  **every `DESTROY_CLIENTID` gets `NFS4ERR_CLIENTID_BUSY`** ("still
  holds 1 stateid(s), 0 lock(s)"). The client record then lingers until
  lease expiry. RFC 8881 §18.38 allows `LOCKS_HELD` only while locks
  remain.
- **D2 — after a hub restart, `CLOSE` on the restored open stateid
  returns `NFS4ERR_BAD_STATEID`** (hub log: `CLOSE: Invalid stateid:
  NotFound`). This happens only in the restart arm. The hub logged
  "loaded 2 stateid records", I/O through the restored lock stateid
  worked, and `DESTROY_CLIENTID` later counted 2 stateids held. So the
  open is counted under the client but not found by `CLOSE`'s lookup.
  **This qualifies design §2's claim:** a restart is invisible for I/O,
  but not for closing a file that was open with a lock across it.

**Both fixed test-first (2026-09-27).** Each test was run against the
unfixed code and seen to fail at the census's own step:
`FreeStateId(LocksHeld)` after the last `LOCKU` (D1), and a restored
open's `DENY_WRITE` not enforced (D2).
- **D1:** `FREE_STATEID` of a lock stateid succeeds once its owner
  holds no range on the file (`LockManager::release_owner_stateid`).
  Open stateids, and lock stateids with a range held, still answer
  `LOCKS_HELD`.
- **D2 was wider than the capture showed.** `load_records` never
  rebuilt the open index for ANY restored open, so after every restart
  restored opens could not be closed, upgraded or downgraded, and did
  not enforce share-deny. The persisted stateid record now carries the
  open-owner and share masks (three nullable columns, backfilled in
  place), and the index is rebuilt at load. `downgrade_open` now
  persists its seqid bump, which it never did.

Lib suite on real Linux: 2550 passed, 0 failed, 6 ignored. **The drill
that found them, rerun against the fixed hub**
(`results-box-6.12-flint-hub/fixed-restart-{0,1}/`), in both arms:
`LOCKU`, `FREE_STATEID`, `CLOSE` → `NFS4_OK`, and `DESTROY_CLIENTID` →
`NFS4_OK` on its first attempt.

Neither defect is caused by the proxy; both exist on direct mounts
today. D1 matters to the proxy because its backend-client teardown
would meet `CLIENTID_BUSY` on every hub a client ever locked on. Both
are to be fixed test-first, separately from the proxy.

## Part 3 — admin revocation, Linux 6.12 knfsd (the open question)

The 6.8 server in Part 1 could not revoke NFSv4 state; 6.12 can.
Script: `capture-knfsd-612.sh`. One client holds an open and a byte-range
lock on `ws-a/f` and on `ws-b/f` (separate exports, separate fsids,
**one session**, exactly the proxy's shape). The server then revokes
**only `ws-a`** (`unlock_filesystem`). Four arms:

| Arm | `ws-a` write | `ws-b` write | Kernel log |
|---|---|---|---|
| **control**: no revoke, delegations off | ok | ok | — |
| **control**: no revoke, delegations on | ok | ok | — |
| revoke `ws-a`, delegations off (flint's setting) | **EIO** | **EIO** | `lost 2 locks` |
| revoke `ws-a`, delegations on | **EIO** | **EIO** | `lost 1 locks` |

The pcap for the delegations-off revoke arm settles who did it:
- **The server revoked only `ws-a`.** The flag was
  `SEQ4_STATUS_ADMIN_STATE_REVOKED`. `TEST_STATEID` returned
  `NFS4ERR_ADMIN_REVOKED` for `ws-a`'s open (`…01`) and OK for `ws-b`'s
  open (`…04`). `ws-b`'s lock stateid (`…06`) was never revoked.
- **The client recovered client-wide anyway.** It re-opened `ws-b`'s
  file (`OPEN CLAIM_FH`), declared `ws-b`'s lock lost, and failed
  `ws-b`'s writes locally with `EIO`. No WRITE was sent.

**Consequence for the design: never surface one hub's state loss as
`SEQ4_STATUS_ADMIN_STATE_REVOKED`.** Linux 6.12 treats that flag
client-wide. **The proxy puts every workspace a node mounts into ONE
client**, so one workspace's loss would break byte-range locks in
every other workspace that node uses. With direct mounts each hub is a
separate client, so this coupling is **new, and introduced by the
proxy**. The §4 hibernate row is withdrawn. What replaces it:
1. **Make "no live leases" a precondition of hibernation** (this is also
   the HIB-1 fix). A hub whose state would be lost then has no client
   holding any.
2. For the losses that remain (a quarantined `state.db`, a crash that
   loses the PVC), return per-operation errors on that hub's stateids,
   **without** the SEQUENCE flag. Whether Linux keeps that recovery
   per-state is **unmeasured**. It is the step-3 drill, with this
   file's four arms as its template.
