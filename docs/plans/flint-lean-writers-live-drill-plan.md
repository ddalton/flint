# flint-lean — live drill: many agents editing one workspace at once

**Date:** 2026-09-13. **Status:** APPROVED 2026-09-13 (provisioning, the
commit, the event trace as product surface, the Ozone leg, the
compressed ~3 h window of §5). Provisioning follows the local prep of §6.

**Read with:** `flint-lean-writer-lease-and-gated-assessment.md` §4, §10,
§10.1 (the per-barrier lease and the six two-writer defects fixed the
same day); `flint-lean-consensus-protocol-assessment.md` §1.1, §4.6 (why
the lease stays load-bearing, and the one residual); `lean/syncer/AGENTS.md`
("Other agents may share this workspace"); `lean/e2e/run-writers.sh` (the
kind/MinIO form, W1–W7, never run).

## 0. What this drill is for

User, 2026-09-13: "plan a live drill to test these changes. Use multiple
i4i.large nodes if needed. The focus is on concurrent edits/changes from
multiple agents."

The unit tests pin each two-writer race with a hook, one interleaving at
a time, against an in-memory store. The model checks two writers in a
bounded world. Neither shows what a fleet does: six agents editing one
workspace through the DEPLOYED path — the operator, the CSI plugin, one
worker and syncer per agent pod — against real S3 conditional semantics
(including `DELETE If-Match`, which nothing has yet measured), real node
clocks, real latency, and pods that die. This drill does, and it checks
the agent contract's sharing claims with an oracle that can fail.

**The claims under test** (AGENTS.md, and §10.1 of the lease design):

| # | Claim | Fix it depends on |
|---|---|---|
| C1 | Every writer publishes every boundary; none waits for another's lifetime | the per-barrier lease |
| C2 | Disjoint edits reach every other tree | writer-local foreign queue (F5) |
| C3 | A peer's delete reaches every tree where the path is clean; a dirty copy stays and publishes, with a record | tombstones (F6) |
| C4 | A same-file edit: the later boundary's version is current; the earlier is preserved in the bucket, recorded on both sides. **Nothing acked is lost** | 412 preserve/supersede; consume-dirty |
| C5 | The manifest never cites an object that is gone | conditional GC delete (F1); commit-section re-read of observed citations (F2) |
| C6 | A `sync` never skips a change it did not apply | overlay-hidden base kept (F3) |
| C7 | An idle fleet is quiet: no generations, no claims | install nothing when the merge adds nothing (F7) |
| C8 | After the storm stops, every tree converges to the manifest, byte for byte | C2 + C3 + C4 |
| C9 | A UI write through the gateway reaches every agent of the workspace — not only the writer whose consume took the inbox entry — or, over a path an agent had edited, is preserved with a record | the first consumer drops the entry once cited; every other writer learns it through its merge into its local queue (F5). Unit tests: `a_gateway_write_reaches_every_writer_not_only_the_first_consumer` (mutation-checked against the local queue), `a_gateway_write_over_a_path_another_writer_edited_is_preserved_on_that_writer` |
| C10 | A UI write that lands while an agent's upload of the same path is not yet cited is never silently lost: the gateway refuses to overwrite an untracked version (409 `concurrent-write`), and the UI's retry overwrites the version that commit cites | the tracked-only overwrite rule (F8, found by the formal gate: `LeanBarrierLeaseHitlOverUncited`). Unit tests: `a_ui_write_over_an_uncited_upload_is_never_silently_lost`, gateway `a_blind_write_over_an_uncited_upload_is_refused_until_it_is_cited` |

**Known open, measured not gated (finding 10):** a writer lost for good
between an upload and its commit leaves an uncited object the manifest,
a fresh checkout and the live trees disagree about. A4 cannot produce it
(its worker deletions relaunch over the same tree, and a SIGKILLed syncer
restarts with its state); host leg H5 opens it on purpose, kill -9 plus
the state directory removed, and reports the divergence.

**Known open, measured not gated:** a supersede of a peer's uploaded but
not yet cited object leaves that peer citing a generation the key no
longer holds, until the superseding writer's commit (consensus
assessment §4.6, row 7). The drill counts how often and for how long.

## 1. The oracle — decided before the workload

Every agent keeps an append-only journal; every check reads journals,
trees, the bucket and a fresh checkout. Each oracle is self-tested
against injected faults before the drill (§6 item 4), because an oracle
that cannot fail proves nothing.

