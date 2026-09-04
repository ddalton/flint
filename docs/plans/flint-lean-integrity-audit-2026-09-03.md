# flint-lean integrity audit — 2026-09-03, HEAD `3e1560ab`

Read-only, multi-agent audit of the flint-lean storage backend under
network, local-device, crash and pure-spot-preemption failures, with S3
assumed perfect (durable, strongly consistent, conditions enforced
atomically). Scope: `lean/sidecar/src`, `crates/flint-store/src`,
`spdk-csi-driver/src/{lean_operator,s3csi}`, `lean/formal`, the plan
docs. The tree was audited at `3e1560ab`, i.e. after the pointer/chunk
wave, the lease auth-refusal fix, the missing-bucket store fix and the
s3csi second pass (`b148319b`).

**Method.** Five area finders (arbitration; manifest layout and reaper;
barrier under crash and lost responses; local device and bytes; the CSI
delivery and operator), each returning at most 8 findings with
`path:line` plus a quoted fragment per step and a list of what it found
safe. One refuter-then-tracer agent per medium-or-higher finding, at most
5 per area. 27 agents, 31 minutes, 3.4 M tokens. The author's six
deliberate design points for the chunk wave and the one acknowledged open
item (csi design §6.3, the arbitration half) were in every brief.

**Caveat on the verification bar.** All 22 verified findings came back
*confirmed*, none refuted. A single verifier that refutes nothing is a
weak filter, so six mechanisms were checked by hand against the code
before this report was written (marked ✔ below); all six held. The other
sixteen rest on one agent's reading plus its trace — credible, not
independently confirmed. Ten findings (lows, or past the per-area cap)
were not verified at all.

## Headline

The manifest CAS is still the atomic commit point and the new
pointer/chunk layout holds up in every way its author claimed: no writer
can install a pointer from a stale view, `assemble` refuses every
malformed chunk list, the reader revalidation and the reaper's fence do
what the models say, and none of the 22 confirmed findings restates one
of the six deliberate points. The holes are around the commit point, not
in it:

1. **The cadence barrier's between-chunk fence does not stop a deposed
   writer** (the gated lanes are fenced), so a straggler overwrites the
   cited generation of every key it still had to upload — silently, and
   unrecoverably on an unversioned bucket.
2. **A lost renew response makes a live holder fence itself**, and under
   CSI that is permanent: exit 0, no restart, no relaunch, nothing
   publishes for the rest of the tenant's life.
3. **NodeUnpublish still removes a tree whose drain did not publish**
   when the drain was attempted and then failed or fenced. `b148319b`
   closed the cases where the drain never started; it keys the remaining
   case on the pod being gone, not on the drain's outcome.
4. **The chunk reaper is not wired.** `sweep_chunks` has no production
   caller while chunking is on by default, so chunk objects are never
   collected and none of the reaper's four rules runs outside unit tests.
   *After the audit:* `cdabc10c` fixed the reaper's age judgement (one
   HEAD per candidate before the delete); as of that commit the reaper
   still has no production caller — barrier.rs:818 calls only
   `sweep_generations`.

## Confirmed findings, grouped by mechanism

Severity is the verifier's. "✔" = the mechanism was re-read in the code
by the session author before publication. Finding ids are the workflow's
(`<area>-<n>`); the full scenario, trace and evidence for each is in the
run's result (`wf_b914bffb-27e`).

### 1. The cadence barrier's between-chunk fence is a no-op — HIGH, arbitration ✔

