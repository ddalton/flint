# NFSv4.1 READ delegations — "recall-or-die" — design of record

Status: **DESIGN OF RECORD (2026-08-31); IMPLEMENTATION CODE-COMPLETE
2026-09-02 — see the STATUS blocks at the top of §11 for what is
shipped and what remains (the §9 wire legs).** Read this document, and
pass the formal-model gate in §7, before changing ANY delegation code.
§2 describes HEAD as it was before the work began and is kept as the
record of what the design started from.

Provenance: multi-agent design workflow `wf_f2f28ed6-0d1` — 4
understanding agents over the state/wire/callback/conformance machinery,
3 independent designs, 2 judges (lens A: protocol correctness; lens B:
3000-share fleet operator), 3 adversarial verifiers against the winner.
Tally: **Design 1 "recall-or-die" 80.5**, Design 3 "health-gated" 75,
Design 2 "Warmhold" (read+write) 62. Both judges picked Design 1
independently. Verification then found **4 fatal holes and ~12 serious
ones** in the winner; every one is folded in below and marked **[Vn]**.
This document is Design 1 + the judges' mandatory grafts + the
verifiers' fixes — the amended sections are the design; the raw hole
list is §8 so the rationale survives.

## 1. What this buys, honestly

A Linux client holding a READ delegation satisfies `open(2)` locally
(no OPEN/CLOSE RPCs), trusts its page and attribute caches without
CHANGE polling (no GETATTR revalidation storms), and skips per-open
ACCESS checks. On **warm re-access** — the agent-fleet re-read and
model-serving workloads — 3-5 RPCs per re-open cycle (OPEN + GETATTR +
CLOSE, often ACCESS) go to zero: 60-90% of steady-state metadata
traffic on warm files.

The flint-specific bonus: evict/hydrate moves real ctime without
bumping F14, so today **every tier cycle spuriously invalidates every
warm reader's cache**. A delegation holder skips CHANGE revalidation
entirely — tier cycling becomes invisible to warm readers. For
tier-cycling read-mostly sets this is the largest single effect.

What does NOT improve, stated plainly: cold first-reads and cold storms
(explicit non-goal); single-pass streaming (saves at most the CLOSE);
write-heavy or mixed-writer files (recalls add a CB round trip + DELAY
retries — strictly WORSE than no delegations; the anti-flap cooldown
and circuit breaker exist for the pathological cases); directory-heavy
metadata loads (Linux has no useful directory delegations); any file
whose re-access interval exceeds recall/expiry churn. **Throughput
benchmarks (MiB/s) should show ~nothing.** This is an RPC-count and
tail-latency feature; the rig leg scores RPC counts, never MiB/s, and
the cold-storm perf gate will not see it.

## 2. What exists at HEAD (the understanding digest, condensed)

- A standalone `DelegationManager` (`src/nfs/v4/state/delegation.rs`,
  451 lines) mints/tracks/returns read-delegation records and is wired
  into lease-expiry sweep, `cascade_destroy_client`, DESTROY_CLIENTID
  busy-count, and `client_has_live_state`. But the grant is
  **wire-invisible twice over**: `OpenRes.delegation` is a bare enum,
  the dispatcher hardcodes `delegation: None // TODO`
  (dispatcher.rs:1599), and the OPEN encoder unconditionally emits
  OPEN_DELEGATE_NONE (compound.rs:2451). With the gate ON the server
  mints phantom records the client never hears about.
- Delegation stateids come from a **private counter disjoint from
  StateIdManager** — never registered, never persisted; READ or
  TEST_STATEID with one answers BAD_STATEID.
- **DELEGRETURN (op 8) and DELEGPURGE (op 7) are not decoded** — they
  hit the compound decoder's default arm and TRUNCATE the rest of the
  compound. `handle_delegreturn` is dead code.
- The decoder parses all 7 OPEN claim arms but **discards delegation
  stateids and delegate_type**; the dispatcher collapses claims
  1,2,3,5,6 to `OpenClaim::Fh` — CLAIM_DELEGATE_CUR (2) executes
  against the PARENT-DIRECTORY CFH with the filename discarded.
- **No CB_RECALL exists** (CbOp has Sequence/LayoutRecall/
  NotifyDeviceId only), but the backchannel transport and the
  client-addressed callback fan-out (`back_channel.rs`,
  `pnfs/mds/callback.rs`) are production-proven for layout recalls and
  are the intended host.
- The gate `FLINT_NFS_DELEGATIONS` (OnceLock, ioops.rs:242) has exactly
  one production consumer: `try_grant_read_delegation`, called only
  from the no-create OPEN arm. Its conflict check consults only write
  DELEGATIONS (which cannot exist) — not write OPENs.
- Both pynfs baselines (171/0/91) skip all 10 st_delegation tests.
- `validate()` (stateid.rs:641) checks existence/revocation/seqid only
  — no access-mode check anywhere; WRITE accepts the anonymous stateid
  (ioops.rs:1941). Verified at HEAD by the verifiers.

## 3. Scope

**IN (v1):** READ (OPEN_DELEGATE_READ) file delegations only, granted
at the existing single no-create OPEN site (ioops.rs:1361), v4.1
sessions-bound backchannel only. Wire honesty that ships regardless of
grants because its absence is a live decoder bug: DELEGRETURN and
DELEGPURGE argument decode (stop truncating compounds), CLAIM_PREVIOUS
delegate_type threading, explicit NOTSUPP arms for
CLAIM_DELEGATE_PREV/CLAIM_DELEG_PREV_FH, and **real
`OpenClaim::DelegCur` decode + validation** for CLAIM_DELEGATE_CUR(2)/
CLAIM_DELEG_CUR_FH(5) — validate the presented delegation stateid
(this client, this file), mint the open stateid, exempt conversion
opens from fences and share-deny during recall, never re-trigger
recall. This is how Linux converts cached opens before DELEGRETURN;
without it recall convergence depends on an accidental collapse-to-Fh
that never validates the stateid **[V-serious]**. Plus CB_RECALL
emission (new `CbOp::Recall`) on the proven callback plumbing, SEQ4
status surfacing (un-hardcode dispatcher.rs:1000), re-keying
DelegationManager from PathBuf to `(dev,ino)` + stored fh bytes, and
**NFS4ERR_OPENMODE** for a READ-delegation stateid presented on WRITE
(one check in the validate path; today the misuse validates cleanly).

Postures: **standalone hub (`flint-pnfs-mds --standalone`) is the v1
target.** MDS+DS fleet is supported by the same code but stays behind a
**separate `FLINT_NFS_DELEGATIONS_PNFS` flag** until the layout-
interaction fixes (§5.9) and the DS client-behavior pin (§9) are green
— so fleet enablement cannot leak in via copied Helm values. The bare
`flint-nfs-server` binary stays delegation-off (per-COMPOUND-only
courtesy sweep). CSI RWX pods are hub clients; nothing CSI-side
changes.

**OUT, explicitly:** WRITE delegations (CB_GETATTR obligation +
dirty-data loss on revocation — and idle-suspend/restage would destroy
client-cached dirty data; `DelegationType::Write` stays never-granted),
directory delegations, WANT_DELEGATION (op 56, stays NOTSUPP),
NONE_EXT/why_no_deleg, CB_RECALL_ANY / CB_RECALLABLE_OBJ_AVAIL /
CB_PUSH_DELEG / BACKCHANNEL_CTL, delegation reclaim across server
restart (CLAIM_PREVIOUS answers the open with delegation NONE —
deliberately, via the threaded delegate_type), CLAIM_DELEGATE_PREV /
DELEGPURGE retention (NOTSUPP — matches knfsd; Linux doesn't use
them), CB_NULL probing, and any layout-grant-policy change beyond
making `callback_ready` available for a follow-up.

## 4. Grant policy

GRANT a READ delegation on OPEN iff ALL of the following hold, in
order (first failure ⇒ OPEN_DELEGATE_NONE with a per-reason refusal
counter; a refused grant NEVER answers DELAY — delegations are
optional, refusal must be free):

1. **Gate:** `delegations_enabled()` AND the runtime circuit breaker
   (§10) is not tripped AND the sentinel kill-switch file
   (`<export>/.flint-nfs/deleg-off`, ~5s watcher) is absent.
2. **Not in grace** — mirroring the OPEN gate's `anything_reclaimable`
   nuance (dispatcher.rs:1491): a fresh-PVC / hibernate wake with
   nothing reclaimable does NOT blackout grants for 90s **[V2]**. When
   grace is real, no grants (new grants could conflict with
   unreclaimed pre-restart write opens).
3. **Claim shape:** CLAIM_NULL(0) or CLAIM_FH(4) on the no-create OPEN
   arm only. The create arm stays NONE on every path — a just-created
   file has no warm re-access value, and skipping it removes a class
   of create/truncate races.
4. **Share bits, masked not compared:** `(share_access & 0x3) ==
   OPEN4_SHARE_ACCESS_READ`, `share_deny == NONE`, and want bits
   (`share_access & 0xFF00`) neither WANT_NO_DELEG (0x0400) nor
   WANT_CANCEL (0x0500). Today's `share_access == 1` exact-match would
   silently refuse any client that sets want bits.