| Oracle | Check | Catches |
|---|---|---|
| O1 no dangling | a fresh `flint-sync checkout` of the prefix into an empty dir completes (every file CRC-verified against the manifest), and a HEAD of every citation returns the cited etag | C5 |
| O2 convergence | after quiesce (agents stopped, 4 floors), every agent's tree digest (`find -type f` + sha256, sorted) equals the fresh checkout's, INCLUDING absences | C2, C3, C8 |
| O3 nothing acked is lost | for every journaled write whose publish ack is `ok` or `partial`-not-naming-it: its sha256 is in the final manifest for that path, OR in a preserved copy under `.flint/lean/conflicts/`, OR a later acked delete/rename of that path exists | C4 |
| O4 records on both sides | every O3 match via a preserved copy has an `upload-412-preserved` record on the superseding writer and (once its copy was replaced) a `consume-dirty` record on the superseded one | C4 |
| O5 fence health | claim-deadline failures = 0 and deposals = 0 on no-fault legs; waits > 0 (contention really happened) | C1 |
| O6 idle quiet | over the idle window: manifest `seq` unchanged, cell epoch unchanged, zero `publish fence held` lines, per-writer bucket requests per tick at the single-writer idle baseline | C7 |

Journal line (agent-side, JSONL, written BEFORE the publish and updated
with the ack): `{agent, n, op: write|delete|mv, path, to?, sha256, t,
nonce, ack: {status, seq, dropped}}`.

**Collection never uses `kubectl cp`** (it truncates silently and exits
0): each pod tars its journal, tree digest, `conflicts.jsonl` and syncer
log to `s3://<bucket>/_rig/collect/<leg>/<pod>.tar.gz` and prints the
tarball's sha256; the Mac pulls it with `aws s3 cp` and compares.

## 2. The rig

**Cluster** — trove, ALL-SPOT (CP included), us-west-1:
- 1 × i4i.large control plane (trove needs a storage-optimized CP; as on
  runcu, Flint CSI workloads deleted and flux scaled to 0).
- 3 × i4i.large workers. Six agents, two per node (`topologySpreadConstraints`
  maxSkew 1): independent syncers on independent clocks, and a pod kill
  never takes out every writer. One verifier pod on the CP-adjacent worker.
- Read the `type=` line `aws-live-allspot.fish` echoes — it ignores
  `TROVE_AWS_CP_TYPE`/`WORKER_TYPES`. Drive over SSM; do not wait on the
  trove kubeconfig download (it hung on runcu).

**Bucket** — fresh, UNVERSIONED (lean no longer needs versioning, and the
teardown stays a plain `rb --force`): `flint-lean-writers-<date>` in
us-west-1, plus an inline policy on `TroveSSMInstanceProfile` scoped to
it. Both are teardown items.

**Images** — built locally (zigbuild, x86_64 musl) from a COMMITTED sha:
the lean worker (`flint-s3-worker-lean`), the s3 CSI node plugin, the
lean operator. Tag `writers-<sha8>`, imported into containerd on every
node from `_rig/` tarballs, never published. On every node, the sha256 of
`flint-sync` inside the image must equal the local build's before any
leg runs. A second syncer binary built from `ba4f53d9` (pre-fix) is the
control arm for the host legs (§3.2).

**Deployment**, per leg: chart with `node.region: us-west-1`; a
`FlintLeanWorkspace` per leg (`region: us-west-1`, `floorSecs: 5`, worker
memory limit 2Gi so a storm cannot OOM a worker); a Deployment `agents`
(6 replicas) whose pods mount the workspace inline
(`csi: {driver: s3.csi.chert.us, volumeAttributes: {chert.us/workspace: <cr>}}`)
and run `agent.sh`; a `verifier` pod with aws cli, jq, python3 and the
`flint-sync` binary for fresh reader checkouts (a reader checkout never
claims — W6).

**The agent** (`lean/e2e/writers-live/agent.sh`, busybox sh + sha256sum):
files only, as a real agent. Seeded per agent so a failing run replays.
Each step: pick an op by mode, perform it, journal it, touch
`.flint/publish` with `{"nonce":"<agent>-<n>"}`, wait for `publish.ack`
to name the nonce (timeout = 3 floors, journaled as `no-ack`), record the
ack. Modes: `disjoint` (own subtree), `hot` (a shared set of 30 paths,
agent-tagged content), `churn` (shared 50 paths: 40% write, 30% delete,
20% `mv`, 10% identical bytes), `vocab` (content drawn from 8 fixed
bodies, so identical bytes are the norm).