`arbitration-1` (+ `arbitration-6`, the assurance gap). `renew_if_due`
(barrier.rs:368) returns `Ok(())` when `fresh || state.epoch !=
lease.epoch` — that is, exactly when the cell shows a successor. The
comments at :366 and :592 credit it with being "how a straggler stops
mid-flight", but the only `verify_not_deposed` calls in the fused barrier
are before any write (:428) and at the manifest CAS (:676, :740). The
gated lanes are fenced: both of their calls (gated.rs:766, :827) are
followed immediately by `verify_not_deposed_pub`; barrier.rs:593 is the
one call that stands alone, inside the upload loop. A holder
whose renewals starve inside one upload chunk (`select!` exclusivity,
:539-548) is taken over; it then completes every remaining `put_whole`
with `If-Match` on baseline etags that still match, because the successor
has published nothing yet. Every manifest-resolving reader then hits 412
on the cited etag and takes the S3-wins arm (checkout.rs:279-289),
adopting the straggler's uncited bytes for the keys it reached. No error
anywhere; on an unversioned bucket the cited generation is gone. This is
the boundary-verbs-plan §3.4 straggler-PUT class, reached by an ordinary
renewal-starved writer instead of a SIGSTOPped one, with a window of the
whole remaining upload set instead of one chunk. No test deposes a writer
between chunks; the long-barrier test only checks a healthy lease renews.

*Fix:* two options with different blast radii. Narrow: add
`verify_not_deposed` after barrier.rs:593, the pairing the gated lanes
already use — one bucket read per chunk, cadence barrier only. Wide: make
`renew_if_due` itself fence when the cell is not at our epoch and holder
(zero extra requests, but it changes behaviour under all three callers
and the comment at :382 that appeals to a check that is not there). Either
way add the between-chunks deposal test and assert `put_whole` is bounded
by one chunk; the `renew_if_due` doc comment at :365 must stop claiming a
fence it does not raise.

### 2. A lost renew response self-fences a live holder; permanent under CSI — HIGH, arbitration ✔