5. **No conflicting opens:** write-open tally on the file is zero for
   ALL clients — a new `StateIdManager::file_has_write_open` derived
   from open state, **keyed by `(dev,ino)`, not fh bytes** [V3]:
   `opens_by_fh` is fh-keyed, and in path-handles mode hardlinks alias
   one inode under different fhs, falsifying the invariant (the hub
   runs kernel handles, but `fh_kernel::try_new` falls back to path
   handles silently on probe failure).
6. **No conflicting pNFS state (MDS posture):** no outstanding
   write-capable layout on the file held by another client, where
   write-capable = iomode RW **or ANY** (dispatcher.rs:2918 accepts
   iomode 3 today) [V3] — consulted through a **`(dev,ino)`-keyed
   layout index** (the existing `file_ident` is a grant-time path
   string that survives renames un-rekeyed for the files class) [V3].
7. **Backchannel health — `callback_ready(client_id)`:** the client
   has ≥1 session with (a) `cb_program != 0`, (b) a live writer in the
   back-channel registry (liveness tracked and dead writers reaped —
   see §6), (c) a cb_cred the server can emit (AUTH_SYS; refuse
   GSS-only cb_sec — GSS callbacks are recognised-but-unemittable, so
   a GSS-only client would fail every recall), and (d) **backchannel
   `ca_maxoperations >= 2`** — CB_SEQUENCE+CB_RECALL is a 2-op
   compound and CREATE_SESSION accepts max_operations=1 today [graft].
   No CB_NULL probe: in v4.1 the bound session backchannel is the
   RFC-sanctioned verification; the recall ladder handles a channel
   that later lies. Build this as ONE shared predicate — it is the
   same predicate `handle_layoutget` lacks (the known layout soft
   spot); wiring it there is a ~5-line follow-up kept out of v1.
8. **No recall barrier, no pending mutation:** the file entry has no
   record in RECALL_* / REVOKED state, **no live mutation-pending
   guard (§5)** [V1-fatal], the requester doesn't already hold a live
   read delegation on this file, and the file is not inside its
   **post-recall cooldown (~30s, `refused{cooldown}`)** — without the
   cooldown, alternating writers drive grant/recall thrash with no
   damping below the global breaker [graft].
9. **Quotas:** per-client outstanding < `FLINT_NFS_DELEG_MAX_PER_CLIENT`
   (default 4096), global < `FLINT_NFS_DELEG_MAX_GLOBAL` (default
   65536). On breach: NONE, never DELAY.

Multiple concurrent READ delegations on one file across clients are
allowed (conflict is write-capable state, not other read delegations).

**Atomicity:** the grant runs under the per-file entry lock — lock,
re-check `file_has_write_open` + write-capable layout + recall barrier
+ mutation guards, mint via `StateIdManager::allocate(
StateType::Delegation, client)` (persist skipped — §6), insert,
unlock. Paired with the fence protocol below, every interleaving
either sees the mutator (refuse) or the mutator's consult sees the
record (recall).

**Wire delivery:** `OpenRes` gains `delegation:
Option<GrantedReadDelegation { stateid, recall: bool }>`; the
dispatcher constructs the never-yet-constructed `compound::Delegation`;
the encoder grows the OPEN_DELEGATE_READ arm: stateid + recall flag
(false at grant) + one permissive nfsace4 (ALLOW / EVERYONE@ — Linux
ignores it; a zero-length who is riskier to foreign decoders). The
chain lesson applies: encoder, advertisement, and client heuristics
are three parties — the wire-chain test exercises all of them.

## 5. Recall machine

**Record states**, per delegation, in a reworked DelegationManager
keyed by `FileId(dev,ino)`, each file entry a small Mutex-guarded
struct holding `Vec<DelegRecord>` + recall barrier + mutation guards
+ cooldown stamp; records also registered in StateIdManager:

```
GRANTED → RECALL_PENDING → RECALL_ACKED → RETURNED (dropped)
any RECALL_* → REVOKED (record retained, flag set) → freed
GRANTED/any → dropped via client teardown (cascade / lease / return)
```

`DelegRecord`: stateid, client_id, fh bytes verbatim-as-granted
(echoed in CB_RECALL — byte-identical in both fh modes; kernel handles
are identity-based so the stored fh survives rename), dev_ino, state,
granted_at, recall_started_at, first_transmit_at, recall_task.

### 5.1 The mutation-pending guard [V1-fatal — the central amendment]

Design 1's original register-open-before-fence ordering closed the
grant race for write OPENs only. REMOVE, RENAME, SETATTR,
anonymous-stateid WRITE (verified: `validate()` passes ANONYMOUS and
READ_BYPASS), the in-process file API, and LAYOUTGET register
**nothing** the grant path checks — so a grant could land between a
mutation's fence consult and its execution, minting a never-recalled
delegation on a file being removed/truncated/rewritten. The fix is
structural, one protocol for every lane:

`deleg::mutation_fence(dev_ino, fh, conflicting_client, kind)` returns
an **RAII mutation-pending guard** registered in the file entry under
the entry lock at consult time and held until the mutation completes.
The grant's under-lock re-check refuses while any guard is live.
LAYOUTGET(RW/ANY) must insert its layout record (or hold the file
entry lock) BEFORE its fence consult, with removal on refusal —
`pnfs.layoutget` takes no lock the grant path shares today [V3].

**DELAYed-conflictor rollback:** the fence consult runs BEFORE
open-state registration, under the same guard — so a conflicting OPEN
answered DELAY leaves no phantom open state behind (the original
ordering left a registered-but-never-confirmed write open visible to
share_conflict for the whole recall window, with no CLOSE ever coming
if the writer gave up) [V1]. The cooldown (grant rule 8) ensures the
writer's retry beats re-grants after the barrier lifts.

### 5.2 Conflict sites

One funnel, `mutation_fence`, called pre-op, colocated with the
existing tier-marker DELAY consults. The F14 bump-site inventory is
the completeness argument — **enforced by an executable completeness
unit test asserting fence-consult sites == production bump lanes**
[V3] (the prose inventory already drifted once: perfops.rs:2647 is
`#[cfg(test)]` code). Sites:

1. **OPEN with write access** (either arm, any claim): any other
   client holds a live read delegation ⇒ recall ALL holders, answer
   NFS4ERR_DELAY.
2. **OPEN with share_deny & READ** from another client: same.
3. **REMOVE** of a delegated file: recall (CB_RECALL truncate=true),
   DELAY; proceeds on retry once no live records. CLOSE never touches
   delegations, so a zero-opens file can still be delegated — REMOVE
   consults the delegation table, not the open table.
4. **RENAME:** target-overwrite ⇒ recall+DELAY (it is a REMOVE).
   Source rename ⇒ recall+DELAY in v1 (conservative; revisit under
   (dev,ino) keying in v2).
5. **SETATTR** on a delegated file: recall **every record whose
   client_id != mutator**, DELAY iff any such record exists [V1 —
   "non-holder" prose broke multi-holder attr coherence: holder A's
   chmod must still recall holder B]. Blunt rule: ANY SETATTR.
6. **WRITE with a non-delegation stateid** on a delegated file:
   mandatory backstop (anonymous/special stateids bypass OPEN —
   confirmed accepted at HEAD), recall+DELAY.
7. **LOCK WRITE_LT** from another client: recall+DELAY + warn (should
   be unreachable). READ_LT does not conflict.
8. **ALLOCATE / DEALLOCATE / COPY(dest) / CLONE(dest) /
   OPEN-createattrs-size:** fence at each (second-line, DashMap cost).
9. **LAYOUTGET with write-capable iomode (RW or ANY)** (MDS): recall,
   answer NFS4ERR_LAYOUTTRYLATER (open question: vs DELAY — verify
   Linux client behavior on the rig first).
10. **Server-local mutation lanes** (in-process file API,
    conditional-write REST): the file API is an in-process v4.0
    client through the dispatcher, so the OPEN/WRITE fences catch it;
    `conflicting_client=None` ⇒ every holder is "another client".
    NOTE: its sessionless read-opens will increment
    `refused{no_cb}` — scope counters per-client or quiesce the API
    during rigs [V3].
11. **LINK (target file)** [V3]: `handle_link` bumps the target's F14
    (nlink+ctime — attrs the holder caches); recall+DELAY.

**Self-conflict carve-out — stated ONCE in `mutation_fence`** [V1,V3]:
if the mutator is the SOLE holder, send CB_RECALL but do not DELAY —
and because it lives in the funnel, sites 5/6 inherit it (as
originally written, the sole holder's O_TRUNC SETATTR and first WRITE
were re-DELAYed one op after the carve-out exempted the OPEN,
nullifying it). pynfs DELEG23 adjudicates this policy before it
freezes.

### 5.3 CB_RECALL flow

New `CbOp::Recall { stateid, truncate, fh }` + `CbResult::Recall`
copying the LayoutRecall encode/decode shape. New
`CallbackManager::send_cb_recall` using the **client-addressed**
pattern (try every session of the client), with **within-session
writer-Vec iteration on Transport/ConnectionClosed** (never `.first()`
— confirmed at mds/callback.rs:175/376) [graft/V1]. Inherits: per-
session slot-0 mutex across the round trip, strictly increasing
sequenceid (C2), minorversion + cb_cred resolved from the session (the
4.2-client BADSESSION trap), 10s timeout, per-op reply classification
(C3).