## 2a. Evidence — captured continuously, so a failure is explained without a re-run

A race that fails once on a live rig may never fail again, so the drill
records enough to rebuild the interleaving from evidence alone. Today it
could not: the syncer prints a handful of prose lines, has no request
counters (its metrics are boundary gauges), and has no log knob. Its
state directory is an emptyDir, and `/var/log/pods` is removed with the
pod, so a deleted pod (leg A4) takes both with it. Everything below ships
off the pod as it happens.

| # | What | How | Explains |
|---|---|---|---|
| E1 | **Protocol event trace** per writer, JSONL: wall + monotonic ms, holder, incarnation, barrier `flush_uuid`, and one event per protocol step — consume entry (path, etag, source shared/queue, action); tombstone applied; scan summary; upload outcome per path (PUT / adopt / preserve+supersede / park / defer, etag, condition, status); claim polls (cell epoch, holder, released, handoff, waiters, verdict); claimed; observed-citation re-read; merge (theirs seq, upserts, deletes, foreign, gone, installed-nothing); CAS (expected etag, new etag, seq, result); GC per path (HEAD etag, recognized, DELETE result); queue write; baseline saved; window open/clear; release (handoff); fence; ack (nonce, status, seq, dropped); sync (applied, hidden, advanced); store-clock skew (S3 `Date` − local) at each claim | new syncer switch `FLINT_SYNC_EVENT_TRACE`, to stderr; reaches deployed workers through a new CR field `spec.eventTrace` | the exact interleaving of every writer, per path |
| E2 | **Store request counters** by operation (GET, HEAD, PUT, COPY, DELETE, LIST, cell) | a counting wrapper around the store in the syncer; on `/metrics` and in a per-barrier trace summary | O6's per-writer request rate; any retry storm |
| E3 | **Node evidence shipper**, per node over SSM | a watcher `tail -F`s every worker, agent, CSI-plugin and operator container log into node-local NVMe (`/mnt/nvme/evidence/`, survives pod deletion); every 10 s the cgroup `memory.peak` / `cpu.stat` of each worker; every 60 s `chronyc tracking` (clock offset); `dmesg` (OOM kills); `journalctl -u kubelet -u containerd`; uploaded to `_rig/evidence/<node>/` every 5 min and at leg end with a sha256 manifest | pod deaths, OOMs, clock skew between writers, plugin/operator decisions |
| E4 | **Cluster events and pod states** | the verifier streams `kubectl get events -A -w -o json` (events expire after an hour) and snapshots pod status (restarts, `lastState`, OOMKilled) every 60 s, to S3 | what Kubernetes did to the writers |
| E5 | **The store's own record** | S3 server access logging on the drill bucket into a separate log bucket (free; the store's record of every request and its HTTP status, independent of what the syncer says it did; hours of delivery delay are fine for a post-mortem). Optional: CloudTrail S3 data events on the drill bucket (~$0.10 per 100k events, ~$1 for the drill; minutes of delay, request parameters included) | whether a 412 / 204 really happened, and in what order |
| E6 | **Bucket history** | the verifier copies every new `current` pointer body, its generation document, the epoch cell and the inbox document every 500 ms to `_rig/history/<leg>/` (retention reaps all but 5 generations, so this is the only full manifest history); end of leg: a full listing of the prefix with ETag, LastModified, size | manifest and cell history; the ground-truth object set |
| E7 | **Writer local state** | at every leg end, and BEFORE any pod is deleted or syncer killed: each writer's `.flint-sync/` (baseline, intent, foreign queue, conflicts, incarnation) and `.flint/` (acks, gauges, capabilities, `remote.seq`) to S3 with sha256 | why a tree diverged: what the writer believed |
| E8 | **Hang capture** | if a barrier runs past 5 min or a claim waits past twice its deadline: `kubectl debug --target=worker` with a gdb image, `thread apply all bt`, plus `/proc/<pid>/{status,wchan,stack}` and the trace tail; an unstripped copy of the exact binary (with its build id) is kept locally to symbolize | a stuck claim, a wedged upload |
| E9 | **Timeline tool** `lean/e2e/writers-live/timeline.py` | merges E1 traces from all writers (clock-corrected by E3), E5 access logs and E6 history into one per-path timeline | reading the evidence |