`barrier-3`, `arbitration-2` (first run's M1, still open). `lease::renew`
is a CAS with `IfMatch(token)`; if our own PUT landed but the response was
lost, the next renew 412s on the stale token and is mapped to `Fenced`
("deposed at renew", lease.rs:171). `flint_sync.rs:242` exits 0 on
`Fenced`. Under the operator's `restartPolicy: Always` the pod restarts
and self-recognises. Under CSI the worker is `OnFailure`, exit 0 is
`Succeeded`, and the plugin relaunches only `Failed` pods
(node.rs:677) — so the sole holder of the workspace is gone for the
tenant's life, its writes accumulate unpublished, and at unpublish the
tree goes with the pod (theme 3). Loud only in the worker's log.

*Fix:* on `PreconditionFailed` in `renew`, re-read the cell; if it names
our holder at our epoch and is not released, the 412 was our own landed
write — adopt the observed token and continue; fence only when holder or
epoch differ. Separately, make a fenced exit under CSI something the
plugin acts on (a distinct exit code, or a `mount.error`).

### 3. NodeUnpublish removes a tree whose drain published nothing — HIGH, staging-window ✔

`csi-1`, `barrier-2`, `local-2`, `arbitration-3`, and `csi-5` (first run's
M17; the §6.3 open item's CSI amplification). `unpublish_lean` preserves
the tree when the syncer was already gone, had already exited, or was
killed at the ceiling (node.rs:1125-1142, :1175). Between those, its
completion sensor after the SIGTERM is `worker::is_gone` (node.rs:1165),
which is true whether the drain succeeded, failed all three attempts
(exit 1), or self-fenced (exit 0) — and the tree is removed. `local-2` is
the ordinary way in: one unreadable path fails every barrier
(scan.rs:33/:47 propagate `?`), then fails the drain three times, the
lease is released, and the tree is deleted with everything unpublished in
it. `arbitration-3`: a failed drain still calls `epoch_release`, so
`released` attests a final flush that did not happen, and the retry
budget is 3×2 s, not the remaining grace. `csi-5`: a challenger deposing a
credential-paused holder (§6.3) costs the tree, not "a takeover plus
recover-staged".

*Fix:* make the drain's outcome the sensor. Have `drain()` write an
atomic `drained` marker (seq, unix) into the comm dir after it returns
`Ok`; in `unpublish_lean`, after `is_gone`, route through
`preserve_undrained` unless that marker post-dates `drain_started_unix`.
Retry the drain until grace minus slack; on final failure do not release
the epoch and exit with a distinct code. The plugin half is in code this
session wrote.

### 4. A Running worker with no syncer inside is "alive" forever — HIGH, staging-window

`csi-2`. Liveness is the pod phase (`WorkerWatch`), not the child. After a
node reboot (the launch record lives on a tmpfs emptyDir) or a relaunch
that failed after pod creation, PID 1 runs with no `flint-sync`, the pod
is `Running`, republish sees alive, and nothing publishes for the rest of
the tenant's life.

*Fix:* have PID 1 write a host-visible `comm/launched` marker when it
starts the child; treat `Running` without it (or a container whose last
exit code is the launch failure) as lost and re-send the launch on the
next republish.

### 5. refuse-foreign is advisory on the data plane — HIGH, arbitration ✔

`csi-3`. `resolve::decide_lean` (resolve.rs:93-98) refuses only when the
CR's phase is already `Refused`; a CR the operator has not yet reconciled
(phase `None`) resolves to its spec. A FlintLeanWorkspace naming another
project's claimed prefix, mounted by a pod before the operator's first
reconcile, is checked out and republished over that project's manifest.
The claim cell is never read on the data plane.

*Fix:* stamp the project id into `sync_env` and have the syncer GET the
claim cell before `claim_step`/checkout, refusing unless it is absent or
names the stamped project — plan P1 already requires the claim to be a
data-plane precondition.

*Fixed 2026-09-04* (`lease::verify_claim`, `FLINT_SYNC_PROJECT_ID`, leg
S22). The first S22 run found a second hole behind the first: the
refusal exited 1, which under `restartPolicy: OnFailure` is a restart,
which the supervisor turned into a relaunch from `launch.json`, and the
plugin — which judges a worker only by phase `Failed`/`Succeeded` —
reported "checkout in progress" to the tenant for the whole leg. A
refusal now has its own variant and exit code (78, `EXIT_REFUSED` =
`SYNCER_EXIT_REFUSED`), the plugin reads it from the container's
termination record, fails the publish with the syncer's own reason,
raises `SyncerRefused`, and tears the worker down; the same tenant pod
mounts on kubelet's next retry once the claim is its own.

### 6. Migration from the legacy single-object manifest is not fenced against an old-binary straggler — HIGH, arbitration

`manifest-1`, `arbitration-4`. Only during a mixed-version rollout with a
pre-pointer binary still publishing. `rotate_for_takeover`'s migration
branch no longer CASes the legacy object the straggler holds a handle to,
so the straggler's legacy-key CAS succeeds and its citations are silently
dropped; and the best-effort poison of the legacy key can 412 after an
old-binary install, stranding an acked publish nobody reads.

*Fix:* fence the legacy object first — CAS a content-identical seq+1 copy
onto the legacy key with `If-Match` (what the pre-pointer rotation did),
treat 412 as a lost race, and only then write chunks and install the
pointer.

### 7. A lost takeover-acquire response skips the rotation — MEDIUM, arbitration

`barrier-5` (first run's M3, still open). The acquire landed, the client
saw an error, the next `claim_step` finds its own holder id in the cell
and self-recognises — and rotation, which "is needed only for the
unreleased-foreign takeover", is skipped, so a stalled straggler's
pointer CAS is not fenced.

*Fix:* self-recognition should require remembering the claim
(`state.epoch == inc.epoch`, saved only on a successful claim); rotate
whenever that fails even if the holder id matches.

### 8. Gated lane: two ways an acked write stays uncited — HIGH, implementation-defect

`barrier-1`: after a consume-dirty advances the baseline, the lane never
re-stages the path; at citation the baseline etag and the object etag
diverge and every later agent edit of that path parks forever.
`barrier-4`: an acked HITL write is dropped from the inbox at consume and
cited only at the next coherent point; a pod replacement in between
leaves it current-but-uncited, invisible to pinned readers and unnamed by
`orphans.json`.

*Fix:* record `base_etag` on the pending entry and re-stage when the
baseline moved past it; defer `drop_entries` in the gated lane until the
consumed path is cited, or have `surface_orphans` include repair
candidates.

### 9. Local durability and TOCTOU — HIGH/MEDIUM, implementation-defect

`local-1` ✔ (first run's M13, still open): there is no `fsync`,
`sync_all` or `sync_data` anywhere in `lean/sidecar/src`. After a node
power loss the marker and baseline can survive while
rename-without-fsync files come back zero-length, and the next scan
publishes them as local edits over the good version. `local-3`: consume
overwrites an agent write that lands between the dirty check and the
rename. `local-4`: the sync verb applies remote adds and deletes against
a scan taken minutes earlier. `local-5`: resume over a live tree that
lost its marker re-materialises every present-but-modified path and
resurrects unpublished deletes; the module doc's "local-wins skips
present paths" is not what the code does.

*Fix:* `sync_all` before the rename and fsync the parent in
`safefs::write_via_tmp`; re-stat immediately before the rename/unlink in
consume and sync (the idiom consume already uses at barrier.rs:177);
record "this tree went live" outside the tenant-deletable tree.

### 10. The chunk reaper is not wired, and judged age from a stale listing — LOW/MEDIUM ✔ (half fixed after the audit)

`manifest-4` (unverified, but confirmed by hand: `sweep_chunks` is called
only from tests.rs) and `manifest-2`. With chunking on by default every
publish leaves its superseded chunks in the bucket forever; the cost is
storage, and the reaper's four model-established rules run nowhere in
production. When it is wired, `manifest-2`: age is judged from the
pre-fence listing, so an adopt-rewrite landing between the listing and
the delete loop is invisible and a live pointer can name a deleted chunk.

*Status after the audit:* `cdabc10c` (other session) closed the age
hole — one HEAD per candidate immediately before the delete, grace
judged from that Last-Modified, a store that cannot answer leaks rather
than deletes; falsifiable test added (142/0). It also found the hole was
wider than stated: the listing's timestamp could never see ANY refresh,
so `LeanChunkGC.tla`'s `Doomed` (judged at delete time) specified a rule
the code did not implement. The wiring half is NOT in that commit:
barrier.rs:818 still calls only `sweep_generations`, and `sweep_chunks`
still has no caller outside tests.rs as of `cdabc10c`.

*Remaining fix:* call `sweep_chunks` beside `sweep_generations` after a
successful install, or gate the chunk default on it.

### 11. `recover-staged` cannot run under CSI — MEDIUM, assurance gap

`csi-4`. The documented recipe execs the verb into the worker, where
`flint-sync run` holds the state dir's exclusive flock (state.rs:150) for
the pod's life; the verb is refused and there is no other container.

*Fix:* serve the verb from the process that holds the lock — a
`/v1/recover-staged` on the UDS door dispatched at the run loop's
existing safe point — and route `flint-sync ctl recover-staged` to it.

## What held

Each finder listed what it examined and found safe, with the guard line.
The parts most worth recording, because they are exactly the new code:

- A pointer CAS from a stale view is impossible: every writer presents
  the ETag of the load it just performed; every pointer write bumps
  `seq`, so there is no ABA on the ETag.
- The chunked merge cannot splice: every publisher re-splits from the
  merged entry map after an entry-level merge; rotation is the only
  pass-through and changes no entries.
- `assemble` refuses count, order, address, first-key, spill and
  duplicate violations; a substituted or truncated chunk cannot read as a
  shorter manifest. A checkout is one `load`, and bodies are re-hashed on
  arrival, so a torn mix of two generations cannot assemble.
- Reader revalidation restarts under a moved pointer and refuses under an
  unchanged one; the only `Ok(None)` left is the legacy no-object case.
- An older binary on a chunked pointer fails to parse (`entries_key` was
  required) and returns `LeanError::State`, never an empty workspace;
  NoSuchBucket is `Other`, never `NotFound`.
- The `prev_chunks` skip is safe against a reaped chunk: a chunk in
  `prev` can only be collected after the live pointer stops naming it,
  which means the straggler's CAS 412s.
- `sweep_chunks`' fence and pagination: the store paginates fully and the
  after-read follows the last page, so a publish landing anywhere during
  the listing aborts the pass.
- The data-key GC delete set is computed from the installed manifest and
  HEAD-guarded on the baseline etag; the gated version reaper deletes only
  ids its own stage named and fails closed when the installed manifest
  names no version.
- Multipart ETags are never used as content identity; every PUT carries a
  full-object CRC-64/NVME validated server-side, and every 412
  arbitration compares CRCs.
- Path containment on every materialisation refuses `..`, absolute
  paths, `.flint/` and any symlink component; the scan never reads an
  I/O error as absence.
- Two workers for one volume cannot be created by a plugin or broker
  restart (the worker name is a function of the volume id; `ensure`
  adopts a live namesake); `published_action` never starts a published
  lean tree over; the drain key is exchanged before the SIGTERM and sized
  to the whole drain.
- The author's six deliberate points surfaced as such, not as defects.

## Not verified (ten), in the order they deserve a second look

- `local-6` (medium): the upload path follows symlinks the scan skipped —
  a regular file swapped for a symlink between scan and read publishes
  the link's target, including files outside the workspace in the
  credential-holding syncer's namespace.
- `local-7` (medium): a whole-tree sync advances `inst_base` past paths
  it recorded conflicts on, so the next merge treats the foreign version
  as integrated.
- `barrier-6` (medium): a gateway PUT that loses the window race leaves a
  foreign current version with no inbox entry; the sidecar parks that path
  forever and the drain still exits 0.
- `manifest-3` (low, author-acknowledged assumption): rule 3 of the
  reaper — grace outlives the longest publish — is unguarded on the
  publisher side.
- `arbitration-5`, `arbitration-7`, `barrier-7`, `local-8`, `csi-6`
  (low): clock-based `fresh` never renewing on a slow node; a takeover
  whose rotation fails after the acquire never rotates; a crash between
  baseline rewrite and `clear_window` loses merge-preserved foreign
  entries; a crash-orphaned `.flint-sync-tmp` is published; the stale-MPU
  sweep aborts every upload whose `Initiated` the store omits.

## Against the first run (paused 2026-09-03 afternoon)

The first attempt reached 57 merged findings and had confirmed six before
it was paused. Of those six: M1 (lost renew self-fence) is theme 2, still
open; M3 (rotation skipped on a lost acquire) is theme 7, still open; M4
(straggler stall past the deposal check) is subsumed by theme 1, which is
wider; M5 (unfenced HEAD-then-DELETE GC) and M7 (a baseline entry
outliving its object) were not re-found — this run's finders were capped
at eight findings each, so that is not evidence of a fix. M13 (no fsync)
is theme 9, still open. M17 (NodeUnpublish deleting an undrained tree) is
theme 3, partly closed by `b148319b`.

## Suggested order

1. Theme 1 — `verify_not_deposed` after barrier.rs:593 (or the wider
   `renew_if_due` change), plus the missing between-chunks test.
2. Theme 2 — re-read the cell on a renew 412; give a fenced exit a code
   the plugin acts on.
3. Theme 3 — a drain-outcome marker; preserve on anything else.
4. Theme 10 — wire `sweep_chunks`, or turn the chunk default off until
   it is (the age half landed in `cdabc10c`).
5. Theme 5 — the claim cell as a data-plane precondition.
6. Theme 4, then the gated pair (8), then local durability (9).