**Reply classification, precedence explicit** [V3]: NFS4ERR_DELAY at
ANY level (CB_SEQUENCE or CB_RECALL) ⇒ ladder retry, never revoke.
CB_RECALL NFS4_OK ⇒ Acked. Definitive refusal (PROC unavailable,
BADXDR, NOTSUPP) ⇒ revoke. Client BADHANDLE/BAD_STATEID ⇒ **the
disown rule, amended** [V1-fatal]: a CB_RECALL can cross the granting
OPEN reply on another connection (referring_call_lists is sent empty,
so the client cannot detect the race and answer DELAY) — a client
saying "I don't hold this" may be about to install it. Drop the
record only with evidence the granting reply was consumed (the
grant's fore-channel slot advanced past the granting seqid), or after
one delayed re-probe on the ladder; alternatively populate
referring_call_lists in the recall's CB_SEQUENCE. Never insta-drop.

### 5.4 Timeout ladder

Per delegation, single-flight (the record's `recall_task`; later
conflicts on the same file see state != GRANTED and answer DELAY
without re-sending):

- t=0: conflict detected. GRANTED → RECALL_PENDING under the entry
  lock; conflicting op answered NFS4ERR_DELAY immediately — never
  block in-server beyond an optional bounded ~100ms park-on-waiters
  (catches the common fast DELEGRETURN without burning a client
  backoff cycle; DELAY replies are slot-cached, retried with fresh
  seqids — the proven tier pattern).
- Send CB_RECALL. **The 90s revoke deadline runs from FIRST
  SUCCESSFUL TRANSMIT, not from conflict detection** [V1] — slot-0
  serialization to one slow client (rm -rf over 40 delegated files)
  must not revoke delegations that were never asked for. Batch
  per-client recalls into one CB_COMPOUND where ca_maxoperations
  permits. *(Implemented 2026-09-02 — first transmit only, chunked to
  `ca_maxoperations - 1`, positional reply split. Linux advertises 2,
  so against Linux the chunk is 1 and nothing changes on the wire;
  the fan-in is for clients with a wider back channel.)*
- Acked ⇒ RECALL_ACKED; resend on timeout/DELAY at +30s and +60s.
- Transport/ConnectionClosed/NoChannel on every session ⇒ **bounded
  ~30s CB_PATH_DOWN wait window, NOT immediate revoke** [graft/V1] —
  set SEQ4_STATUS_CB_PATH_DOWN, and **rearm-on-rebind**: a new
  backchannel writer registering (implicit SEQUENCE bind or
  BIND_CONN_TO_SESSION) immediately re-drives pending recalls. An
  nconnect blip or TCP reconnect converts a would-be revocation into
  a completed recall. Requires reaping dead writers from
  `back_channels` (the registry only appends today; the comment
  promising removal is aspirational) — without reap + iterate +
  rearm, one TCP reconnect makes every later recall revoke instantly
  for the life of the session [V1-fatal].
- Deadline with no DELEGRETURN ⇒ REVOKE.
- **Every ladder wakeup re-acquires the entry lock and no-ops on
  record-gone/state-changed** [V1] — dropping a JoinHandle detaches,
  it does not cancel.

**Revocation:** mark the StateIdManager entry revoked, set the
client's pending `SEQ4_STATUS_RECALLABLE_STATE_REVOKED` bit (new
per-client seq_flags OR'd into every SEQUENCE reply — replaces the
hardcoded 0), drop from the live per-file set (barrier lifts, cooldown
starts, DELAYed op proceeds on retry), retain the revoked record until
FREE_STATEID or client death; TEST_STATEID ⇒ NFS4ERR_DELEG_REVOKED.
Revocation without a client-visible signal is the named worst case.

**DELEGRETURN:** validate exact (seqid 1 for life; wrong ⇒ OLD_STATEID;
unknown ⇒ BAD_STATEID; revoked ⇒ DELEG_REVOKED with record retained);
on OK remove from both managers under the entry lock, lift barrier,
NFS4_OK. No F14 bump. Never gated by the recall barrier (structurally
guaranteed: the fence lives in mutation lanes, DELEGRETURN is not
one). Expired-courtesy-holder short-circuit at the conflict consult:
check the holder's lease first and revoke-instead-of-recall for
already-expired holders [graft].

## 6. State and restart [V2 — both fatal holes live here]

**DECISION (amended): near-zero persistence — delegations die with the
process, but the server persists minimal HOLDER EVIDENCE so no client
can keep trusting a delegation the server forgot.**

The original design said "zero persistence; clients recover via
BADSESSION → CREATE_SESSION → grace reclaim." **That recovery does not
happen in this repo** [V2-fatal]: on a same-PVC restart (pod roll,
upgrade, node drain — and the documented kill switch itself),
`load_from_backend` restores the client record with
`reclaim_complete=true` and all open/lock stateids; EXCHANGE_ID hits
case 1 (same owner+verifier ⇒ same confirmed clientid); Linux treats
it as session loss, NOT server reboot — no CLAIM_PREVIOUS, no cache
invalidation. A delegation holder then serves its page cache forever
against a server that forgot the delegation, and by design it sends no
RPCs that could surface BAD_STATEID. The persistence layer exists
precisely to make transparent restarts possible; delegations must not
be silently dropped across one.

Mechanics, amended:

- Mint through `StateIdManager::allocate(StateType::Delegation, ...)`
  — this is what makes READ/TEST_STATEID/FREE_STATEID and quotas work,
  killing the disjoint-namespace BAD_STATEID trap — with the
  per-stateid `persist()` skipped for the Delegation type.
- **At grant, persist a per-client "holds recallable state" marker
  row** (or per-delegation tombstone). On `load_from_backend`, any
  client with the marker gets `SEQ4_STATUS_RECALLABLE_STATE_REVOKED`
  set pre-armed, so its first SEQUENCE after the restart tells Linux
  to drop and revalidate its delegations. The original "defensively
  DELETE Delegation rows at startup" becomes "convert to revoked
  tombstones/flags" — never erase the only durable evidence [V2].
- **Stateid counter monotonicity** [V2]: unpersisted delegation mints
  advance the in-memory counter past the persisted high-watermark, so
  after restart the counter would re-issue values still live in a
  holder's memory (cross-client stateid collision). Fix: mix a
  per-boot epoch into the delegation stateid `other` bytes (reject
  stale-epoch on validate), or persist a block-amortized high-
  watermark reservation at mint.

**Idle-suspend integration — mandatory before default-on in any hub
posture** [V2-fatal]: delegations invert the idle signal. `activity.rs`
counts exactly the ops delegations eliminate, so a delegation-warm
fleet classifies as idle BY CONSTRUCTION; the controller suspends the
hub, and wake is the same-PVC silent-loss restart above. Outstanding
delegations must feed `lite_operator/idle.rs` as a `delegations_live`
input (like `sessions_live`, defaulting to BLOCK suspend), or a
pre-suspend recall-all drain hook must run. A chart-docs note about
longer thresholds is not a fix — any threshold is eventually crossed
by a fleet whose steady-state wire traffic is zero by design.

