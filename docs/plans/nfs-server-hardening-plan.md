# flint-lite NFS Server — Production-Hardening Plan (v2)

**Status: plan of record.** Written 2026-08-24 against HEAD `50e5cd8` (v1.37.0, "the
conformance release") by a 10-agent ultracode review: 4 code auditors → 3 track
designers (suites / concurrent load / failure behavior) → 3 adversarial critics.
Every blocker/high audit finding was independently re-verified in code (10/10
confirmed, 0 refuted); every proposed test leg was attacked for vacuity
(24 confirmed, 16 adjusted, 0 refuted — the adjustments are folded in below).

**Relationship to the 2026-08-23 plan.** That plan was believed lost when this
review started (it was never committed), but it survives — updated with wave-2
progress — at `~/Documents/nfs-server-hardening-plan.md` (76 legs, tracks
C/L/F/M, 6 done / 4 partial as of 2026-08-24, wave 2 on branch
`harden/blockers-2-4-6-7`: metadata fsync, extended dev-drift heal,
`FLINT_NFS_MAX_CONNECTIONS`, capability-trimmed root securityContext). This
document is an **independent at-HEAD re-derivation**; the two need reconciling
into one document (recommended: keep the 2026-08-23 plan's tracks, gates,
baseline format and §7 product questions as the skeleton; fold in §2's findings
— B1/B2 are new defects *introduced by* the wave the old plan records as ✅,
which its C14-B/F10 legs as designed would not catch — plus the novel legs
here: CL5–CL8 workload storms, F11–F13, G3/G7/G8/G9/G11/G14/G15, and the
suite-trim decisions). §1 records what is verified fixed at HEAD so nobody
re-litigates it.

**The goal:** flint lite moves to production shortly. The NFS server was written
from scratch; this plan is how we earn the right to ship it.

---

## 0. Ground rules (apply to every leg in this document)

1. **The product binary is `flint-pnfs-mds`** (`nfs_mds_main.rs` →
   `pnfs::mds::MetadataServer`), *not* `flint-nfs-server`. The two front-ends
   have already shipped drifted-recovery defects (v1.37.0 fixed three). Any
   number quoted as a gate must be measured against `make pnfs-mds-vm`
   (Makefile:228-247), or measured on both binaries with the binary recorded.
   First-triage runs on `nfs-server-vm` are fine; *gate* numbers are not.
2. **Linux, server as root, is the only quotable posture.** macOS-host numbers
   are rig-confounded (uid stamping, APFS telldir cookies). Cross-build per
   `reference_linux_test_crossbuild` or build in-VM.
3. **Anti-vacuity is mandatory.** Every leg must demonstrate its oracle *can*
   fail: a failing control (broken binary, knob off, injected fault), a
   calibrated checker (feed it a corrupted fixture), or a mechanism-of-pass
   check (wire status, direct sqlite read). History: 24 of 41 legs in one prior
   drill would have passed with the product broken; one shipped leg passed
   *because of* the bug it tested. Gate on **executed-count floors**, never on
   the absence of failures (a dead runner produces zero failures).
4. **Shell discipline:** redirect suite output to a file and read `$?` directly
   (`> log 2>&1`, never `2>&1 > log`; a pipeline's exit status is `tail`'s);
   `grep -c` never `grep -q` under pipefail; assert `findmnt -T <dir> -o FSTYPE`
   = `nfs4` before trusting any suite run (a silently failed mount tests ext4);
   log-grep oracles only in quiesced windows — the non-blocking tracing writer
   drops lines under load (`nfs_mds_main.rs:64`); `/status` counters are the
   truth under storm.
5. **No cluster provisioning without explicit approval.** Everything below is
   local ($0) except the marked ask-first cluster extensions.

---

## 1. Where HEAD (v1.37.0) actually stands — verified, do not re-audit

All verified in code at `50e5cd8`, per-binary on the MDS path. **Re-derived
from code at `0693f02e` on 2026-08-28** — the two rows that moved are marked;
everything else still reads as written:

| Area | Status | Evidence |
|---|---|---|
| Lock persistence on the hub (7abc0a5) | ✅ landed — `LockManager::bring_up` binds backend + restores + grace-gates (`pnfs/mds/server.rs:257`); anti-drift lint forbids bare `LockManager::new()` | but see the two **new** restart defects in §2.1 |
| Grace design (67f405a, c3bcee7) | ✅ sound — window always runs 90s from start; reclaimability a separate fact; `end_grace` has no production callers | residual: unverified reclaim grants during degraded grace (accepted) |
| F33 self-fencing (f5d98ff) | ✅ armed in `nfs_mds_main.rs:143-155`, exit 59, FIN-before-exit | positive fire never tested — leg G2 |
| Stateid/client persistence + CONFIRMED_R | ✅ correct (`stateid.rs:524-545` bumps counters on load; sessions deliberately not restored per RFC 8881 §15.1.10.4) | correctness here is what makes the lock-orphan bug dangerous |
| Evicted-file read-zero (tier blocker) | ✅ closed — pre+post marker consult, DELAY+park, hydrate-on-demand; flush unconditionally refuses marker-set files | |
| Generation-row dev-drift | ✅ merged — `heal_generation_device` at flush startup, insert-before-delete; wave 2 (branch) extends the heal to `tier_evicted` + `tier_dirty` keyed by inode with path repair (the destructive read-zero half) | no anti-shrink barrier at manifest publish — leg G13 |
| `gate::exclude` wedge (53ef50e) | ✅ intact — 30s deadline, backs out, metered | |
| Concurrent-PUT corruption | ✅ fixed + regression-pinned (pid+counter temp names; VERIFY+RENAME one compound) | |
| Wire-fed array-count amplification (51c658e) | ✅ uniform at all sibling sites via `checked_array_len` | READ *scalar* count not covered — §2.1 |
| CI truth (f71f696) | ✅ pynfs gate non-vacuous (floor 171 committed, `check-pynfs.py` refuses collapse/missing baseline); vacuous asserts gone | but the gate is manual-only — §7 |
| Conformance floors | pynfs **171/0/91**; nfstest_posix **459/2**; nfstest_lock **5296/0** (Linux-as-root) | |
| Namespace-op durability (DUR-1) | ✅ **landed** `10a9d97` — `v4/metadata_sync.rs`: parent-dir fsync on CREATE/REMOVE/RENAME/LINK/OPEN-create, on by default, fsync failure → NFS4ERR_IO. A source-scanning lint (`every_namespace_mutation_commits_before_it_is_acked`, 5 guarded call sites) fails the suite if a namespace op forgets `commit_parent_of`, and asserts its own anchors resolved so it cannot pass by not looking | but leg F4 is still **unwritten** — the durability claim is argued from code, never measured under power loss (§2.1 B11) |
| RPCSEC_GSS / Kerberos accept path | ✅ **hardened** `4742db9` — four defects, three security-relevant, found by the first drill that asked whether an *incorrect* client is refused: the window-reset arm left its own new high-water number unmarked (replayable); `RPCSEC_GSS_MAXSEQ` unenforced; the ticket's `starttime`/`endtime` parsed and never read, so an expired ticket authenticated forever and a KDC revocation stopped nobody; and `validate_data` spent a sequence number *before* the MIC check (RFC 2203 §5.3.3.1 orders it the other way), letting a **keyless** peer park an established context's replay window from the wire | `tests/krb5/run-gssneg.sh` 27/27 against a real KDC and the kernel client; the pre-fix RED run committed beside it (`results/gssneg-prefix-2026-08-28.log`). Three oracles were wrong first, each passing against the buggy server |

---

## 2. Pre-GA burn-down — code findings, each fix lands with a pinned regression leg

### 2.1 Fix before GA

**Burn-down at `0693f02e` (2026-08-28): 9 closed, 2 fix-without-leg, 1 open.**
Status re-derived from code and `git log -S`, not from notes — B7 was still
being carried as the next item to start when its fix had already shipped. B9
and B11 have their *fix* but not their *leg*, which is the distinction this
column exists to keep visible; B12 alone is untouched.

| # | Finding | Severity | Fix shape | Pinned leg | Status |
|---|---|---|---|---|---|
| B1 | **MDS-LOCK-ENTRYKEY-COLLISION** — `entry_key_counter` restarts at 1 and `load_records` never bumps it (`lockops.rs:254,267,277-296`; the one counter that skips the pattern `stateid.rs:524-545` follows). Post-restart mints collide with restored rows and silently overwrite them in memory **and** in sqlite (`INSERT OR REPLACE`) — mutual exclusion lost again for pre-restart holders; the exact class 7abc0a5 shipped to close, reintroduced by its own restore path. Invisible to pynfs/nfstest (no restart); existing tests use synthetic keys. | **blocker** | Bump the counter past max restored key in `load_records`; salt minted keys with the persisted instance counter (`server.rs:562-566`). | F2/F6-shape generation test: restore production-minted rows, mint to collision, restored lock must still deny + exist. | ✅ **closed** `48de6d1` — `load_records` bumps the counter past the max restored key; leg `restored_entry_keys_are_never_reminted` |
| B2 | **MDS-LOCK-CANONICAL-ORPHAN** — the client-visible lock stateid→owner map is memory-only (one registration site, `lockops.rs:920-931`); after ANY restart the holder's LOCKU/exist-owner LOCK → BAD_STATEID while TEST_STATEID says Ok and FREE_STATEID says LOCKS_HELD (non-converging); restored rows deny the range to everyone until the holder's lease lapses. Post-restart sqlite wedge on the flagship workload. Also: the reclaim arm replies with the *internal* 0xFC stateid (`lockops.rs:818-831`), which nothing can validate later. | **high** | Persist owner bytes/discriminator in `LockRecord` (one schema touch, do together with B1); reclaim arm must return a canonical stateid. | Restart e2e: LOCK → restart → LOCKU via canonical must succeed. | ✅ **closed** `48de6d1` (same schema touch as B1) — legs `canonical_lock_stateid_survives_a_restart`, `locku_succeeds_via_the_canonical_stateid_after_a_restart`, `reclaim_replies_the_canonical_stateid_not_the_internal_key` |
| B3 | **DUR-2 write-verifier seconds granularity** (`ioops.rs:263-271`) — a sub-second supervised restart re-mints an identical verifier; clients silently discard uncommitted UNSTABLE data. No wire anomaly to observe. | high (silent loss) | Mint from nanos, or XOR the persisted per-boot instance counter. | F1 fast-restart arm: post-restart verifier ≠ pre-kill verifier as a hard assert (tcpdump COMMIT replies). | ✅ **closed** `81d6541` — leg `write_verifier_differs_across_incarnations_and_holds_within_one` |
| B4 | **DUR-5 `tier_unacked` coverage hole** (`dispatcher.rs:745-756`) — CREATE/REMOVE/RENAME/LINK queue capture/identity events but their results can't be doctored on a failed durable drain; an OK escapes → never-flushed file / resurrected generation on import. The guard's own comment believes this lane can't fire; it can. | medium (silent divergence) | Add the four match arms; fix the comment. | Unit: failed drain doctors a CREATE result. | ✅ **closed** `e89a4ad` — the four arms added and the comment that denied the lane corrected; leg `tier_unacked_doctors_the_namespace_results_too` |
| B5 | **SEQUENCE slot reply cache unbounded** — every reply cached unconditionally regardless of `cachethis` (`operations/session.rs:867-873`), no size check vs `ca_maxresponsesize_cached`; up to slots(≤128)×1 MiB ≈ **128 MiB per session from normal traffic**, sessions uncapped. Dwarfs the per-connection math; blocker-class for fleet memory sizing. | **blocker** (memory) | Cap cached-reply size (honor `ca_maxresponsesize_cached`), honor the hint, cap per-session total. | CL9 memory model must include slot cache; assert bounded. | ✅ **closed** `996a929` — legs `oversize_cached_reply_is_refused_not_pinned`, `create_session_clamps_the_cached_window_and_replays_it_identically`, `uncached_replay_gets_retry_uncached_rep` |
| B6 | **No quota on NFS state tables** — any TCP-reachable peer mints unbounded clients/sessions/stateids/lock rows (memory + state.db growth on the PVC). | **blocker** (DoS) | Per-client and global caps with a clean NFS4ERR_RESOURCE refusal. | CL11 flood asserts the cap engages. | ✅ **closed** `66474e8` — per-client and global caps refusing with DELAY; a leg at each of the four mint sites (`exchange_id_quota_…`, `create_session_quota_…`, `open_refuses_with_delay_at_the_stateid_quota`, `lock_quota_refuses_at_cap_…`) |
| B7 | **DOS-2 no idle/read timeout** (`server_v4.rs:375,428`) — slowloris/idle conns pin tasks + ~384 KiB each forever; no credential needed. | high | Idle-read timeout (env-tunable) + zero-RPC reap window. | CL10 records reap behavior. | ✅ **closed** `56e8d2c` — `FLINT_NFS_IDLE_TIMEOUT_SECS`, default 360s, `0` opts out; module `idle_timeout_tests`. This is the cap's other half: without it B6's connection slots were held forever |
| B8 | **DOS-4 READ count unclamped** (`compound.rs:1369-1373`, `ioops.rs:1371`) — ~100-byte frame with `count=0xFFFFFFFF` allocates min(4 GiB, file size) *before* the response-size gate; multi-GB model weights make this real. Same review for READ_PLUS (`dispatcher.rs:1877`). | high | Clamp to negotiated `ca_maxresponsesize` before allocating. | Unit + G1 fuzz corpus entry. | ✅ **closed** `aad7a47` — clamped to the negotiated ceiling *before* the allocation; leg `read_count_is_clamped_to_the_response_ceiling` |
| B9 | **DOS-3 shippable subset** — hub container is uid 0 with no `securityContext` (`Dockerfile.pnfs.prebuilt`, `hub.yaml`); AUTH_SYS trusts wire uid 0, no squash, no in-process authz; chart NetworkPolicy defaults off and is documented-vacuous cross-cluster. **Wave 2 (branch) largely lands this** — and refutes the naive fix: the hub *cannot* be non-root (chown-to-arbitrary-owner, cross-uid file access, setuid preservation); the branch ships explicit uid 0 with caps cut to CHOWN/DAC_OVERRIDE/FOWNER/FSETID + no-privilege-escalation + seccomp, applied in **both** renderers (chart and `lite_operator/render.rs`). | high | Land the branch's securityContext; **document** sec=sys ⇒ network-isolated, single-tenant (L3/L4, not the in-cluster NetworkPolicy). Squash/allow-list = post-GA. Note `FLINT_FH_KERNEL=1` needs CAP_DAC_READ_SEARCH the trimmed set does not grant. | G8 AUTH_SYS edge leg; a live pod started under the trimmed caps (never yet done) + the pjdfstest ownership subset behaving identically to the root run. | 🔶 **fix without leg** `b52a423` — explicit uid 0, caps cut to CHOWN/DAC_OVERRIDE/FOWNER/FSETID, no-privilege-escalation, seccomp `RuntimeDefault`, in **both** renderers, pinned by `parity_fixture_matches_the_current_chart`. The empirical half of the pinned leg is still unrun: no drill starts a live pod under the trimmed caps, and pjdfstest's ownership subset has never been run under them |
| B10 | **EXCHANGE_ID capability clamp** — the hub echoes SUPP_MOVED_REFER/MIGR back to any requester (`operations/session.rs:341-347`) while implementing neither; advertises never-tested client recovery paths. | medium | One-line clamp to implemented flags. | pynfs gate wire assert: reply never carries MOVED flags. | ✅ **closed** `bc7fed3` — leg `exchange_id_reply_never_advertises_unimplemented_capabilities` |
| B11 | **DUR-1 namespace fsync** — landed in wave 2 (branch): `metadata_sync.rs`, parent-dir fsync on CREATE/REMOVE/RENAME/LINK/OPEN-create, on by default, fsync failure → NFS4ERR_IO; measured cost ~1.9× on a pure-create loop (~0.6 ms/create, the worst case). Merge it. Note: change_info is a fabricated constant on namespace ops (`fileops.rs:3258-3262`) — clients cannot detect a rollback; the change-attr probe (S-P below) is the compensating gate. | high | Merge the branch. | F4 power-loss leg with knob-off failing control — the durability claim is still argued from code, not yet measured. | 🔶 **fix without leg** `10a9d97` — see §1. Leg F4 (power loss, with a knob-off failing control) is unwritten; this is the one shipped durability claim with no measurement behind it |
| B12 | **Prefix-reuse tenant adoption** — open at HEAD: `tier/manifest.rs` carries no owner/claim identity; delete share A, create share B on the same prefix ⇒ B serves A's files, no race required. | high (data exposure) | Stamp claim identity into manifest/epoch cell and refuse silent adoption, or enforce prefix retirement at the operator front door. | G12: create/tear-down/re-create leg asserting prior data NOT readable. | ❌ **open at HEAD** — `tier/manifest.rs` still carries no owner/claim identity, so the finding stands exactly as written. Blocked on a design call: stamp claim identity into the manifest/epoch cell, or retire prefixes at the operator front door. **Do not start code before that call** |

### 2.2 Deferred, with rationale (decide explicitly, don't drift)

- **DOS-1 global connection/dispatch caps** — the *connection* half landed in
  wave 2 (branch): `FLINT_NFS_MAX_CONNECTIONS` default 1024, RAII slot guard,
  refusal-is-a-close, with a two-arm differential drill
  (`conn-cap-drill.sh`: cap=2 → HELD=2/REFUSED=2, cap=0 control → 4/0, agree ⇒
  VOID). The *global in-flight dispatch* cap remains open — size it from CL9's
  measured number; a guessed cap is worse than none. Chart docs get the
  per-connection memory math (DOS-7).
- **MDS-CLIENT-REMOVAL-ASYMMETRY** (DESTROY_CLIENTID / EXCHANGE_ID cases 3,9-alt
  retire a client without the lock cascade → unreapable phantom rows) — narrow
  reachability in the sec=sys fleet profile; route through the case-5 cascade
  when touched next.
- **MARKER_CYCLE narrowing** — availability-only, needs the model
  (temp+rename changes the inode). Watch `EvictedOpDelays` in production.
- **DUR-3 ms-scale lock-ack window** — accepted as designed (documented);
  refuse `FLINT_PNFS_STATE_SYNC=normal` when a tier is configured.
- **fileapi PUT full-buffer (DOS-6)** — token-gated, single-tenant posture;
  stream-above-threshold like the GET fix when the file API grows users.

### 2.3 Observability is a release **blocker**, not a recommendation

Before GA the hub must expose (on the existing health listener): connection
count + in-flight dispatch (today log-only statics, `server_v4.rs:228`), per-op
counters, `EvictedOpDelays`/`HydrationFailures`, `state=memory-fallback` +
`state_lost` flags, lock/stateid/client table sizes, dropped-log-lines counter.
Ship alert rules with the chart: memory-fallback≠0, sustained HydrationFailures,
fence exit-59/crash-loop, RSS slope, active_leases==0 on a live share. Flip
`monitoring.enabled` default for production profiles (`values.yaml:163`).
Every drill below leans on `/status` — production gets the same truth.

---

## 3. Track S — conformance suites beyond pynfs + nfstest_posix/lock

pynfs's **entire NFSv4.2 surface is 4 tests**; nfstest has 15 unused tools.
Vacuity corrections applied to every entry (§0 rules 1, 3, 4 are load-bearing
here — this track carried the systemic vacuity risk).

**Must-run (in yield-per-hour order):**

| # | Suite | What it adds | Key corrections |
|---|---|---|---|
| S1 | **xfstests `generic/`** over NFS (`./check -nfs -g quick`, then full) — `/opt/xfstests` already built in the VM | Cross-op *semantic* interactions through the real kernel client: mmap+truncate, clone+write, dio/buffered mixing, unlink-while-open, fsync ordering, ENOSPC, readdir under churn — the canonical from-scratch-server bug mill | Gate on **pnfs-mds-vm**; require an executed floor (≥300 in `-g quick` post-exclusions) — below floor = VOID; `findmnt` nfs4 assert before/after; curated exclude list (generic/035/423/465 fail even against knfsd); two-subtree exports + `nosharecache` for TEST/SCRATCH |
| S2 | **nfstest 4.2 trio: `nfstest_alloc` + `nfstest_sparse` + `nfstest_ssc`** (zero install) | 40+ tests on ALLOCATE/DEALLOCATE/SEEK/intra-server COPY incl. negative wire decode — the exact decoder class that shipped the mkdir bug | Parse PASS/FAIL/total with per-tool floors (alloc 13, ssc 15, sparse full); require SEEK/ALLOCATE on the wire in nfstest's own trace (zero wire ops = VOID — SEEK-NOTSUPP fallback is POSIX-legal and tests nothing) |
| S3 | **cthon04** (basic/general/special/lock; ~15 min once built) | `special/` is a curated list of exactly what from-scratch servers ship: telldir cookies across deletes, unlink/rename-while-open, replay idempotency through the session DRC (per-binary state — run on the MDS), multi-process blocking locks | Count per-section completion banners (one aggregate exit hides skipped sections); groff present or general/ legs are VOID not skipped; `findmnt` assert; loop `-s` ×10 |
| S4 | **Real-workload loop**: untar+`tar --compare`, git commit loop+`git fsck --strict`, sqlite | The category that found 3+ shipped bugs; closest proxy to the agent-fleet workload | **The drafted sqlite arm was itself vacuous** (INSERT into a table never created; integrity_check passes on an empty db): CREATE TABLE first, assert `journal_mode` output == `wal` where claimed, exact row-count from a fresh client, per-step exit capture. WAL = single-host multi-process only (sqlite's own rule); cross-client uses rollback journal via the netns second client. Run on the MDS. |

**Should-run:** S5 **pjdfstest** (~8800 error-code/permission checks — SUID-clear,
sticky-bit rename; exclude mkfifo/mknod: flint's stand-ins are by design; gate on
a parsed ok-count floor ≥5000, freeze the exclude baseline in-repo);
S6 **fsx/fsstress long-run** on BOTH binaries (leg B = MDS with standalone-banner
+ zero-LAYOUTGET lane proof; buffered fsx must `drop_caches`/remount periodically
or it validates the client cache against itself); S7 **fio --verify** (direct=1
arm honest as-is; buffered arm: write, drop_caches, separate `--verify_only`
pass); S8 **nfstest_io** (locks interleaved with data+namespace; timeout-as-FAIL
+ mountstats op floor).

**Optional:** LTP `nfsv4/locks` locktests (cross-client arm is the gate — 50-proc
contention against the just-persisted LockManager).

**Trimmed by the completeness critic (do not spend rig-hours):**
- `nfstest_delegation` on-arm: delegations are OFF in the product
  (`FLINT_NFS_DELEGATIONS`, OPEN hardcodes DELEGATE_NONE). Keep only the cheap
  **off-arm wire assert** (OPEN storm, every reply's delegation == NONE) per
  release; run the on-arm only if the gate is ever proposed for flipping.
- `nfstest_cache`: replace with the **direct change-attribute probe** (S-P):
  for each mutation class (WRITE, SETATTR, CREATE/REMOVE/RENAME-in-parent,
  LINK), GETATTR the change attr from a *second* client before/after, assert
  strict increase. Doubly needed: wire change_info on namespace ops is a
  fabricated constant, so GETATTR change is the *only* cross-client
  invalidation signal the product has. Per-release gate.
- `nfstest_dio`, LTP fs suite: skip for gating (marginal yield / broken oracle).
- `nfstest_interop` (no v3 in the tree), `nfstest_pnfs`/`nfstest_rdma`
  (lite never grants layouts — a lite run passes vacuously): skip.

---

## 4. Track CL — concurrent load (the 10s-of-connections question)

Rig (proven mechanism, reuse): Lima VM resized to 8cpu/8GiB/40GiB; MDS cross-built,
run in-VM as root with `state.backend=sqlite` (lite.yaml ships `memory` — load
must exercise the production persist path), `monitoring.health.enabled=true`,
4 pinned worker threads. **N distinct clients on one VM:** loopback alias
`127.0.1.$i` + rewrite `/sys/module/nfs/parameters/nfs4_unique_id` before each
mount (serialized — the parameter is module-global). Metrics sidecar: VmRSS, fd
count, `ss -tn` to :20490, `/status` activity counters + `active_leases` before/
after every leg. **Loopback numbers are ratios/equalities/slopes only — never a
MiB/s headline** (the client kernel walls ~3000 MiB/s regardless; the rig drifts
~2x within a session).

| Leg | What it does | Oracle (hard) | Anti-vacuity guard |
|---|---|---|---|
| CL1 | Client ladder 1/4/16/32/64, distinctness gate for the whole track | `active_leases == N` exactly; cross-client read of another client's file | **Collapse arm first**: 4 mounts *without* unique_id rewrite must read < 4, or the oracle can't tell clients from connections and the track is VOID |
| CL2 | Metadata storm — N× parallel untar (20k files) + rm | zero errors; completeness census `find -type f | wc -l` == 20k **from a different client** (or post-drop_caches — same-client dcache can mask server loss); fairness ≤3–5x; fd/RSS return to baseline | `namespace_ops` delta ≥ N×20k (below = load never reached the server = VOID); calibration kill of ¼ of the untars must read short. **Add**: one 500k-entry *single directory* + full READDIR from a second client under concurrent create/remove — agent fleets produce exactly this shape and the telldir path just changed |
| CL3 | nconnect 1/2/4/8 × 8 clients | server-side conn count == 8×nconnect and **must move** across settings; mountstats xprt lines == nconnect; zero errors at 64 conns | 10% perf bound demoted to informational (rig drift ≫ 10%); structural oracles are the gate |
| CL4 | Data-streaming fairness N∈{4,16,32}, interleaved rung order + end-of-session re-run of rung 1 (drift measurement) | fairness ≥25% of mean; server-side `du` byte census; `data_ops` delta | rate-limited-client calibration must trip the fairness metric |
| CL5 | **Lock storm** — contended increment loop on one file + disjoint-range arm; state.db growth watched | final counter == N×500 **exactly** (mutual exclusion is the oracle); state.db growth <10 MiB (monotonic growth = persistence leak) | **No-lock arm must lose updates** (add `stress -c 4` if loopback won't — the conditional-writes drill proved the loss is load-dependent); zero loss in the no-lock arm = no real concurrency = VOID |
| CL6 | sqlite multi-writer, rollback journal, N∈{4,8,16}, actimeo arms | integrity_check ok + row count == N×200 exact from a fresh client; zero app-visible BUSY | corrupted-copy must fail the checker; kill-a-writer must read short; `namespace_ops` delta > 0 (sqlite's fcntl locks must traverse the wire) |
| CL7 | git concurrent push/commit, one bare repo | fsck clean; branch == N×10 commits exactly (silent ref loss is the failure) | truncated-object fixture must fail fsck; `mount | grep -c nfs4` == N (first-commit-never-worked precedent) |
| CL8 | fileapi PUT storm (32 distinct + 32 same-path races) × NFS load, If-Match arm | every final file byte-identical to exactly ONE complete payload; readers never see a partial; stale etag ⇒ 412; NFS p99 ≤ 3x baseline under storm | torn-file fixture must be flagged; **the HTTP client harness is the census** (64 issued/64 answered — server logs are corroboration only, they drop under load); assert `fileApi.routesMounted` (no token ⇒ routes never mounted ⇒ 404) |
| CL9 | **Permit/memory probe** — `FLINT_NFS_MAX_INFLIGHT=4` vs 64 vs 64@32-clients; produces the number the chart lacks | arm A completes (permits = flow control, not deadlock); peak RSS within model; **deliverable: MiB-per-heavy-client for the chart** — must now include the B5 slot-cache term | arms A and B must *separate* (≥2x delta) or the permits never filled and "bounded" is vacuous; A's mountstats backlog > B's; use direct=1 |
| CL10 | Pathological clients: never-reads-replies, 1 B/s trickle, silent connects — beside healthy load | healthy p99 ≤ 2x baseline in every phase; accept loop stays live (new client can mount *during* pathology); teardown returns fd/RSS to baseline; reap behavior recorded | engagement proof **restated** (the drafted ">64 accepted" is unsatisfiable under the 64-permit cap): pathological conn at its permit cap + `ss -tni` peer at zero window + full send-q; fd count must rise by exactly the pathological socket count |
| CL11 | Mount flood — 500 cycles / 16 identities / 8 concurrent + `umount -f` arm | CLOSE_WAIT == 0 after settling; fd within +32; `active_leases` returns to live count; state.db not monotonic | fd/leases must be observed HIGH before the fall counts; held half-open socket calibrates the CLOSE_WAIT detector; ≥400 distinct identities seen server-side (else the kernel deduplicated and the flood never happened) |
| CL12 | Soak + cluster arm. **Pre-GA: one 48–72 h soak** (16 clients, hourly workload cycle, mid-soak clean restart + one kill -9 restart, log-rotation/disk-slope oracles); 6 h version becomes the recurring per-release soak. Arm 2: kind (ONE control plane, ≥8 GiB Docker VM), 24 client pods with distinct hostnames via ClusterIP; re-run CL1-collapse, CL5, CL8 at N=24; record hub usage vs its 100m/128Mi requests | RSS slope ≤1 MiB/h over the tail; hour-N p99 ≤ 2x hour-1; leases exact at idle checks | per-hour heartbeat with executed-op floors cross-checked against `/status` — a dead-generator hour is VOID, never PASS; in-cluster collapse arm re-proven FIRST (same-hostname pods must collapse to 1 lease) |

AWS multi-node arm: designed only; plan + cost presented; provisioned strictly
on explicit approval (standing gate).

---

## 5. Track F — failure behavior (the hub-crash question)

Rig A (process-level): hub in a Lima **qemu** VM (raw disk — required for the
power-loss leg), ext4, sqlite state; two client identities via
`unshare --net --uts` sandboxes with **mandatory pre-flight**: tcpdump both
EXCHANGE_IDs, assert distinct co_ownerids before any two-client leg counts.
`FLINT_NFS_GRACE_SECS` shrinks grace for iteration; one confirming pass at 90 s
per grace leg. Partitions: `iptables -j DROP` both directions + `conntrack -F`.
**Control binary: keep v1.36.0 (pre-7abc0a5) built** — the failing control for
F2/F6. Rig B (k8s semantics): kind + charts + MinIO.

| Leg | Scenario | Sharpest oracle | Guard |
|---|---|---|---|
| F1 | kill -9 mid-WRITE/COMMIT storm, 2 clients, ×5 random phases; **plus the fast-restart arm hunting B3**: supervised restart <1 s | every client-logged ACKED record bit-exact after recovery; stall time per client; **post-restart verifier ≠ pre-kill verifier as a hard assert** — a match is the silent-loss bug, fail loudly | ≥1 in-flight un-ACKed record at kill time or the run is void; truncate-an-ACKED-record calibration must be flagged |
| F2 | kill -9 at LOCK-grant +δ, δ swept 0–50 ms, ×50 — the enqueue_write crash window measured | loss-rate-vs-δ curve; wherever the sqlite row survived, C2 MUST be denied | **v1.36.0 control: C2 must be granted every iteration** (proves the oracle can fail); direct `sqlite3` row count decouples "denied" from "persisted" |
| F3 | kill -9 mid-RENAME carousel (sessions deliberately not restored ⇒ replay protection dies with the process) | every ACKed rename applied exactly once (never both names, never neither); straddled op = clean success or one clean error | checker calibrated against both synthetic failure states; tcpdump must show ≥1 rename straddling the kill |
| F4 | **Node power loss** (hypervisor kill of the hub VM) under namespace + data storm — verifies the in-flight B11/DUR-1 fix at the only honest layer | zero ACKed namespace ops missing; zero ACKed bytes wrong; state.db opens without quarantine (synchronous=FULL held); recovery time decomposed | **the control IS the guard**: `FLINT_NFS_METADATA_FSYNC=0` under the same storm+kill MUST lose ACKed ops — tune the rig until the control fails, then run the main arm |
| F5 | state.db corrupted at restart, live lock holders | during grace C2 denied with wire status **NFS4ERR_GRACE** (not a coincidental conflict); C1 reclaims; after grace C2 proceeds | intact-DB control must NOT gate (proves the gate isn't permanently on) |
| F6 | Double crash during grace | both incarnations log "restored N" with the SAME N, corroborated by direct sqlite counts at both boots; persisted reclaim_complete edge documented | pre-7abc0a5 binary as failing control |
| F7 | Partition past lease expiry, then heal | C2 gets the lock only after expiry; byte-level audit shows no C1 write lands after C2's grant (post-heal writes on the dead stateid refused) | spliced-fixture calibrates the interleave checker; C2's log must show denials BEFORE expiry |
| F8 | Disk-full mid-write + lock churn during the full window | clean ENOSPC, never a lost ACKed byte; COMMIT during full returns NOSPC; granted-while-full locks missing from the table = the measured exposure (enqueue_write errors are logged-only); no fence exit-59 (fence is for vanish, not full); tiered arm refuses at reserve | direct sqlite count DURING the window; churn must provably overlap the full window |
| F9 | S3 unreachable mid-hydrate while clients read evicted stubs | every read eventually bit-exact vs pre-evict sha; wire shows only DELAY during the outage — never OK-with-zeros; mid-GET kill leaves truncate-back | zero-read control **must bounce the hub** after deleting the marker row (the in-memory evict set otherwise defeats it — a control that still DELAYs is control-invalid, not evidence of safety) |
| F10 | kind: pod delete, config-roll bounce, worker-node stop — all mid-load | client stalls then resumes, zero app errors, sqlite integrity ok; `/status` podName changed + serverId UNCHANGED; node-death arm documents the kind-vs-cloud PVC gap | ops provably in flight at each disruption; wipe-PVC control proves serverId oracle falsifiable. F29/PVC-follow = ask-first cluster extension (~$5-10) |
| F11 | Idle-suspend racing a live mount; both `suspendWithSessions` arms + scale-to-zero write race | (a) default suspends despite live leases — measured hang + recovery; (b) `false` holds through 3× the window *and* still suspends after true idleness; (c) every pre-SIGTERM-ACKed write survives resume; clean shutdown ⇒ instant epoch claim | arm (a) is the failing control for arm (b); kill-at-scale-down control must show the epoch HELD (~60 s successor wait) |
| F12 | SIGKILL at the termination-grace deadline with 2 GiB dirty tier | epoch cell HELD (read via `mc`, log-independent) during the gap; successor waits the lease then serves; bucket catches up; no behind-state publish; wake-time number → runbook | clean-SIGTERM contrast must show RELEASED + instant claim — identical wake times void the leg |
| F13 | **Quarantine-recreate fails ⇒ silent memory-backend fallback, then a second bounce** (`state_backend/mod.rs:317-323`; fresh random server_id per boot) — shipped-bug candidate found during design | first boot serves degraded with grace gating + serverId changed; second bounce characterized (expected ESTALE/BAD_STATEID storm under live mounts) — file at observed severity | serverId oracle valid only because F10 proved both directions; no-mtime-change corroborates the fallback through the filesystem |

---

## 6. Track G — gap legs (from the adversarial critics; none existed in any track)

| Leg | Gap |
|---|---|
| G1 | **Malformed-XDR / structure-aware fuzz** against `flint-pnfs-mds` — the decoder is the proven bug mill yet every leg sends valid frames. Truncated fragments, oversized fragment lengths, absurd READDIR dircount/maxcount, garbage op bodies ⇒ GARBAGE_ARGS/BADXDR without panic/leak/pool damage. RSS/fd-monitored. **Highest-value absent leg.** |
| G2 | **F33 positive fire**: make the backing store actually vanish under load; watchdog fires, exit 59, clients get FINs and recover instead of hanging. |
| G3 | READDIR cursor held **across a restart** (kill -9 and clean): read half a large dir, bounce, resume the cursor — cookie+verifier semantics on the persisted path. |
| G4 | File API × tier states: HTTP GET/PUT on an **evicted** file; during an S3 outage; during the pre-serving import window (503 routes). |
| G5 | **Active two-hub epoch race**: second hub against the same bucket while the first is healthy; loser fences on heartbeat CAS 412; neither publishes over the other. |
| G6 | Backing-device partial **EIO** (dm-flakey): per-op NFS4ERR_IO, never a panic, wedge, or false ACK — between F8's ENOSPC and G2's vanish. |
| G7 | **Clock discontinuity** (NTP step, VM pause/resume): leases neither mass-expire nor immortalize; epoch heartbeat doesn't self-fence. |
| G8 | **AUTH_SYS credential edges**: >16 supplementary groups (AUTH_SYS truncation), non-root uid permission checks — everything today runs root-on-root and skirts this. |
| G9 | **Adversarial filenames on the wire**: invalid UTF-8, embedded `/`, `.`/`..`, 255/256-byte names ⇒ BADNAME/NAMETOOLONG, never mangling or aliasing (classic Rust OsStr/String bug site). |
| G10 | **fd-cache eviction under active I/O**: hold more concurrently-OPEN files than the FdCache bound with reads in flight against evicted fds. |
| G11 | **Hibernate cycle with tier under a live mount**: PVC deleted, namespace rebuilt from bucket on wake (F11 stops at suspend/resume). |
| G12 | **Prefix-reuse adoption** (B12's leg): share A publishes, tear down, share B on same prefix — A's data must NOT be readable. Today it is. |
| G13 | **DR verification cadence**: anti-shrink barrier at manifest publish (refuse/alarm an entry-count collapse without matching tombstones — the dev-drift bug shrank 37→4 silently); recurring restore-verify (import latest manifest to scratch, compare census vs live registry). |
| G14 | **Client interop matrix** (per-release): CL1 distinctness + S4 loop on the fleet's real kernels (Ubuntu 22.04/5.15, 24.04/6.8, newest fleet kernel) under hard AND `soft,timeo=,retrans=` mounts — soft mounts convert parked hydration DELAYs into app EIO mid-transaction; today every track mounts hard only. Deliverable: documented supported mount-option envelope in the fleet guide. |
| G15 | **Upgrade/rollback under live mounts** (per-release): helm prev→candidate with old state.db read by new (locks/stateids/tier rows restore), then rollback against the newer db. At HEAD a schema_version mismatch is a hard refusal whose message says "delete the state file" — on a tier-ON hub that advice is the DANGEROUS action (recreating republishes emptiness); any SCHEMA_VERSION bump makes rollback a crash-loop. Fix the error text (tier-ON remedy = restore-from-bucket); document the rollback window + version-skip rules (the 1.26/1.27 must-skip precedent). |

---

## 7. Release gating matrix — what found the bugs must gate the releases

The recurring failure here (T6): the suites that find the bugs never gate a
merge. Encode enforcement explicitly:

| Tier | Contents | Where |
|---|---|---|
| Per-merge CI | `cargo test --lib --tests` + every pinned regression test from §2.1 fixes | rust-ci.yml (exists) |
| **Per-release BLOCKING** — run against `flint-pnfs-mds` via `pnfs-mds-vm`, recorded in the tag checklist | pynfs ≥171 floor (`check-pynfs.py`), nfstest posix/lock floors, cthon (15 min), S4 workload loop, change-attr probe S-P, delegation off-arm assert, EXCHANGE_ID MOVED-flags assert, restart legs F1/F2/F5, G14 interop subset, G15 upgrade leg, 6 h soak | new `make release-gate` target + checklist doc — not memory |
| Scheduled (weekly/nightly) | xfstests quick w/ frozen exclude list + executed floor, CL5/CL6 storms, CL11 flood, G1 fuzz | Lima runner |
| One-time pre-GA, results recorded | F4 power loss, 48–72 h soak, CL9/CL10 (numbers → chart docs), CL12 kind arm, pjdfstest baseline, G2/G5/G6/G7/G13, B12/G12 | this campaign |

---

## 8. Deliverable docs (gaps — none exist today)

1. **`docs/flint-lite-runbook.md`** — symptom→signal→action: corrupt state.db
   per posture (bookkeeping auto-quarantines; **tier-ON must restore-from-bucket,
   never delete**), memory-fallback mode (serverId churn is the tell), epoch-held
   slow wake (~60 s after SIGKILL vs instant after SIGTERM — F12's numbers), F29
   restage recovery (suspend→resume; csi-node restart does NOT work), hydration/
   S3-outage triage via the meters, ENOSPC/ballast, grace semantics after unclean
   restart (90 s new-lock refusal is expected, not an outage), fence exit-59,
   verified mount command + PV-needs-ClusterIP trap.
2. **Capacity/limits page** — sourced from CL9/CL2/CL5/CL12 numbers: max tested
   clients, per-connection + per-session memory model (incl. B5), 5 GiB fileapi
   PUT buffering, state.db growth rates, and the explicit list of currently
   UNBOUNDED surfaces until §2 fixes land.
3. Fleet-guide updates: mount-option envelope (hard required; nconnect/actimeo
   ranges), sec=sys network-isolation requirement, single-tenant statement.

---

## 9. Sequencing and effort

Phase 1 (parallel with §2 fixes): S1–S4 bring-up + first triage, CL1 rig, F1–F3.
Phase 2: remaining CL ladder, F4–F9, G1–G3. Phase 3: kind legs (F10–F12, CL12
arm 2), remaining G legs, 48–72 h soak, gating matrix + docs. Rough effort:
§2 fixes ~5–6 eng-days (B1+B2 together ≈ 1.5 d incl. the restart e2e; B5/B6 ≈ 2 d;
the rest ≤ ½ d each); Track S ~4 d incl. triage; Track CL ~8 d; Track F ~6 d;
Track G ~6 d; docs/gating ~2 d. **~4–5 eng-weeks serialized, compressible to
~3 with the suite/load/failure tracks interleaved.** Cost: $0 local; ask-first
cluster extensions ~$5–10 each.

**Definition of done for "hardened":** every §2.1 fix landed with its pinned
leg red-then-green; every must-run suite green on `pnfs-mds-vm` at a committed
floor; CL1–CL12 pass with all anti-vacuity guards demonstrated; F1–F13 + G1–G3
pass incl. the failing controls failing; the gating matrix committed and the
one-time legs' numbers recorded in the runbook and capacity docs.