**Freeze on failure.** Any failed oracle or guard: the driver pauses the
agents with a flag file (never scaling them down, which would delete
their state), runs E7 for every writer, flushes E3–E6, writes
`_rig/FAILED-<leg>`, and stops: no further legs, no teardown, until the
evidence has been read.

**The evidence is tested before the drill.** E1 is switched on in the
unit-test repros of §10.1 (F1, F2, F3, F5–F7): E9 must show each race's
interleaving from the trace alone (for F1: A's GC HEAD, B's PUT, A's
DELETE and its result, B's CAS) before any leg relies on it.

## 3. The legs

### 3.1 Deployed legs — the concurrent-agent storms

| Leg | Setup and workload | Fault | Oracles | Anti-vacuity guard | Time |
|---|---|---|---|---|---|
| **P0 preflight** | image content check on 3 nodes; `flint-sync probe-conditional` on the drill bucket (records S3's PUT AND DELETE `If-Match` results — first ever for DELETE); one agent publishes 50 files, fresh checkout byte-equal; `SyncerObserved` names the drill binary | a planted corrupt object under a cited key | O1 must FAIL on the plant, pass after | the plant is caught | 15 min |
| **A1 disjoint storm** | 6 agents, `disjoint`, 20 files per publish, publish every 3–10 s jittered, 10 min, then quiesce | none | O1 O2 O3 O5 | every writer's report shows `consumed > 0` from each peer; waits > 0 | 20 min |
| **A2 shared hot set** | 6 agents, `hot`, 10 min, quiesce. The verifier GETs every citation `If-Match` every 2 s throughout | none | O1 (after quiesce) O2 O3 O4 O5 | ≥ 50 `upload-412-preserved` records; the residual's 412 count and longest window REPORTED (expected ≥ 0 during, must be 0 after quiesce) | 25 min |
| **A3 churn** | 6 agents, `churn`, 10 min, quiesce | none | O1 O2 (absences too) O3 O5 | deletes crossed: `consume` removals on every writer; `consume-foreign-delete-vs-dirty` and `gc-skip` counts reported | 25 min |
| **A4 identical bytes under kills** | 6 agents, `vocab` on a shared set with deletes, 15 min; every 90 s one worker pod is deleted (its syncer drains or dies) and one syncer is `kill -9`'d in place | pod deletion + SIGKILL | O1 O2 O3; O5 relaxed to "every deadline failure is followed by a successful retry" | adopt path exercised: `adopt` / `adopt-withheld` log counts > 0 (needs the log lines, §6 item 2) | 30 min |
| **A6 UI writes during a storm** | A2's `hot` storm and A3's `churn` storm each run with a seventh actor: the UI, writing through the gateway (`flint-lean-gateway` in the verifier pod, HTTP writes to the same shared paths, ~1 write every 2 s, journaled with the gateway's returned etag as its ack) | none | O2 (every UI write that is final reaches EVERY agent tree, not only the first consumer's) O3 (every UI write is final, preserved with a record, or later superseded by an op whose `base` is it) | the inbox entries were consumed by different writers (trace `consume` events name ≥ 2 distinct holders), and ≥ 1 UI write met a locally-edited path (`consume-dirty` on some writer); the UI journal's 409 `concurrent-write` count is REPORTED (C10's rule firing) but not required — an upload window is short, and a storm may never land a UI write inside one | rides A2 + A3 |
| **A5 idle fleet** | after A1's quiesce, the 6 agents stay mounted and idle for 30 min at `floorSecs: 5`; then ONE agent edits one file; then 10 more idle minutes | none | O6; control: the single edit moves `seq` by exactly 1, then quiet again | the idle writers really ran: ≥ 300 ticks each in their logs | 45 min |

### 3.2 Host legs — each race opened on purpose, on real S3

The deployed path cannot pass drill-only env to a worker, and a millisecond
window is never hit by timing. These run `flint-sync` processes over SSM
on two worker nodes against `h/<leg>` prefixes of the same bucket. Each
has a CONTROL ARM on the pre-fix binary that must show the defect — the
proof that the leg can fail against real S3, not just the double.

| Leg | Sequence | Pass (fixed binary) | Control (pre-fix `ba4f53d9`) |
|---|---|---|---|
| **H1 GC gap (F1)** | node-1 A deletes `x` with `FLINT_SYNC_DRILL_HOLD_GC_SECS=20`; during the hold node-2 B edits `x` and barriers | A's DELETE gets 412 → `gc-skip`; B's commit cites `x`; O1 clean | B's citation dangles (O1 fails) |
| **H2 adopt window (F2)** | B deletes `x` with `DRILL_HOLD_COMMIT_SECS=30`; during the hold A writes B's exact bytes to `x` and barriers (412 → adopt → waits for the cell); B commits and collects | A's commit withholds `x` (`adopt-withheld`, ack `partial`); its next barrier publishes; O1 clean | A cites an object that is gone |
| **H3 sync overlay (F3)** | gateway HITL write to `x`; A deletes `x` with the GC hold; during it B runs the `sync` verb | within 2 floors B's tree has no `x`; O2 | B keeps `x` forever |
| **H4 stall + drain** | run-writers W5 (mid-commit stall deposed after ~60 s) and W7 (heartbeats, drain hands the fence on), on real clocks | as written in run-writers.sh, expectations re-derived for "a fence is a retry" | — |

### 3.3 Ozone leg (on-prem target) — a separate instance, recommended

Not on the cluster. The 2026-09-12 Ozone probe ran on its own spot
**i4i.xlarge** (four vCPUs for Ozone's six JVMs plus the syncer), and this
leg reuses that rig as it was: Ozone 2.2.1's own compose environment,
`lean/e2e/perf/ozone-probe/` scripts, driven over SSM, s3g on
`localhost:9878`. Run `flint-sync probe-conditional` (PUT and DELETE
legs), then H1 and H2 as two `flint-sync` processes on that one host
against Ozone (two writers need not be two machines to open a held
window). If Ozone ignores `DELETE If-Match`, F1's fix is silently void
there, and multi-writer on Ozone must be refused until it is solved: a
blocking finding for the on-prem target, unrecorded today. ~1 h including
Ozone start-up; the instance is torn down on its own.

## 4. What a result means

- Any **O1 or O3** failure is a data defect: stop the leg, collect
  everything, and reproduce it locally as a failing test before any
  further leg or any fix.
- An **O2** failure is a convergence defect: same procedure.
- **O5/O6** failures are liveness or cost defects: record, continue.
- A **host-leg control arm that passes** means the leg did not open the
  race on real S3: the leg is VOID, not green.
- A spot interruption during a leg voids that leg; re-run it.
- The residual's numbers are reported, not gated.

## 5. Cost and time (estimates — re-check at provision)

| Item | Estimate |
|---|---|
| 4 × i4i.large spot, us-west-1 | $0.042/h each when last observed (runcu, 2026-09-12) ⇒ ~$0.17/h; a 6 h window ~$1 |
| 1 × i4i.xlarge spot for Ozone, ~1 h | spot price not recorded; on the order of twice the i4i.large rate — cents |
| Evidence | S3 access logs: storage only; traces + node logs: a few GB in the drill bucket; optional CloudTrail data events ~$1 |
| S3 requests | storms dominate: ~6 agents × ~1–2k PUTs per leg plus ~5 cross-writer GETs each, the verifier's GET sweep in A2, and idle ticks (~5 requests × 7 writers × 12/min) in A5 — order of 1M requests ⇒ ~$3–6 |
| Storage, transfer | negligible: small objects, same-region S3, MBs of pod traffic |
| **Total** | **~$5–10** |
| Cluster window | **~3 h, compressed (approved):** provision + setup + evidence shipper 1 h; A1–A4 with 5-minute storms and A5 with a 15-minute idle, ~1.25 h, while H1–H4 run at the same time from the control-plane node on their own prefixes (the oracles do not depend on timing); evidence pull, teardown + zero-set 45 min, not waiting on S3 access-log delivery (the log bucket is kept a day and deleted on its own). The first uncompressed estimate was ~5.5 h: sequential legs, 10-minute storms, a 40-minute idle leg |
| Ozone instance | ~1 h on its own, in parallel with the cluster legs |

## 6. Prep, all local, before asking to provision

1. **Formal gate green** on the extended module (running 2026-09-13), then
   **commit** the working tree so the images carry a sha.
2. **Syncer evidence and drill switches:** the event trace (E1,
   `FLINT_SYNC_EVENT_TRACE`) and the CR field that turns it on for a
   deployed worker (`spec.eventTrace`); store request counters (E2);
   `FLINT_SYNC_DRILL_HOLD_GC_SECS` (between the GC's HEAD and its DELETE,
   drill-only, like `…HOLD_COMMIT_SECS`) with a unit test that it holds.
   The anti-vacuity counters (adopt, `adopt-withheld`, tombstone applied,
   skipped install) are trace events. Then the evidence test of §2a:
   every §10.1 repro, traced, reconstructed by `timeline.py`.
2a. **Evidence rig:** the node shipper (E3), the event/pod-state
   streamer (E4), the bucket-history sampler (E6), the state snapshot
   (E7), the hang-capture recipe with a gdb debug image (E8), and the
   freeze-on-failure path in the driver.
3. **Rig:** `lean/e2e/writers-live/{agent.sh, oracle.py, collect.sh,
   drill.sh, manifests/}`; `drill.sh <leg>` runs one leg end to end and
   exits non-zero on any failed oracle or guard (runcu's rig printed
   "SETUP DONE" on a failed step — every wait is fatal here).
4. **Oracle self-test:** synthetic journals and trees with each fault
   injected (a lost acked write, a dangling citation, a diverged tree, an
   extra file, a missing record) — each must be flagged. No leg runs on an
   oracle that has not failed once.
5. **Kind dry run** of P0 and A1–A3 at 2 agents × 2 minutes on the
   existing `flint-lean-chaos` MinIO rig, including `probe-conditional`
   on MinIO — rig bugs are cheap here and expensive on EC2. (Check first
   that no other session is driving Docker Desktop.)
6. **Images and binaries:** the three images from the committed sha, the
   pre-fix `ba4f53d9` syncer, tarballs staged for `_rig/`.
7. **Teardown script** written and reviewed before provisioning.

## 7. Traps carried from earlier drills

- A CR with no `region` crashloops against a us-west-1 bucket (runcu) —
  set it on every CR and `node.region` on the chart.
- Worker memory: the 512 window OOM-killed a 1Gi worker; now 128, plus
  the 256 MiB upload bound; storms still get 2Gi.
- The deployed checkout is disk-bound in the loop image (runcu) —
  irrelevant to correctness; no timing claims from this drill.
- A pod's image is mutable: unique tags, content check per node.
- Trove installs Cilium with WireGuard — irrelevant to correctness; noted.
- `kubectl cp` truncates silently — S3 collection with sha256 both ends.

## 8. Teardown — every item, then the zero set

1. Copy `_rig/collect/`, `_rig/evidence/`, `_rig/history/` and the
   access-log bucket (wait for delivery, up to a few hours, or accept the
   gap and say so) to the Mac, verify sha256.
2. `aws s3 rb s3://flint-lean-writers-<date> --force` and the access-log
   bucket; delete the CloudTrail trail if one was created (trove-admin).
3. `aws iam delete-role-policy --role-name TroveSSMInstanceProfile
   --policy-name <drill-policy>` (trove-admin).
4. trove `POST /projects/delete` for the cluster, and for the Ozone
   instance's project.
5. Zero set, with **trove-admin** (rolesanywhere cannot enumerate):
   instances 0, spot requests 0, volumes 0, the projects' SGs gone,
   buckets 0, inline policies 0, trails 0, trove orphans matched 0.

## 9. Decisions for the user

| # | Question | Recommendation |
|---|---|---|
| 1 | Approve provisioning: 1 CP + 3 workers, all i4i.large spot, us-west-1, ~6 h, ~$5–10 | yes, once §6 is done |
| 2 | Commit the working tree (lease, upload bound, two-writer fixes) before building images — local commit, no push | yes |
| 3 | Include the Ozone leg on its own i4i.xlarge (§3.3) | yes — it is the only way to learn whether F1's fix holds on-prem |
| 4 | Agent count | 6 (two per node); raise to 9 only if A1–A3 are green and time remains |
| 5 | Add the drill-only GC hold to the syncer (§6 item 2) | yes — without it H1/H3 cannot be opened |
| 6 | The event trace and request counters as PRODUCT surface (`FLINT_SYNC_EVENT_TRACE`, `spec.eventTrace`, counters on `/metrics`), not drill-only | yes — the next field failure on a shared workspace needs exactly this evidence, and a CR switch is the only way to reach a deployed worker |
| 7 | CloudTrail S3 data events in addition to access logs (E5) | optional, ~$1; access logs alone suffice for a post-mortem |