RFC legality of declining reclaim: delegations are optional, revocable
leased state (RFC 8881 §10.4 — never granting at all is compliant).
CLAIM_PREVIOUS during grace reclaims the OPEN and answers the
delegation half NONE — safe exactly because (a) no new grants during
grace, (b) READ delegations carry no dirty client state. This is why
write delegations are out of scope: declining a WRITE reclaim can lose
data, and "no persistence" stops being free. CLAIM_DELEGATE_PREV /
CLAIM_DELEG_PREV_FH ⇒ NOTSUPP always (we never retain across CLIENT
restart; Linux doesn't use them). DELEGPURGE ⇒ decode clientid,
NOTSUPP, and stop truncating the compound.

Lease expiry of a holder: existing cascade reaches
`cleanup_client_delegations`; teardown of a record in RECALL_* lifts
the file barrier so DELAYed conflictors proceed. DESTROY_SESSION:
delegations survive (client-scoped); if the destroyed session was the
last callback-capable one, the next conflict finds NoChannel ⇒ the
ladder's CB_PATH_DOWN window ⇒ revoke + SEQ4 on remaining sessions.

**Out-of-band mutation residual — explicit operator contract** [V3]:
an admin editing files directly on the PVC (kubectl exec, any
non-flint process) bumps no lane, fires no recall — today the holder
revalidates within acregmax; with a delegation it serves stale
FOREVER. There is no lane to fence, by construction. Accepted
residual, with a runbook rule: **sentinel kill-switch + rate-limited
drain before any out-of-band write to a delegation-enabled export.**

## 7. Formal model — GATING, written before ladder code

**STATUS: DONE (2026-08-31)** — `formal/FlintDelegRecall.tla`, 11 runs
in the gate (now 207): strict breadth + liveness, the four fatal holes
as required-to-fail mutations (NoGuard / DisownDrop / NoEvidence, and
hole 3 as the RearmWorks/RearmStale inverse pair), the C5-drift lane,
the detached-ladder discipline, and two vacuity probes. See the
`FlintDelegRecall` section of `formal/README.md`.

`formal/FlintDelegRecall` (TLA+), added to the 196-run gate, written
BEFORE the recall-task code. Both judges and two verifiers converged
on gating (upgrading D1's "recommended"): this state machine is
exactly the shape the repo's models have refuted pre-code three times,
and the tier modules found campaign bugs after a 44-leg campaign
passed.

Event set (the verification holes name the transitions the model must
carry): grant, conflict-with-mutation-guard (§5.1's protocol),
CB_RECALL send/cross-with-granting-reply (the disown race [V1]),
ack, return, revoke, ladder wakeup on a freed record, client expiry,
backchannel death **modeled as killable-and-staying-dead (not
lossy-but-eventually-delivering)**, rebind-rearm, server restart
**including the same-PVC transparent-restore arm** [V2], and
suspend/wake. Invariants: (a) no foreign write-capable state (write
open, RW/ANY layout, live mutation guard) concurrent with a live read
delegation; (b) every DELAYed op is eventually unblocked; (c) after
ANY restart path, every pre-restart holder observes a revocation
signal before the server accepts a conflicting write (no
silent-stale). The abstraction-was-the-bug lesson applies: model the
guard protocol and the disown evidence rule concretely, not as
atomic-magic.

## 8. What adversarial verification found (why the amendments exist)

Fatal:
1. **Grant races every non-OPEN mutation lane** — the lost-wakeup
   proof covered write OPENs only; REMOVE/RENAME/SETATTR/anon-WRITE/
   file-API/LAYOUTGET registered nothing the grant checked. Fix §5.1.
2. **The disown rule loses the delegation when CB_RECALL crosses the
   granting OPEN reply** (empty referring_call_lists; DRC replay makes
   it worse). Fix §5.3.
3. **Append-only back_channels + `.first()` ⇒ one TCP reconnect makes
   every later recall revoke instantly.** Fix §5.4 (reap, iterate,
   rearm, CB_PATH_DOWN window). CORRECTION (verified in code
   2026-08-31): the REAP half already shipped — `server_v4.rs`'s
   `InflightGuard` purges a connection's writer from the registry on
   every exit path (the F18/audit-C5 fix; the verifier's "grep
   confirms no retain" was wrong). The `.first()` iteration half stood
   and is fixed in slice 2; a write can still beat the reap (the read
   loop notices the EOF after the send fails), so iteration remains
   load-bearing, and rearm + the CB_PATH_DOWN window remain slice-3
   work.
4. **Same-PVC restart is transparent to clients ⇒ unpersisted
   delegations vanish with zero client-visible signal — stale cache
   served forever on every pod roll**; and **idle-suspend is
   triggered by the feature's own success**, and wake IS that
   restart. Fix §6 (holder evidence + idle integration).

Serious (all folded in): DELAYed-conflictor rollback unspecified;
layout table path-keyed + un-rekeyed on rename + no insert/consult
ordering; iomode ANY bypass; `file_has_write_open` fh-keyed
(hardlinks); conversion opens collapse to Fh unvalidated (claim 2
lands on the parent CFH); ladder deadline from detection vs transmit;
ca_maxoperations=1 grants-then-revokes; no OPENMODE check; stateid
counter reuse after restart; grace blackout on fresh-PVC wakes;
out-of-band mutation class; server-side-writer conflict path untested
(file API 503 retry budget vs 90s ladder unpinned); LINK missing from
the site list; negative legs 2/3 vacuous as written (§9);
CB_SEQUENCE-level DELAY classified as refusal; grants_paused trips
forever on one broken client / resets on restart / has no true manual
trip (sentinel file is the manual lever); DS "acceptance" leg vacuous
(DS discards stateids — reshape as client-behavior pin).

## 9. Test plan

**pynfs** (lima, private port — two sessions share the VM): keep both
gate-off baselines verbatim (dark-behavior pin, floor 171). Add
flag-ON legs for standalone + MDS with their own baseline JSONs.
Expected motion in st_delegation (all 10 SKIP today): DELEG1/4/8/9
PASS; DELEG5/6/7 (CBSecParms) execute — pin and investigate before
default-on (the AUTH_SYS-only cb_cred policy must not turn them FAIL);
DELEG23 adjudicates the self-conflict carve-out BEFORE it freezes;
DELEG2 stays SKIP (assert SKIP, not FAIL). Re-run the full 262 with
the flag ON: floor-171 must not regress. **nfstest_delegation** as
suite #2 (Linux server; never quote nfstest with the server on macOS).

**★ DELEG4 could not have passed as this paragraph assumed [2026-09-01].**
`testNoDeleg` sets OPEN4_SHARE_ACCESS_WANT_NO_DELEG and requires
OPEN_DELEGATE_NONE_EXT with `ond_why == WND4_NOT_WANTED`.
NONE_EXT was implemented NOWHERE — no constant, no encoder arm, no
reason code — so every refusal encoded a bare OPEN_DELEGATE_NONE and
DELEG4 was a guaranteed FAIL. Fixed; the paragraph's prediction is
sound only from that commit forward. Two rules came out of it and are
now enforced by unit tests:

- NONE_EXT is also how a server signals it understands WANT bits at
  all, so it is sent ONLY to a client that set one. Volunteering it
  answers a question nobody asked.
- It is NEVER sent with the flag off. The kill switch's promise is a
  wire identical to the pre-feature server, and an informational arm
  is still a wire change. **So DELEG4 is a flag-ON expectation and
  FAILS on the control arm — that is the pin, not a regression.**

The ordering defect underneath it is the more general lesson: the
want-bit check sat BELOW the server-side gates, so a server that
merely happened to be unable to grant answered WND4_RESOURCE to a
client that had asked for no delegation. "I would have, but I could
not" is a different statement from "you told me not to". The client's
own instruction is now consulted before any gate but after the flag.

And the encoding trap worth carrying to any future union work:
`open_none_delegation4` switches on ond_why, and only WND4_CONTENTION
and WND4_RESOURCE carry a trailing bool — every other arm is `void`.
A bool on a void arm raises no error; it shifts every following word
of the compound, so the client mis-decodes the NEXT operation and
blames that. The test asserts reply LENGTH per arm, not the reason
code.

**Warm re-access rig leg** (the feature's raison d'être; paired
per-rep, interleaved arms): pass 1 opens+reads N files, sleep past
acregmax, pass 2 re-opens+re-reads; score OPEN/GETATTR/CLOSE/ACCESS
deltas from mountstats. Assert flag-ON pass-2 metadata RPCs < 5% of
pass-1. Anti-vacuity, all mandatory: (a) flag-OFF control shows
pass-2 ≥ ~80% of pass-1 (the rig can see the storm it claims to
eliminate); (b) `deleg_granted_total ≥ 0.95·N` on the ON arm (grants
actually happened — and the grant-coverage floor); (c) **invalidation
control + content oracle** [graft]: mid-run a second client writes
exactly 10 files; exactly those 10 re-fetch sha256-fresh content,
`cb_recall_outcome{acked} == 10`, recall p99 < 5s — the only guard
that catches stale-cache-served-forever masquerading as RPC
elimination. Positive WANT-bits pin: OPEN with WANT_READ_DELEG ORed
in still grants.

**★ MEASURED, 2026-09-01 — `tests/lima/deleg/pynfs-deleg.sh`.** Three
arms against one build: flag OFF (control), flag ON at the shipped 90s
deadline, flag ON at a 5s deadline. Result with the flag ON: **9 of 10
st_delegation tests pass** (DELEG1/3/4/5/6/7/8/9/23), 14 grants
observed, zero grants on the control.

Two pinned non-passes, both expectations rather than deferrals:

- **DELEG2 FAILs by design.** It asks for a WRITE delegation; flint
  grants only READ (§1, an explicit non-goal). The earlier note that
  it "stays SKIP" holds only under the `writedelegations` flag gating —
  selecting the `deleg` set runs it, and it fails.
- **DELEG8 FAILs at the shipped 90s deadline for a CLIENT-side
  reason**, and this is a finding about the deadline, not about pynfs.
  pynfs allows a compound 10 DELAY retries at 1s and then gives up; its
  slot bookkeeping then leaks the slot it acquired for the retry it
  never made, so the outer loop dies "Out of slots". **At the
  production deadline no pynfs test can ever WATCH a revocation
  happen.** The ladder timings are therefore env-overridable (defaults
  unchanged, pinned by a unit test) and the third arm runs at 5s, where
  DELEG8 passes and exercises the whole revocation chain end to end:
  recall → ack → deadline → revoke → READ answers DELEG_REVOKED → SEQ4
  RECALLABLE_STATE_REVOKED → TEST_STATEID → FREE_STATEID clears the
  bit. That is §9's "acks-but-never-returns" and TEST_STATEID negative
  legs, discharged against a real client.

  The general lesson for §10's rollout: **90s exceeds what a real
  client's DELAY budget may be.** The server-side writer leg's open
  question ("nothing pins today that REST callers retry for up to 90s
  — measure and state the budget") now has one measured data point,
  and it is a client that gives up at 10s.

**What the run found that no unit test could.** The feature was INERT
in both binaries — `install_recall_spawner` had callers only in
`#[cfg(test)]` code — and the symptom was silence, because a refused
grant is indistinguishable from a workload that never qualified. Then,
once it could ask, a real client found three protocol defects in one
run: the CREATE arm never answered WANT bits, BACKCHANNEL_CTL was
undecodable (truncating any compound containing it), and a revoked
delegation answered BAD_STATEID instead of DELEG_REVOKED. None was
reachable from the Rust suite, which was green at 2300+ throughout.

**★ WARM RE-ACCESS, MEASURED 2026-09-02 — `tests/lima/deleg/warm-reaccess.sh`.**
Real Linux 6.x client, 40 files, `/proc/self/mountstats` per-op deltas,
two arms differing only in the flag. Three passes: cold, warm past the
attribute cache, warm inside it.

```
                pass1  pass2  pass3   per-op pass2 / pass3
flag OFF           80     80     80   OPEN_NOATTR=40 CLOSE=40 / OPEN_NOATTR=40 CLOSE=40
flag ON            80     40      0   GETATTR=40              / (none)
```

**Inside the attribute-cache window the elimination is TOTAL: 80 → 0.**
Past it, the OPEN/CLOSE pair is gone but ONE GETATTR PER FILE REMAINS —
50%, not the "<5% of pass 1" this section predicted. The prediction was
wrong and the number stands as measured.

The control is loud on every pass (80 RPCs, ratio 1.00), which is what
licenses reading the treatment's silence at all — and note the control
is loud in pass 3 too: Linux re-OPENs on every `open(2)` regardless of
its attribute cache, so the OPEN/CLOSE saving is unconditional and it
is the GETATTR that is timer-driven.

**The residual is not a lost or recalled delegation.** Server-side
counters for the whole run: 40 granted, `outstanding 40` throughout,
`recall sent +0`, `returned +0`, `revoked +0`. The client held all
forty continuously and revalidated attributes anyway. Cause not yet
attributed — do not guess it; it is a client-behaviour question and the
pass-3 discriminator is what narrows it.

Practical reading for §1's claim: this rig mounts with `acregmax=5` so
the control is loud without a 60s wait. **Linux's default is 60s**, so a
workload re-accessing inside a minute gets the 100% regime and one
re-accessing beyond it gets 50%. §1's "60-90% of steady-state metadata
traffic on warm files" is consistent with the first regime and
optimistic for the second.

**★ LIVENESS PRECONDITION ON EVERY CONTROL ARM [V4, 2026-09-01 — from
the oci-ab campaign's G-COLD confound].** Every leg above whose
flag-ON assertion is *near-zero RPCs* must first assert that its
flag-OFF control arm is **actually loud** — server-side RPCs > 0, in
range of the stated expectation — and be **VOID, not PASS**, when it
is not. Three-state like the stripe-width gate: the third state is
the one that does the work.

The reason a flag-OFF control alone is insufficient: **a control that
is equally broken is not a control.** If both arms are warm enough to
be quiet, flag-OFF also shows ~zero RPCs, the arms agree, and the leg
reads "no difference" when the truth is "the rig is blind". Quiet is
exactly what a broken rig produces, so a leg that concludes from quiet
cannot distinguish success from blindness.

What made this visible in the field was **a server-side counter with a
known non-zero expectation**, not client timing and not the absence of
errors: five cold pulls of a 400 MB image produced 2 `LAYOUTGET
granted` lines where the workload should produce ~42. Absurd by
inspection *because the expected magnitude was known in advance*. So
each control arm states its expected count before the run, and the
assertion is against that number.

Concretely for the tier leg: after evicting to the stub, confirm a
**cold** holder *does* generate server reads, and only then does "warm
holder re-reads with zero RPCs" attribute to the delegation rather
than to anyone's page cache. For the warm re-access rig: the flag-OFF
pass-2 floor (~80% of pass-1) is a liveness assertion, not a
formality — if it fails, the run is void and the flag-ON number means
nothing. The arms must match on **warmth**, not merely on workload:
cooling the client while the server stays warm cools one side of a
two-sided measurement.

Same rule as the §6 restart leg's "assert the precondition, not just
the postcondition" and the F68 setup guard: in all three the failing
version *looked healthier* than the working one, because absence of
signal reads as success.

**Negative legs** (the GSS lesson — negative legs find the defects),
with the [V3] vacuity fixes baked in: legs that kill the backchannel
must keep the holder's FORE path demonstrably alive — in v4.1 the
backchannel rides the fore TCP connection, so "iptables the callback
path" as naively written severs lease renewal and makes the 90s
ladder indistinguishable from 90s lease expiry. Use a scripted pynfs
client that keeps SEQUENCEing while refusing CB traffic, or `ss -K`
only the bound connection of an nconnect pair; assert
`deleg_revoked_total{reason} == 1`, the client record still exists at
unblock, and the SEQ4 bit is observed on the live connection. Legs:
no-backchannel client ⇒ zero grants; dead backchannel ⇒ revoke within
window + SEQ4 + TEST_STATEID ⇒ DELEG_REVOKED + FREE_STATEID clears;
acks-but-never-returns ⇒ revoke at deadline; gate-off vacuity
(`deleg_granted_total == 0` under the full rig); WANT_NO_DELEG ⇒
`refused{share_want}`; GSS-only cb_sec ⇒ zero grants; compound
[DELEGPURGE, GETATTR] and [DELEGRETURN(bogus), GETATTR] decode fully
— per-op status, NO truncation; CLAIM_DELEGATE_PREV ⇒ NOTSUPP, never
an Fh open on the parent.

**★ MDS ARM MEASURED, 2026-09-02 — `tests/lima/deleg/pynfs-mds.sh`.**
The delegation legs against `flint-pnfs-mds` in real MDS posture (mode:
mds, so `pnfs_ops` is Some and `pnfs_posture()` is true), three arms:

```
                       grants   DELEG1/3/4/5/6/7/9/23   DELEG2   DELEG8
  off       (no flags)      0   FAIL                     FAIL     FAIL
  pnfs-off  (DELEGATIONS)   0   FAIL (DELEG4 PASS)       FAIL     FAIL
  on        (both flags)    7   PASS                     FAIL     FAIL
```

**Slice 5's gate is real on the wire: zero grants with
`FLINT_NFS_DELEGATIONS_PNFS` unset, seven with it set**, on the same
binary and the same posture. The `pnfs-off` arm is the one that makes
this a measurement rather than a demonstration — without it, "the gate
works" and "the MDS cannot grant at all" produce identical output, and
the rig treats agreement between that arm and the ON arm as VOID.

The expectation table is IDENTICAL to the standalone binary's, which is
the answer to "does the MDS posture change the grant decision": it does
not, once its gate is open. DELEG2 (asks for a WRITE delegation) and
DELEG8 (pynfs's DELAY budget, not a server fault) stay pinned FAIL for
the reasons recorded above.

**DELEG4 PASSES on the `pnfs-off` arm** — and that is the want-bit
ordering rule, observed on the wire in the exact posture it was written
for. The client's own WANT_NO_DELEG instruction is answered
(NONE_EXT/WND4_NOT_WANTED) even though the posture gate would refuse
any grant, because the client's instruction is consulted BELOW the
master flag but ABOVE every server-side gate. No previous run could
show this: it needs a posture where the flag is on and a gate is shut.

The server now states its own posture at startup —
`posture=MDS · pnfs gate=ON · layout probe=installed` — so a rig has
the server's word for all three things that decide whether it will ever
grant, rather than inferring from silence. The probe field is there
because rule 6 fails CLOSED without its oracle, and "refused
everything" and "granted nothing because nothing qualified" look
identical in a log.

**★ NEGATIVE LEGS MEASURED, 2026-09-02 — `tests/lima/deleg/negative-legs.sh`
+ `st_flintdeleg.py`** (installed into pynfs's own harness, so the legs
get sessions, credentials and compound plumbing for free):

```
  FLINTNEG1  no back channel  => no delegation      off FAIL / on PASS
  FLINTNEG2  [PUTROOTFH, DELEGPURGE, GETATTR]       off PASS / on PASS
  FLINTNEG3  [PUTFH, GETATTR, DELEGRETURN(bogus)]   off PASS / on PASS
  FLINTNEG4  WANT_NO_DELEG => NONE_EXT/NOT_WANTED   off FAIL / on PASS
  FLINTNEG5  compound-shape calibration             off PASS / on PASS
  FLINTNEG6  CLAIM_DELEGATE_PREV refused, no CFH    off PASS / on PASS
```

**Grant rule 7 is now proven on the wire.** FLINTNEG1 carries its own
control, and it has to: "no delegation" is equally the answer of a
working gate, a switched-off feature and a broken server. So it first
opens on an ORDINARY session and requires a delegation, then makes the
identical open on a session created without
CREATE_SESSION4_FLAG_CONN_BACK_CHAN and requires none. That the shape
legs (2/3/5) pass on the OFF arm is what proves the rig ran there at
all, which is what licenses reading 1 and 4's failures on that arm as
refusals rather than as a dead rig.

**FLINTNEG5 exists because it caught three phantom defects.** The first
cut of legs 2 and 3 reported that flint truncated compounds before the
failing operation — the BACKCHANNEL_CTL shape, three findings, all
false. pynfs's `resarray` does not carry the SEQUENCE result that
`sess.compound` prepends, so every count was off by one. The replies
were correct throughout: `[(PUTROOTFH,OK), (DELEGPURGE,NOTSUPP)]` and
`[(PUTFH,OK), (GETATTR,OK), (DELEGRETURN,BAD_STATEID)]` — the failing
operation's own result present in both, the compound stopping exactly
where it should. The calibration leg pins the arithmetic against a
known-good and a known-bad compound, and the runner WITHDRAWS the
shape verdicts rather than reporting them if it fails. A shape verdict
on top of a miscalibrated counter is not a finding, it is a rumour.

Two rig traps worth carrying: `create_file` opens SHARE_ACCESS_BOTH by
default, and a write open by the same client refuses the grant by rule
5 — a control built on it can never succeed; and design §4 rule 3 means
the CREATE arm never grants at all, so a delegation control must use a
NO-CREATE open. Both produced a "server won't grant" failure that was
entirely the test's.

FLINTNEG6 carries the F69 shape as its second half, which is the
reason it is a leg rather than a status check: a no-create OPEN that
fails must not leave the current filehandle on the PARENT directory.
When it does, the error is returned correctly and the NEXT operation
addresses the wrong object — so the leg sends a GETFH after the failed
OPEN and requires that the compound stopped instead of answering it.
Refusal is flag-independent (flint never supports reclaim), so this one
passes on both arms.

"Acks but never returns ⇒ revoke at deadline" is NOT repeated here: the
`on-short` arm of `pynfs-deleg.sh` already runs it end to end through
DELEG8 at a 5s deadline — revoke, READ ⇒ DELEG_REVOKED, SEQ4
RECALLABLE_STATE_REVOKED, TEST_STATEID ⇒ DELEG_REVOKED, FREE_STATEID
clears the bit.

Still open in this leg: dead-backchannel revoke, which is a TRANSPORT
failure rather than a client that acks and holds — it needs `ss -K` on
one connection of an nconnect pair, or a scripted client that keeps
SEQUENCEing while refusing CB traffic, because the naive version severs
lease renewal and makes the ladder indistinguishable from lease expiry.
And GSS-only cb_sec, which this pynfs build cannot run: it reports "no
gssapi module" at import.

**★ CONFLICT-SITE MATRIX MEASURED, 2026-09-02 —
`tests/lima/deleg/conflict-matrix.sh` + `st_flintconf.py`.** Two
clients, five sites, the full sequence required at each:

```
  FLINTCONF1  open_write   off FAIL / on PASS
  FLINTCONF2  remove       off FAIL / on PASS
  FLINTCONF3  rename_src   off FAIL / on PASS
  FLINTCONF4  link         off FAIL / on PASS
  FLINTCONF5  setattr      off FAIL / on PASS
```

At every site: **B's FIRST attempt answered NFS4ERR_DELAY**, A observed
CB_RECALL and returned, B's retry succeeded. Server-side counters per
leg confirm the chain rather than the client's impression of it —
`granted +1 · recall sent +1 acked +1 · returned +1 · delay +N`, and
`revoked +0` throughout: every recall was honoured by the client, none
timed out.

DELAY is REQUIRED here, not merely permitted, and that is the whole
difference between this matrix and pynfs's own DELEG1 — which accepts
`[NFS4_OK, NFS4ERR_DELAY]` from its conflicting open and therefore
passes just as happily against a server that never fenced anything and
let the writer straight through. Silent success is the failure mode
this matrix exists to catch, so it is spelled out as a failure.

Every leg begins by requiring client A to actually hold a delegation,
so every leg FAILS on the control arm with "client A got no
delegation". That asymmetry is the control: legs that passed on both
arms would not be measuring the fence.

`link` earns its place separately from the others — the new name is a
second path to the same inode, so a fence keyed on the filehandle
rather than on the file's identity would let it through while looking
correct at every other site.

Sites still unmeasured on the wire: `layoutget_rw`, `layoutcommit` and
`write_proxy` (they need a kernel pNFS mount against the MDS with a DS
fleet, which is also where LAYOUTTRYLATER vs DELAY gets OBSERVED rather
than argued — §13 open question 3), the v4.2 sites
(allocate/deallocate/copy_dst/clone_dst), `lock_write`, and
`open_create`. The server-side writer leg (hub file-API upload against
a held delegation ⇒ first response 503) is also still open.

**★ nfstest_delegation MEASURED, 2026-09-02 —
`tests/lima/deleg/nfstest-deleg.sh`.** Suite #2 is live. It asks what
pynfs does not: it reads the PACKET TRACE and asserts what the CLIENT
did with the delegation, which is where §1's claims actually live.

```
  basic set, flag ON     165 / 177 passed   (12 failed)
  READ subset, flag ON    60 /  60 passed   (basic01, 03, 05)
  READ subset, flag OFF   31 /  35 passed   ← the control
```

**Every read-delegation assertion passes.** From the trace, on a real
Linux 6.8 kernel client: OPEN sent with CLAIM_NULL, the file's name and
the directory's filehandle; **READ delegation granted**; **no second
OPEN for the same file**; READs carried the DELEGATION stateid and
returned NFS4_OK; **no READ at all when a different process read the
delegated file**; CLOSE with the correct OPEN stateid; DELEGRETURN
after the close, carrying the delegation stateid. That is the §1 claim
— the metadata round trips do not happen — verified on the wire rather
than inferred from a counter.

**All 12 failures are confined to the nine WRITE-delegation tests**,
which flint refuses by design (§1, an explicit non-goal): 9 are
literally "WRITE delegation should be granted". The other 3 are "OPEN
should be sent with the filehandle of the file to be opened", and that
one is a DEPENDENT rather than a separate defect — **it fails on the
control arm too**, alongside the missing delegation, because a client
holding nothing re-opens by name. Running the read-only subset alone
(60/60) is what proves the confinement rather than assuming it.

**The control is loud**: with the flag off, "READ delegation should be
granted" fails three times. Without that, 60/60 would be a statement
about nfstest's willingness to pass, not about the feature.

TWO RIG FACTS, both of which produced convincing false results first,
and both now baked into the script:

- **The server runs INSIDE the VM, as root, on ext4.** A macOS-hosted
  server runs as a non-root uid against a root client; 35 of 42
  nfstest_posix "failures" were once exactly that. The rule stands.
- **Client and server need DISTINCT IP addresses.** nfstest identifies
  calls and replies by source/destination, so with both ends on
  127.0.0.1 every packet has src == dst and its matching finds
  nothing. It reported "OPEN should be sent" about a trace that
  plainly contained OPENs, and "READ delegation should be granted"
  about a trace that plainly contained `rd_deleg_stid` — two failures,
  one cause, neither of them the server's. `nfstest_pkt` on the same
  capture is what settled it. The rig now puts the server in a netns
  behind a veth pair (10.200.0.2) with the client in the root
  namespace (10.200.0.1), and passes `--client-ipaddr` explicitly —
  without it nfstest auto-detects the VM's main address, tcpdump
  captures nothing, and the failure surfaces as "Packet trace file is
  empty".

**The recall set (recall01-54) is NOT run**: it needs `--client`, a
SECOND host with passwordless ssh and sudo, and there is one VM. The
conflict-site matrix above covers the same ground from pynfs with two
sessions; a second VM would be needed to run nfstest's version.

**Restart/suspend legs**, rewritten for the same-PVC arm [V2]:
(a) same-PVC pod roll with grants outstanding (pre-kill: assert
`deleg_granted_total > 0` and holder TEST_STATEID OK) ⇒ the holder's
next SEQUENCE carries RECALLABLE_STATE_REVOKED and a content oracle
shows revalidation — NOT merely "conflictor proceeds" (Linux never
sends CLAIM_PREVIOUS on this path; the old leg could not fail);
(b) kill -9 mid-RECALL_ACKED ⇒ conflictor's retry proceeds after
grace; sqlite has zero live Delegation rows (tombstones only);
(c) no grants during real grace; no blackout on fresh-PVC wake;
(d) hub restage to fresh PVC ⇒ STALE recovery, no wedge (pre-restage
grant floor asserted); (e) idle-suspend: a delegation-warm fleet does
NOT suspend (or drains first), pinned at the operator.

**Conflict-site matrix** (two clients, every site in §5.2 including
LINK and LAYOUTGET-ANY): assert B's FIRST attempt answered DELAY (not
silent success), A observed the recall **scored on
`cb_recall_outcome{acked}`, never `cb_recall_sent`** [V3], A returns,
B's retry succeeds. **Server-side writer leg** [V3]: hub file-API
upload / conditional-write against a Linux-held delegation — first
response 503 (never silent success), acked recall, sha256 oracle on
the holder's next read, retried upload succeeds within a stated
budget (nothing pins today that REST callers retry for up to 90s —
measure and state the budget).

**Wire-chain unit leg:** gate on ⇒ OPEN answers OPEN_DELEGATE_READ;
the stateid passes TEST_STATEID and is accepted by READ (pins the
BAD_STATEID trap dead); CB_RECALL fh bytes == granted fh bytes in
kernel-handles mode. **Tier leg** with [V3] guards: grant, evict to
stub (assert EvictOutcome committed / marker present), holder re-reads
from cache AFTER sleeping past acregmax (zero server RPCs), flag-OFF
control shows today's spurious-invalidation re-read storm; writer ⇒
recall ⇒ hydrate ⇒ write succeeds. **Concurrency stress** (10k
iterations, readers racing a writer loop, WITH hardlink pairs): the
post-iteration invariant scan plus a **write-time debug assert in the
fence** (no live foreign delegation at write execution — transient
windows the scan misses) plus a granted floor for the run. *(SHIPPED
2026-09-02 as `state/deleg_stress_tests.rs` — see §11 for the shape
and the first run's numbers.)* **DS
client-behavior pin** (MDS posture): a client holding delegation +
READ layout demonstrably serves its READs from the DS (mountstats/DS
op counters) — NOT a DS "validation" assertion; the DS discards
stateids at HEAD (ds/server.rs:707/745/1393), so acceptance-shaped
assertions are vacuously green; pin the discard so future DS work
cannot silently break the path *(the discard pin SHIPPED 2026-09-02 —
`pnfs/ds/server.rs` `deleg_stateid_pin_tests`; the mountstats/DS-op-
counter wire leg is still open)*. Rust legs run on real Linux (zig
cross-build; the macOS suite is NOT the suite); `cargo test
--all-targets` (plain `cargo test` does not build bins).

## 10. Rollout, kill switches, observability

Master gate `FLINT_NFS_DELEGATIONS` (existing env). **v1 ships DARK**
— the splice playbook: dark one release, default-on the next after
burn-in, standalone hub posture first; MDS posture stays behind
`FLINT_NFS_DELEGATIONS_PNFS` until §5.2-9/§9-DS legs are green.
Existing gate-off pinning tests kept verbatim.

Kill layers:
1. **Full kill = unset env + roll the pod** — safe by construction
   ONLY AFTER the §6 holder-evidence fix lands (until then a roll
   with grants outstanding IS the silent-stale scenario; the runbook
   must say so) [V2].
2. **Sentinel file** `<export>/.flint-nfs/deleg-off` (~5s watcher):
   the true manual, no-restart grant stop; doubles as breaker
   re-enable. Plus opt-in **rate-limited drain** (recall-all at
   ~50/s) — the remediation lever for the out-of-band-write runbook
   rule.
3. **Automatic circuit breaker** `grants_paused`: trips when
   revocations exceed `FLINT_NFS_DELEG_REVOKE_TRIP` (default 10) in 5
   minutes; stops NEW grants only (recalls/returns/SEQ4 keep running
   so state drains). Amended [V1/V2]: **per-client quarantine first**
   (refuse grants to a client whose revocations exceed a per-client
   cap — one NAT-broken fleet member must not darken delegations for
   everyone), auto-reset after a quiet interval, and persist the trip
   (state.db row or the sentinel) so a roll cannot silently re-arm
   granting mid-incident.

Observability: `deleg_granted_total`; `deleg_refused_total{reason=
gate|grace|claim|share_want|write_open|rw_layout|no_cb|barrier|
cooldown|dup|quota}` (scoped per-client where rigs assert equalities);
`cb_recall_sent_total`; `cb_recall_outcome_total{acked|timeout|
refused|path_down|client_disowns}`; `delegreturn_total`;
`deleg_revoked_total{deadline|channel_dead|refused}`;
`delay_answered_total{site}`; `seq4_flag_raised_total{flag}`; gauges
`deleg_outstanding`, `files_under_recall`; histogram
`recall_latency_seconds` (first-transmit → RETURNED/REVOKED). The
grant/refusal split is the metering that says whether the fleet
workload actually re-accesses — required evidence before arguing
default-on.

Release sequencing: (a) wire/decoder fixes may land ahead of grants —
they fix latent decoder bugs and are inert; (b) never ship a release
where the encoder arm is live but DELEGRETURN decode is not (the
three-party chain lesson); (c) default-on requires: pynfs deleg legs
green on BOTH binaries, warm rig green with ALL anti-vacuity guards,
negative + restart/suspend legs green, idle-suspend integration
shipped, and one full regression sweep (pynfs floor-171, nfstest)
with the flag ON.

## 11. Implementation shape

**STATUS (2026-09-02, slice 5 + the last slice-3/4 code items):**
everything in this document that is CODE is now implemented and
unit-tested on macOS; what remains is the wire work in §9 (the
negative legs, the restart/suspend legs, the two-client conflict
matrix, the tier leg, nfstest, and pynfs flag-ON against the MDS
binary). In this increment:

- **Slice 5, MDS posture.** `FLINT_NFS_DELEGATIONS_PNFS` exists and
  gates grants in the MDS posture (refusals counted under `posture`,
  distinct from `gate`, so a rig against the MDS binary can tell the
  two apart). Grant rule 6 is live: `StateManager::
  write_layout_held_by_other` consults a probe the dispatcher installs
  in the same step as the posture (a constructor test pins the pair;
  no probe ⇒ the rule fails CLOSED). The index behind it,
  `LayoutManager::by_file_ident`, is keyed by the layout's
  `file_ident` — the truncate-gate key, which for every non-legacy pin
  is `id:<placement file_id>` and therefore follows the file through
  RENAME. That is the (dev,ino)-index requirement of §4/[V3] met by a
  different immutable identity: the placement's, which the MDS already
  keys stripes by. Legacy `path:` pins cannot be renamed at all (the op
  is refused), so the path-keyed residual the verifier worried about
  has no reachable instance. Every `layouts` insert/remove site
  maintains the index and a rebuild-and-compare test pins that.
  Conflict site 9 is fenced at LAYOUTGET for iomode RW **and ANY**,
  before the class dispatch (scsi included), answering
  **NFS4ERR_LAYOUTTRYLATER** with the guard held across the grant so
  the index sees the layout before any grant re-checks. LAYOUTCOMMIT
  and the proxied fallback WRITE — the two lanes the completeness gate
  had exempted "until slice 5" — are fenced (`layoutcommit`,
  `write_proxy`) and the gate now requires it. The MDS binary starts
  the §10 reporter. **DS client-behaviour pin:** a test drives the
  DS's real compound arm with a delegation-shaped stateid the DS has
  never seen and asserts it is ACCEPTED (with a garbage-fh
  anti-vacuity leg), so DS-side stateid validation cannot be added
  without first teaching the DS about delegation stateids.
- **Per-client recall batching (§5.4).** The FIRST transmit of a
  fence's orders is grouped per client and chunked to the client's
  back-channel `ca_maxoperations - 1`; one CB_COMPOUND carries the
  chunk, the reply is split positionally, and each record's ladder
  runs on from its own slice (a sibling's DELAY or BAD_STATEID is
  never yours; an op the compound stopped before is resent alone; a
  CB_SEQUENCE-level failure is everyone's). Rungs and deadlines stay
  per record. **Against Linux this is inert by construction** — it
  advertises back-channel maxops 2 (`NFS4_MAX_BACK_CHANNEL_OPS`), so
  the chunk is 1 and the wire is byte-identical to before; a test pins
  that. Metered as `batched +N` on the reporter line (compounds with
  >1 recall; zero against Linux is the honest number).
- **Concurrency stress (§9).** `state/deleg_stress_tests.rs`: 4
  readers × 2500 grant attempts racing 2 writers × 1000 OPEN-write →
  WRITE → CLOSE cycles over 8 files each reached by TWO filehandles,
  through the production funnel with the real stateid manager and a
  cooperative client thread returning recalls. Scored on the
  post-run invariant scan (`check_invariants`), a write-window probe
  from inside the writer (no Granted record while its write open is
  registered — through either alias), the new **release-time
  exclusivity assert armed on every proceeding guard** (a foreign
  non-revoked record at guard drop is a debug panic, and a
  should-panic test proves the assert is live), a granted floor, and
  a contention floor (a run with no writer DELAY never exercised an
  interleaving). First run: 1855 grants (921/934 by alias), 379
  writer DELAYs, 2000/2000 writer proceeds, 0 revocations, scan
  clean, 40ms.
- Also: `DelegationManager::set_cooldown` (rigs), the
  `granted_holders` probe, and a refusal reason `posture`.

**THE STRESS RIG EARNED ITS KEEP (2026-09-02, follow-up).** The Linux
suite — which is the suite; the macOS one compiles out every
`cfg(linux)` arm — failed the new stress leg on a 2-vCPU box with
`live_per_client[2] 1 != 0 live records`. A real defect in `try_grant`,
present since slice 1: the record was pushed under the entry lock, but
`by_stateid` / `by_client` / the live counters were written AFTER
releasing it. Every removal path reaches a record through `by_stateid`,
so that gap is a window in which a DELEGRETURN can run to completion —
and its `dec_live_client` finds no `live_per_client` entry yet and
silently does nothing, after which the grant's `+= 1` lands on a record
that no longer exists. Two consequences, both permanent: a phantom live
delegation that counts against the client's per-client cap for the life
of the server, and dangling `by_client` entries (the return's `retain`
runs before the grant's `push`) which grow without bound AND inflate
`count_for_client` — the busy check DESTROY_CLIENTID reads, so the
client can become undestroyable. `live_global` survives the identical
interleaving because `fetch_sub`/`fetch_add` on a `u64` commute, which
is exactly why only the per-client map ever showed it. Fixed by moving
the index and the accounting INSIDE the entry lock, atomic with the
record push.

The verification is the part worth keeping. The full stress rig hit it
ONCE, in-suite; 10 standalone runs and 20 more under CPU hogs never
reproduced it, so "the suite is green now" would have been worth
nothing. `a_grant_is_counted_before_it_becomes_findable` gets inside
the window deliberately — the granter publishes the id it is ABOUT to
mint, so the returner is already spinning on that id when the grant
opens the window; a returner that learns the id from the grant's return
value is by construction too late. **Control arm (old publish order):
12/12 FAIL, reproducing the suite's exact violation string plus the
dangling `by_client` entries. Fixed arm: 12/12 PASS, 60k grants and
8k-19k returner wins per run, ~3s.** An earlier cut of that rig minted ONE stateid
for every grant and manufactured index violations of its own —
`unindex` removes by key, so a reused id lets a return delete the NEXT
record's index. Production mints fresh ids; that was the rig lying
about the server, and a rig that manufactures its own violations cannot
testify about anything.

A second rig lesson, paid for the same day: the first working cut of it
ran 35s, because the granter's retry loop hit `AlreadyHolder` on most
iterations and answered each one with `yield_now()` — a syscall per
refusal. Two hot threads for 35s on a 2-vCPU box starved the suite
badly enough that an unrelated tier test began failing, and the
attribution experiment (run the suite with ONLY this test skipped: 2/2
clean, 40s faster) is the only reason that was not written off as
someone else's flake. Spinning instead of yielding was worse — the
granter monopolises its core, the returner never runs, and the run
collapses to ~10 grants, which "fails" the control for the wrong
reason. The shape that works: the granter clears the record ITSELF
after a short spin, so every iteration grants, no syscalls, the
returner still races the publish window, and 60k iterations take ~3s.
**A control arm that fails because a FLOOR tripped is not a control
arm.**

Same run, second Linux-only failure: the DS discard pin called
`Segment::as_mem()` on the READ reply. On Linux the DS answers READ
through the splice path (default ON since the splice workstream), so
the payload segment is a pipe and `as_mem` panics by design; macOS
compiles that arm out entirely, so the test had been passing there
while exercising a path the product does not take. It materializes with
`into_test_bytes` now, which also means the pin covers the splice lane
it is actually served from.

**STATUS (2026-09-01):** slices 0-2 SHIPPED (`2d913055` formal model,
`9f1fbdbb` wire fixes + foundations; validated macOS+Linux suites,
pynfs 171/0/91 both binaries, TLA 207/207). **Slice 3 IN PROGRESS**
(`454a03c4` + the grant increment): shipped so far — the reworked
(dev,ino)-keyed state core with RAII mutation guards; the §5.2 fence
funnel at every conflict site plus the EXECUTABLE completeness gate
(`every_f14_bump_lane_is_fenced_or_exempted`); CB_RECALL wire +
client-addressed sender with writer failover; the §5.4 recall ladder
(paused-clock suite: rungs, deadline-from-first-transmit,
CB_PATH_DOWN window with retry-as-rearm, disown re-probe); the §4
grant rules with wire delivery (OPEN_DELEGATE_READ + ace), refusal
counters, breaker (global + per-client quarantine, window
auto-reset) and sentinel; epoch-mixed unpersisted delegation
stateids; the DELEGRETURN / TEST_STATEID(DELEG_REVOKED) /
FREE_STATEID(tombstone + level-triggered SEQ4 lowering) loop —
grant→recall→DELAY→return and grant→revoke→free proven end-to-end at
the dispatcher level. §6 holder-evidence markers + load-time
SEQ4 pre-arm with delivery-tracked consumption (the V2-fatal restart
fix — a same-PVC roll now tells the holder its state is gone, so §10
kill layer 1 is safe by construction). **REMAINING for slice-3
complete:** event-driven rearm-on-rebind (the
ladder's retry loop approximates it within ~5s); rule 6's (dev,ino)
layout index (MDS posture refused wholesale until slice 5); metrics
export; per-client recall batching; expired-courtesy short-circuit;
breaker trip persistence.

Slices, in order:
0. **FlintDelegRecall formal module (§7) — gating, before slice 3.**
1. Wire/decoder fixes (inert, ship early): DELEGRETURN/DELEGPURGE
   decode, claim NOTSUPP arms, delegate_type threading, DelegCur
   decode+validation, OPENMODE check.
2. Foundations: (dev,ino) keying, `file_has_write_open`,
   (dev,ino) layout index, `callback_ready`, back-channel reap/
   iterate/rearm, seq_flags + SEQ4 surfacing, holder-evidence rows.
3. Grant + recall machine, dark: mutation_fence + guards, ladder,
   DELEGRETURN dispatch, cooldown, breaker+sentinel, metrics.
4. Rigs + baselines + idle integration ⇒ default-on (standalone hub).
5. MDS posture: layout ordering/rekey fixes + DS pin ⇒
   `FLINT_NFS_DELEGATIONS_PNFS`.

Size: Design 1 estimated ~1.6-2.1k production lines; the amendments
(guards, holder evidence, backchannel robustness, idle integration,
layout index) push this to roughly **2.5-3.5k production + ~2k test**
lines, 4-6 weeks single-person including the model and rigs. The
original 3-slice estimate predates the verification wave; do not quote
the smaller number.

## 12. Alternatives considered

- **Design 2 "Warmhold" (read+write, CB_GETATTR, 62 pts):** best
  elimination ceiling, eliminated on correctness risk and radius —
  write delegations carry the only data-loss failure shape in the
  field (dirty client cache lost on revocation/idle-suspend), CB_GETATTR
  serves holder-reported attributes that are not server truth, 5s
  in-server conflict parking, create-arm grants, ~4-5.5k lines over 4
  releases. Its Phase-B READ elimination is essentially identical to
  the winner for the target workloads. Write delegations remain a
  possible v3 ONLY with a persistence + flush-on-recall story.
- **Design 3 "health-gated" (75 pts):** closest loser, best ops story
  — its sentinel/drain/runbook/DS-flag/ca_maxoperations ideas are
  adopted as grafts. Lost on: CLAIM_PREVIOUS delegation re-grant
  during grace on unpersisted trust, tier-eviction delegation-pinning
  (caps bound count, not bytes — a delegation-heavy fleet can make
  the eviction watermark unreachable), stub-refusal surrendering
  first-open grants on exactly the tiered warm sets the feature
  targets, and DELEGPURGE⇒OK divergence from knfsd. Its grace-time
  re-grant idea is the queued v2 item (warm caches surviving
  restarts), behind field data and a trust-model argument.

## 13. Open questions (carry into implementation)

1. Self-conflict policy: DELEG23 + knfsd behavior — carve-out vs
   symmetric DELAY (one branch either way; policy lives in
   `mutation_fence`).
2. CLAIM_PREVIOUS delegation re-grant during grace (v2, from D3).
3. LAYOUTTRYLATER vs DELAY for site 9 — verify Linux pNFS client
   behavior on the lima rig. *(2026-09-02: implemented as
   LAYOUTTRYLATER, by codebase precedent — it is what the
   truncate-dirty gate already answers, and Linux's
   `pnfs_update_layout` retries TRYLATER for ~2 lease times before
   falling back to MDS I/O, whose WRITE lane is itself fenced, so
   both client paths converge on the recall. DELAY would make the
   client retry LAYOUTGET indefinitely instead of falling back;
   either is safe. Still to be OBSERVED on the rig, not just argued.)*
4. Per-client cap default (4096) — measure Linux client delegation
   cache pressure under the fleet rig; too low silently caps the win,
   too high inflates recall fan-out.
5. Wire `callback_ready` into `handle_layoutget` same release or
   follow-up? (Recommend follow-up; ~5 lines once the predicate
   exists.)
6. RENAME-source recall (v1 conservative) vs let-it-ride under
   (dev,ino) keying — measure whether target workloads rename hot
   read files before spending the v2 relaxation.
7. RFC 8881 section-number spot-check before quoting this doc's RFC
   citations externally (workflow digests flag them as best-effort);
   confirm plain OPEN_DELEGATE_NONE (vs NONE_EXT) is right for a
   server that ignores want bits.
8. Standalone-binary sweep cadence if `flint-nfs-server` ever fronts
   delegations at fleet scale (per-COMPOUND-only courtesy sweep).

Raw workflow artifacts (designs, judge scorecards, verifier reports):
session journal `wf_f2f28ed6-0d1`; this document is the durable
synthesis.
