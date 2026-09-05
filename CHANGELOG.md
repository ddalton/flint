# Changelog

All notable changes to Flint CSI will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The public API surface for SemVer purposes is the CSI gRPC verbs, the
StorageClass `parameters` schema, and the `volume_context` key
namespace. Internal Rust types and node-agent HTTP routes are not
covered by the stability guarantee.

## [Unreleased]

### Fixed — flint forge, the chart could not install itself

- **`helm install ./flint-forge-chart --set door.deploy=true` failed for
  everyone.** The chart's `door.yaml` passes `--git-only`;
  `values.yaml` pinned `1.46.0-forge.1`; and that is the one published
  tag whose `flint-hub-gateway` predates the `--git` → `--git-only`
  rename. The door crashlooped on `error: unexpected argument
  '--git-only' found`, `helm --wait` timed out with "timed out waiting
  for the condition" and named nothing. The chart's template moved
  forward with the source and the image tag it pins did not, so the
  chart was internally inconsistent — **found by installing it, which
  nothing did.**
- **The tag is five revisions behind.** `1.46.0-forge.6` has been
  published since 2026-09-05; the chart pinned `forge.1` from
  2026-09-04, which predates the restore fan-out (`7557d3c1`), the
  restore that held every pack twice (`827c5f90`), the lease renewer
  and orphan-MPU sweep (`2a213b01`) and the S3 transfer fixes
  (`62775105`). This is the second instance of the tag-provenance drift
  already recorded as open, and the first to be caught by a leg rather
  than by hand.

### Added — flint forge, a drill on the PUBLISHED artifact

- **`forge/e2e/published/`** — the path a user takes, which no other
  forge drill takes. Every existing leg runs `drill-<sha7>` images
  built from the checkout, deliberately and correctly; twelve
  falsifiers are green against those. A user gets whatever tag
  `values.yaml` names, pulled from Docker Hub, and until now that
  artifact had never been drilled at all. This leg overrides no image:
  it installs the chart as shipped, forces a real pull, and asks the
  release to clone, push durably, move a protected branch, lose its pod
  and serve a clone restored from S3 alone.
- **A tag bump is the whole fix**, established rather than assumed:
  `OVERRIDE_TAG=1.46.0-forge.6` runs every leg against a named tag, and
  turns P1 PENDING while it does, because a run that was TOLD its
  images is not evidence about which images the chart chooses.
  `values.yaml` now pins `1.46.0-forge.6` on the strength of it.
- **The leg's own first green depended on the cluster it ran on**, and
  finding that is worth more than the finding it was reporting. The
  `FlintRepo` CRD ships in the chart's `crds/`, so it does not exist
  until helm has installed; the rig was applied BEFORE helm with its
  output discarded, so the repository's apply failed silently. On a
  cluster where an earlier run had installed the chart the CRD was
  still present — **`helm uninstall` does not remove a CRD, and
  deleting namespaces cannot, because CRDs are cluster-scoped** — so it
  passed. On a genuinely fresh cluster there was no repository at all
  and the leg blamed the product. The rig is applied twice now, the
  second apply's status is checked, and the CRD is asserted as a
  precondition of P4 so a rig failure can never be reported as a
  product failure.
- **Two traps, each of which produced a confident wrong answer first.**
  `docker run <image> <binary> --flags` does not run `<binary>` — those
  become args to the image's ENTRYPOINT, here the operator, which fails
  on kubeconfig before parsing a flag; read as "the flag is accepted",
  it exonerated a broken image across two tags until the pod's own log
  contradicted it. And the registry-digest check first ran immediately
  after `helm --wait`, found two operator digests and called them "all
  images": the syncer and git images live in the repository's
  namespace, whose pod does not exist until the operator has
  reconciled. It now runs after the repository is Ready, across both
  namespaces, and requires all three images to have appeared rather
  than judging whatever turned up.

### Added — flint forge, `git propose`

- **A protected branch moves only by a push to `refs/for/<target>`**,
  which is Gerrit's plumbing by way of Gitea's AGit flow and which
  nobody — a person or a model writing git commands — emits from
  memory. The refusal already names the remedy, but git prints it as
  `! [remote rejected]` on a non-zero exit, and a harness that treats
  that as fatal without surfacing stderr hides the one sentence that
  says what to do instead. The forge-git image now carries
  `git-propose` on `PATH`, which git's own subcommand dispatch turns
  into `git propose` — no alias, no config, and it works in a clone
  made before the image existed. It is a convenience and never an
  authority: it pushes the same refspec by hand, and the merge is
  authorised on the server, twice, from the rendered policy.
- `docs/flint-forge-for-agents.md` now leads with the fact that **none
  of this is required to use forge**: a repository with no `branches`
  block is stock git, and `refs/for` is the price of protecting a
  branch, not of using the product.

### Fixed — flint forge, a doc comment describing a defect that was fixed

- **`export::ExportConfig::timeout_secs`** still explained itself in
  terms of the pre-`2a213b01` world, where the heartbeat was a timer
  arm of the serving loop's `select!` and a blocked export therefore
  stopped renewing the lease — a 300 s default timeout against a 60 s
  takeover threshold. The renewer has been its own task since, and it
  beats through an export because `Phase::Serving` is not a phase that
  must progress. What a blocked export costs today is pushes, bounded
  by the timeout and then by `backoff_secs`. The comment would have
  sent the next reader chasing a bug that no longer exists.

### Added — flint forge, the repack amplification, measured

- **`forge/e2e/repack/`** — how many bytes reach S3 per byte pushed.
  `maybe_repack` uploads every pack the snapshot does not name, and
  after `repack -a -d -b` that is one pack containing the whole
  repository, so **every 24 pushes a repository re-uploads all of
  itself**. Measured end to end against the shipped binary: **67x**
  steady-state amplification on a source-shaped repository (a 3.6 MiB
  re-upload for 2 KiB of content) and **3.4x** on a blob-shaped one
  (146 MiB for 2 MiB). The ratio misleads in both directions; the
  absolute statement is that a repack costs the repository, which the
  scale drill timed at 262 s for 10 GiB on real S3.
- **A control with the repack put out of reach** returns no spike at
  all, which is what makes the two numbers above attributable to the
  repack rather than to git. Its 5.1x floor is git rewriting the tree
  of the directory each push touches — present in both arms, and not
  something a repack policy could remove.
- **What `--geometric` would cost instead**, measured with pure git on
  the same repositories: 0.0 MiB against the full repack's 3.1 MiB
  (source) and 12.0 MiB against 156.1 MiB (blob). Two conditions found
  by measuring: geometric REFUSES on a repository with
  `pack.writeBitmaps` on, and `--write-midx` leaves a MUTABLE
  `multi-pack-index` at a fixed key in a directory whose every rule
  assumes immutable content-named objects. Recorded in the design, not
  built.
- **The probe's own first version reported a false win.** It sent
  git's fatal to `/dev/null` and read the command not running as
  "geometric rewrites 0.0 MiB". It now fails the leg instead — and the
  progression turned out to be over object counts rather than bytes,
  which is why both repository shapes are measured.

### Fixed — flint forge, the lease went quiet while the server worked

- **The heartbeat is its own task** (`lease::spawn_renewer`), running
  from the claim, so a restore, a batch and an export all beat through.
  It was a timer arm of the serving loop's `select!`, which could not
  fire while that loop was inside any of them: the scale drill measured
  the token silent for **125 s during a 10 GiB push and 141 s during
  the restore**, against a 60 s takeover threshold. A live pod,
  mid-work, could lose its repository to a challenger — design §5's
  window, on the wire.
- **The renewer is gated on PROGRESS, not on a timer.** While a phase
  that must move is reported, it renews only if the operation's byte
  counter advanced since the last renewal, so a wedged restore or
  upload lets the token go quiet and a challenger take over. An
  unconditional renewer would have traded "a live pod loses its
  repository" for "a dead one keeps it forever", which is the case the
  quiet polls exist for. `ComposeSpec` carries the counter, so the
  store's part loop reports through it.
- The lease and the fence now live in one `Hold`, shared with the loop
  through a watch channel: the renewer being deposed IS the loop's
  exit, from whatever it was awaiting. `/status` reports the renewer's
  last renewal and the progress counter.
- **Measured under injected latency** (`forge/e2e/latency/` P3a-c):
  through an 8.8 s restore the token rotates 7 times, longest silence
  1.2 s, against **0 rotations and 7.0 s of silence** on the pre-fix
  binary; through an 18.3 s multipart push, 10 rotations against
  **11.7 s of silence**; and a restore stalled mid-flight goes quiet
  and resumes, where the pre-fix binary never rotates at all.

### Fixed — flint forge, an interrupted push left its parts billed

- **`sweep::abort_orphaned_uploads`**, after the claim and between
  batches. A push killed inside its multipart upload left parts no
  `Complete` would ever claim, billed as storage until a hand abort —
  the scale drill measured **384 MiB from one interrupted 2 GiB push**.
  Forge had no sweep; lean and the tier have had one since A9. There is
  no grace because there need not be: at both moments nothing of this
  process's own is in flight, so anything pending is a predecessor's or
  a deposed straggler's. Hygiene, not a gate — a listing the credential
  cannot make is logged and retried, never a crash loop.
- **P4 takes both samples**, pending after the kill and zero after the
  restart, with the kill placed inside the upload by observing
  `list-multipart-uploads` rather than a guessed sleep: a sweep
  inferred from a zero is not observed. On the pre-fix binary the parts
  survive every restart.

### Fixed — flint forge, the restore ran in series twice over

- **The restore now fans out across files and chunks together**
  (`packio::fetch_all`): one bounded set of ranged GETs, `fanout` ×
  `FETCH_CHUNK` in flight, all-or-nothing, every temporary renamed only
  after every chunk of every file has landed. Before this the ranged
  fetch (`827c5f90`) ran its chunks one at a time and the restore ran
  its files one at a time — at the design's 10 GB envelope, 1,280 round
  trips in a single stream, at every pod start. Measured with injected
  latency: a 33-file restore costs **14.3 round trips** at fanout 4
  against **38.6** serial (2.3 s vs 4.9 s at RTT 100 ms). The price is
  memory: ~20 MiB per chunk in flight, so the restore's flat floor is
  43 MiB at fanout 1 and 104 MiB at the default 4 (the large-repo leg,
  160 MiB pack), still independent of the pack.
- **`ForgeConfig::fanout` is now read.** It was declared at 16,
  documented as bounding "pack uploads and restore fetches", and used by
  nothing: the batch had a hard-coded 4 and the restore had no bound at
  all. It now bounds both, defaults to 4 (the RAM-motivated value the
  batch already used), and is `FLINT_FORGE_FANOUT` on the syncer.
- **Sibling temporaries no longer collide.** The fetch's temporary was
  `dest.with_extension("part")`, which maps `pack-X.pack` and
  `pack-X.idx` to the SAME `pack-X.part`. Harmless while files were
  fetched in series; a corrupted index the moment two siblings were in
  flight together. The temporary is now the destination name with
  `.part` appended.
- **The memory double counts get_range calls in flight** and can delay
  every one of them, so a fan-out's bound is testable from both sides:
  the new tests read a peak of exactly 1, 2 and 4 for fanouts 1, 2 and 4,
  and a failed chunk in one sibling lands none of the set.

### Added — flint forge, a latency leg

- **`forge/e2e/latency/`** — the round trips a push and a restore cost,
  measured rather than counted. Every other forge drill runs on loopback
  MinIO, where a round trip is a millisecond and only the request COUNT
  is visible; the concurrent sibling upload (`62775105`) shipped with
  its win "structural rather than measured". This leg puts toxiproxy in
  front of the same MinIO with a latency toxic each way and runs the
  same binary at `FLINT_FORGE_FANOUT=4` against `=1` — what the code did
  before — with arms interleaved and the position changing, a null leg
  at RTT 0, and a fit across RTTs that must scale. A push costs **5.1
  round trips** with the fan-out and **7.1** without: exactly the
  request count, nothing hidden.
- **Run against the pre-fix binary as a control** (`cda4b21e`, no knob,
  sequential restore): both nulls pass and both measured legs FAIL,
  which is what makes the green run worth anything. Both logs are under
  `forge/e2e/results/`.
- **The shared rig now owns the tri-state verdict** (`inconc`, exit 2
  on any inconclusive leg) and the binary-freshness precondition, moved
  out of the large-repo leg so both use one copy.

### Added — flint forge, a large-repository leg

- **`forge/e2e/largerepo/`** — the size regime the suite could not
  reach. Every other forge drill runs under 12 MiB against a 64 MiB
  whole-put ceiling, which is why three shipped defects were invisible:
  the multipart upload had never been executed by any test or leg, the
  restore held every pack twice, and a restore slow enough to outlast
  the takeover window loses the repository. L1 proves the composed
  upload from the outside, by the `-<partcount>` ETag suffix S3 and
  MinIO give a multipart object and never give a whole PUT. L2 holds
  peak memory under the pack size, and also fscks the restored
  repository — a cheap restore that produces the wrong bytes is worse
  than an expensive one.
- **Run against the pre-fix binary as a control**, which is the only
  reason the leg is worth anything: L2 reads 38.9 MiB for a 96 MiB pack
  on the shipped code and **202.5 MiB (2.11x)** with the whole-object
  read put back — independently reproducing the 2.05x measured in
  §5 by a different instrument.
- **A staleness precondition, because this bit for real.**
  `cargo build --bins` SILENTLY SKIPS `flint-forge-syncer`: it carries
  `required-features = ["s3"]`, so a plain build leaves the previous
  binary at exactly the path the rig uses, and the drill reports green
  about code that is not in the tree. The leg refuses to run when any
  source under `forge/syncer` or `crates/flint-store` is newer than the
  binary, and names the rebuild.
- **`INCONCLUSIVE` is not `PASS`.** A leg that could not measure what it
  exists to measure exits 2 rather than being folded into "0 failed"
  (the rule `tests/k8s/oci-ab` earned).

### Fixed — flint forge, a restore held every pack twice

- **The restore read whole objects and needed ~2x the repository in
  memory.** `fetch_to_file` called `get_whole`, which holds the SDK's
  aggregation buffer and the contiguous `Bytes` it returns at the same
  time: measured at a flat **2.05x of object size** across 256 MiB to
  2 GiB. At the 10 GB envelope §5 sizes, ~20.5 GB — on a path that runs
  at **every pod start**, so under a memory limit it is not a slow
  restore but an OOMKill and a crash loop with no other symptom. The
  fetch is now ranged and etag-pinned: **38-40 MiB flat** from 512 MiB
  to 2 GiB (27x lower at 512 MiB, 53x at 1 GiB), byte-identical output.
  This takes design decision 9, which had been waiting on exactly this
  measurement.
- **A cut connection no longer restarts the file.** The retry budget is
  per chunk, so a transport failure partway through a multi-GiB pack
  costs one range, not everything already written.
- **A pack that moves under a restore is refused, not adopted.** The
  deliberate divergence from `tier::hydrate`, which adopts on a 412
  because a tier's object legitimately moves. A forge pack is immutable
  and content-named, so a moved etag means something wrote a pack file
  that is not the pack it is named for.
- **`list_pack_files` no longer discards the size and etag** the LIST
  already returned, so the restore fetches pinned ranges without a HEAD
  per file. The sweep's own HEAD is untouched: it reads age at the
  delete on purpose, never from a listing that is by then as old as the
  sweep.

### Known — flint forge, not fixed here

- **The lease is not renewed during restore.** `restore.rs` never calls
  `lease::renew`, and the heartbeat does not start until restore
  returns. Takeover is `QUIET_POLLS` x `heartbeat_secs` = 60 s of a
  non-rotating token, inside the 40-80 s §5 gives for a 10 GB restore,
  so a large repository can be taken over while its pod is alive and
  mid-restore. Same shape as the export/heartbeat item recorded in §17.

### Added — flint forge, the git↔S3 transfer path

- **`packio` has coverage.** The module that moves every byte between
  the repository and the bucket had none. Seven tests now cover the
  part grid, both upload paths and the fetch. Four were run against a
  deliberately broken variant to prove they can fail: three on the grid
  arithmetic, and one proving the store really does enforce the part
  minimum rather than accepting a grid S3 would reject.
- **The multipart path is exercised for the first time.** The ceiling
  is 64 MiB; the largest payload anywhere in the suite was 12 MiB, so
  every composed upload — the ordinary case for a repacked repository —
  shipped untested. A 65 MiB pack now round trips byte-identical, and
  the grid is checked at sizes up to S3's 5 TiB maximum without
  materialising them.
- **The per-push S3 protocol is pinned.** Three tests hold a batch to
  the cost section 4 documents, so a regression that adds a round trip
  to every push is visible here rather than on a request bill.

### Fixed — flint forge, S3 request cost and transfer I/O

- **`HEAD` was re-uploaded on every push.** Section 3 calls it
  "derived, once"; the code restated `ref: refs/heads/main` in every
  batch, a fifth of the fixed per-push cost. Published on change only.
  Fixed cost per batch 5 → 4 requests; steady state 8 → 7 per push.
- **Pack siblings were uploaded in series.** They are independent,
  immutable, content-named keys written unconditionally, so nothing
  orders them; each one added an S3 round trip to the batch's
  dependent chain. Now uploaded with bounded concurrency (4 — the
  bound is RAM, since `put_whole` holds the whole body).
- **Every pack under the ceiling was read from disk twice**, once to
  checksum and once for the body. One read now serves both. Both reads
  also moved off the async runtime, which carries the lease heartbeat.

### Fixed — flint-store, a test double weaker than the backend

- **`MemoryStore` accepted any checksum on a composed upload.** Real S3
  validates the full-object CRC64-NVME at CompleteMultipartUpload; the
  double never looked at `spec.crc64`, so every multipart upload in the
  tree — forge's large packs, lean's chunks — had its checksum accepted
  untested and a wrong CRC would first have been seen by a real bucket.
  The double now validates, and no existing test needed changing.

### Added — flint forge, composition drills (C1-C5)

- **A local drill suite for two products on one bucket**
  (`forge/e2e/composition/`, MinIO + the real binaries). Everything
  else in `forge/e2e/` tests forge against itself; these test forge
  meeting lean, and forge meeting a read-write passthrough mount.
  First run 30 passed / 12 failed; now 45 passed, 0 failed, 8 accepted. Every control and precondition green, so the failures are
  findings. Recorded in the design doc, section 17.

### Known — what the composition drills found (no code changed yet)

- **"One prefix, one writer" is not enforced across products.** forge
  arbitrates on `<prefix>/git/epoch`, lean on
  `<prefix>/.flint/lean/epoch`. Pointed at one prefix both acquire at
  epoch 1 with no 412, no fence and no log line, while the drill's
  forge-vs-forge and lean-vs-lean controls both contend on the same
  rig. `arbitrate` reasons over `&[FlintRepo]` and cannot see a lean CR.
- **A foreign write into the export prefix is never repaired.** The
  claim at `export.rs:27` that "a foreign write into its prefix is
  overwritten by the next export" is false: the barrier diffs a local
  scan against a local baseline and consults only the manifest pointer
  remotely, so an object changed behind it is not in the diff. Measured
  across two further exports that each republished what git changed.
- **A foreign delete is refused loudly but never restored.** The
  asymmetry is the point: the overwrite, which looks milder, is the one
  that propagates silently.

### Changed — the composition suite records accepted conditions instead of standing red

- **Four conditions the drills found are not being fixed**, three by
  decision and one because it cannot be fixed where it is observed
  (design doc section 17 carries the table and the reasoning). The
  suite now reports those eight legs as `KNOWN <id>` rather than
  `FAIL`, totals them separately, and exits green while only accepted
  conditions are outstanding — a suite that is permanently eight-red is
  one nobody reads, and a real regression would hide among them.
  A leg whose accepted condition stops reproducing reports `STALE` and
  fails, because a record that has quietly become wrong also needs a
  human. Suite: 45 passed, 0 failed, 8 accepted.

### Added — a shared prefix between two products is no longer silent

- **Each product now probes the other's lease cell at claim time**
  (`flint_store::layout`), and says what it found: the neighbour's kind,
  its cell, its holder, whether it is holding or released, and which
  field would move it. One exact-key read — not a listing, so it finds
  a writer rather than the litter one leaves, and a nested export is not
  mistaken for a collision. Detection, deliberately not enforcement:
  prevention belongs to whatever assigns prefixes, and refusing here
  would turn a diagnostic into an outage the first time a stale cell
  outlived its workspace. What it buys is the property an external
  control needs to be safe to rely on — the control still prevents, and
  you find out when it did not. A writer alone on its prefix stays
  quiet, which is drilled, because a check that cries wolf gets switched
  off. A published mirror skips the probe and its publisher does it once
  at startup instead: forge spawns a barrier per export, and
  `run_barrier` echoes only child lines containing "barrier", so a
  warning raised there would have been discarded — a check whose output
  is thrown away is worse than none, because it reads as coverage.

### Fixed — flint lean, a reader adopted a foreign write into a published mirror

- **A workspace published by one writer is now marked as such, and its
  readers refuse rather than adopt.** `checkout`'s etag-pinned refusal
  was guarded by `if pinned`, so it fired only under a gated citation;
  for the cadence/hybrid manifests forge's export writes, the next arm
  took over and did an explicit S3-wins adopt — right for a workspace an
  agent works in, where an object past its citation means a human wrote
  newer bytes, and exactly inverted on a mirror, where it means a
  stranger did. A reader therefore copied bytes no manifest cites into
  its tree and reported success (composition drill C4). `LeanManifest`
  and `Pointer` now carry `sole_writer`, set by the installing pass from
  `FLINT_SYNC_SOLE_WRITER`; forge's export sets it. The flag is in the
  manifest rather than in the reader's configuration on purpose — a
  reader that must be configured to be careful will be deployed without
  it. Ordinary workspaces keep the S3-wins arm, guarded by its own test.
  Not reached, and not reachable at the reader: a manifest-less reader
  (a passthrough or lite mount) has nothing to check against and still
  takes the foreign bytes.
- **`export.rs`'s claim that a foreign write "is overwritten by the next
  export" is withdrawn.** It was false — the barrier diffs a local scan
  against a local baseline — and the comment now records what C3
  measured instead.

### Fixed — flint forge, a blocked export took the repository with it

- **A second writer on the export prefix wedged the whole repository.**
  `flint-sync`'s claim loop never gives up, `run_barrier` awaited it
  with no timeout, and `maybe_run` is awaited inline in the serving
  loop whose `select!` also carries the lease heartbeat. A read-write
  lean workspace mounted over the export prefix therefore stopped
  forge's pushes *and* its lease renewal on a **different** prefix —
  silently, because the child's stderr is only read after it exits, and
  with the status listener still answering Ready from its own task.
  `run_barrier` now spawns with `kill_on_drop` and waits under
  `FLINT_FORGE_EXPORT_TIMEOUT_SECS` (default 300, the export floor's
  default); on elapse the child is killed and `ExportBlocked` names the
  prefix and points at `<export>/.flint/lean/epoch`. A blocked export
  then holds off, doubling per consecutive failure and capped at an
  hour, because the blocker is a misconfiguration that stands until
  someone clears it — without that the loop re-enters the doomed
  barrier every batch and rebuilds the same outage. Residual: the loop
  still stalls for up to one timeout, so the structural fix is to move
  the export off the serving loop.

### Fixed — flint forge, the remaining falsifiers

- **The legible export froze permanently after the first restart.**
  lean makes every upload conditional and PARKS a file whose etag it
  did not last write — right for lean, whose baseline lives on the
  volume with the workspace. Forge's export has no volume: the baseline
  sat on the pod's `emptyDir`, so the first restart destroyed it, every
  object then looked foreign, and every file parked. Permanently,
  because nothing rebuilds a baseline — the published workspace froze
  while `main` moved on. The cluster run found `README.md` still holding
  the first seed commit's text, 164 files parked, `up=0`. The baseline
  is now preserved to the bucket after each successful barrier and
  rehydrated at startup. It prevents the loss; it does not repair a
  prefix already stuck, which still needs the export prefix cleared.
- **The agent image could not use the LFS the server offers.**
  `flint-forge-git` is what agent pods run and it shipped no `git-lfs`
  client, so the batch API answered correctly and no agent could reach
  it — with multi-modal agents being the reason forge carries LFS at
  all. Added to the image.

### Fixed — flint forge, the clone storm

- **A restore came back with no bundle advertisement.** `advertise`
  writes into the repository's LOCAL git config, and both that config
  and the bundle module's own record live on the pod's `emptyDir`,
  while the bundle object and the snapshot naming it live in the
  bucket. So every restart served a repository whose bundle existed,
  was paid for, and was advertised to nobody — until `every_secs`
  elapsed and a new one was cut. For forge that window is not an edge
  case: a repository that idles to zero restores at the moment a clone
  storm wakes it, which is exactly when the lever should be pulled.
  `bundle::readvertise` now re-signs the snapshot's bundle during
  startup, before the server reports Serving.

### Fixed — flint forge, first cluster run

Three defects that only a real cluster could find. All three were in
code whose unit tests were green.

- **A `--git-only` gateway.** The file API's two credentials are built
  before the git door is wired and both refuse to start without a
  value, so a forge-only cluster — no hubs, no root key, no inbound
  token — could not run `flint-hub-gateway` at all. The chart's own
  NOTES described a deployment that could not exist. `--git-only` now
  serves the git door alone and touches nothing about `FlintShare`, so
  a forge cluster installs no CRD it has no use for.
- **A git-only door had no probes.** `/healthz` and `/readyz` live in
  `proxy::routes`, which `--git-only` does not mount, so both 404'd and
  kubelet killed a door that was serving correctly. `git::health_routes`
  now carries them, with the same split: liveness up immediately,
  readiness False until the repository cache has listed.
- **The NetworkPolicy blinded the operator that wrote it.** A policy
  selecting a pod is default-deny for every port it does not name, and
  it named only the git port — so the operator's own `/status` poll was
  denied, and every guarded repository sat in `Starting` forever with
  the pod 2/2 Ready, the syncer reporting `serving`, and nothing
  logging an error. The policy now admits the operator to the status
  port. The unit test that was there asserted `ports.len() == 1, "the
  git port only"`: it passed, because it was encoding the bug.

- **A new repository could never create its default branch.** A direct
  push to `main` was refused because it is protected; a `refs/for/main`
  merge request was refused with `no such merge target` because `main`
  did not exist yet. Between them a fresh repository was unusable from
  birth. A merge request into the DEFAULT branch now creates it — which
  is within the authority `mergeInto` already checked — while a merge
  request into any other missing ref is still refused, so this cannot
  be used to conjure arbitrary refs. Every merge test seeded `main` by
  direct push first, which is why none of them could see it.

Also: `flint-forge-chart` can now render the door itself
(`door.deploy`), with the RBAC it actually needs — `flintrepos` and
`tokenreviews`, neither of which the lite chart's gateway role grants.


### Added

- **`s3.csi.chert.us`: the two identity modes nothing had exercised**
  (`s3csi/e2e/aws-identity.sh`, A1-A3), on EC2 against a real bucket.
  `ambient` on a LEAN workspace WORKS: the syncer is handed nothing, its
  own credential chain checks out the workspace and publishes, and the
  worker carries no key, no token and no brokered credential. Like the
  passthrough leg before it, A1 GATES on the platform rather than
  judging it — a pod with nothing injected must first obtain an identity
  from the default chain, or the leg is recorded as skipped.

  The suite carries the control that separates an exchange from a
  fallback: the broker logs one line per issue, and a workspace that
  mounts while the broker issued NOTHING for it did not do a
  web-identity exchange — it fell through its default chain to another
  identity, which on a node whose instance role can reach the bucket
  looks exactly like success. Without that control this suite reported
  a pass it had not earned.

### Fixed


- **flint forge: git LFS**, for the multi-modal case. A pack is
  delta-compressed and rewritten whole by `repack -a`, so weights,
  video and audio committed as ordinary blobs make every clone, repack
  and restore pay for them again; with `spec.lfs.enabled` the bytes
  live at `<keyPrefix>/lfs/objects/<oid>` — immutable, content-named,
  the layout the packs already use — and the pointers stay small in
  git. The batch API is served by the syncer, because it needs the
  bucket credentials the door deliberately has none of, and **the
  objects never cross the server**: the response hands the client a
  presigned URL, so a 4 GB checkpoint goes straight to the object store.
  `flint-store` grows `presign_put` behind a presigning client that
  sets `RequestChecksumCalculation::WhenRequired` — the SDK's default
  adds a checksum header to PutObject, a presigned URL signs the
  headers it was built with, and a git-lfs client does not send it, so
  S3 answers 403 with nothing in it about checksums. An object already
  in the bucket is offered no upload action, which is the dedupe that
  makes LFS cheap. Nothing collects LFS objects, deliberately.

- **flint forge, phase 5: the fleet levers.** Clone bundles, the
  agent-branch pruner, and `docs/flint-forge-for-agents.md`. The syncer
  cuts a bundle on a floor, uploads it beside the packs, advertises a
  presigned URL through `uploadpack.advertiseBundleURIs`, and re-signs
  at half the URL's TTL — so a thousand agents cloning at once pull the
  bytes from the object store instead of through one pod's NIC, which
  is what binds first. `flint-store` grows `presign_get`, defaulted to
  a refusal so a backend that cannot sign says so rather than
  advertising a URL that will not resolve. The pruner takes an agent
  branch only when it is already contained in the default branch AND
  has been quiet past a TTL: age alone would delete somebody's
  unfinished work, and a merge that just landed must not take the
  branch out from under the agent still pushing to it. Its deletions go
  through the ordinary batch — one CAS, one ref transaction. Bundles
  are swept by the same four rules as packs. Both levers are off unless
  asked for.
- **`spec.wipSnapshots` is gone from `FlintRepo`**, replaced by
  `spdk-csi-driver/docker/forge/wip-snapshot.sh` and a section in the guide. Forge owns
  repository servers, not agent pods, and injects nothing into them, so
  a field asking for a sidecar was a field the operator would silently
  ignore. The script is plumbing — `write-tree`, `commit-tree`, `push`
  — because `git commit` against a throwaway index still moves HEAD.

- **flint forge, phase 4: the legible export.** A `FlintRepo` with
  `spec.export` publishes a chosen ref's tree as a lean workspace, so
  lite, lean and passthrough readers can mount what forge holds with no
  forge code in them. Forge writes no manifest: it materialises the tree
  and runs the shipped `flint-sync barrier` over it, inheriting lean's
  ordering — upload, CAS, deletes LAST — rather than re-deriving the one
  thing a crash makes load-bearing. The tree is updated by a two-tree
  `read-tree -m -u` against an index kept beside it, not by `git archive
  | tar -x`: that pipeline rewrites every file, so the next scan
  re-uploads the whole tree, and it leaves deleted paths behind, so the
  export would publish files the ref no longer has. The export runs
  after the push is acknowledged and never CASes the snapshot — it
  stashes its commit and the next batch's single CAS carries it.
  `spec.export.refs` must name exactly one ref, refused at admission,
  because a lean workspace is one tree.

- **flint forge, phase 3: the operator** (`forge_operator`,
  `flint-forge-operator`, `flint-forge-chart`, and the two server
  images). A `FlintRepo` becomes a ConfigMap carrying its branch
  policy, a headless Service, and a Deployment of one pod — the syncer
  plus nginx/fcgiwrap/`git http-backend`, 25m/32Mi each, `Recreate`,
  an `emptyDir` cache and no PVC anywhere. A slim controller rather
  than a trim of lite's, which is 4,000 lines of PVC, hibernate,
  reprovision and expand; what is shared is the code that carries a
  lesson — the wake-stamp skew rules, extracted so both front ends
  enforce one of them, and `hubstatus::suspendable`, which forge's
  `/status` was written in the shape of. One idle rung, because an
  `emptyDir` cache makes `Suspended` and `Hibernated` the same state.
  The operator arbitrates the bucket subtree (earliest CR wins, ties on
  uid so every replica agrees; an export prefix is a claim too, which
  the CRD's CEL rule cannot see), polls the server's own `/status`, and
  never treats a failed poll as idle. The syncer now releases its lease
  on SIGTERM, so a successor claims at once instead of waiting out six
  quiet polls, and re-reads its branch policy between batches, so a
  policy edit takes effect on the next push without rolling the server
  and dropping every clone in flight. `crdgen -- forge` emits the CRD.

- **flint forge, phase 2: the door and the branch policy.**
  `lite_gateway::git` serves `/git/<namespace>/<repo>.git` with a
  `Door::Git` arm on the gateway's existing resolve-wake-dial decision.
  An agent authenticates with HTTP basic whose password is its own
  projected ServiceAccount token (audience `forge.chert.us`); the door
  reviews it, caches the verdict briefly so a thousand clones are not
  four thousand `TokenReview`s, checks `spec.consumers`, and forwards
  the verified principal as `X-Remote-User` while the credential itself
  stops at the door. `Git-Protocol` is forwarded, without which every
  clone silently degrades to protocol v0 and bundle URIs cannot work at
  all. The three routes stream both ways with no length limit — the
  file API's own upload route would answer 411 to a push. The upstream
  URL is the CR's endpoint plus a `&'static str`, so the caller's path
  segments are a lookup key and never a path. New `FlintRepo` CRD
  (`forge_operator::crd`) carrying the identity, the consumers and the
  branch policy; the door is off unless `flint-hub-gateway --git` is
  set, and off means the CRD is neither listed nor watched.
  `pre-receive` applies the rendered branch policy at the edge for the
  message, and the syncer applies the same document at the writer for
  the guarantee — a repository whose hooks are misconfigured must not
  become an open one. `flint-forge-credential` is the agent-side git
  credential helper.

- **`s3.csi.chert.us`: a hardening suite on real nodes**
  (`s3csi/e2e/aws-hardening.sh`, L1-L7) for the paths the passthrough
  suite left: a lean workspace under a graceful node reboot and under a
  hard power-off, a broker outage that outlives the credential, 120
  tenants on one node through a plugin roll, a 100,000-file workspace,
  a one-GiB workspace's real ceiling, and what a preserved undrained
  tree costs. Measured on all-spot EC2 against a real bucket; all seven
  legs green.

  Three properties the suite had to establish to test the preservation
  path at all, each worth knowing: a killed syncer is relaunched and
  drains normally (so killing one costs nothing), a worker's termination
  grace is derived from its workspace's `floorSecs` (an hour's floor
  gives a 3681 s grace, and an unpublish waits that budget out before
  giving up on a drain), and a workspace filled to its own ceiling
  cannot attest a drain because the attestation is itself a file.

  At 100,000 files: the tree publishes in 100 s (~1,000 objects/s)
  behind a 2 KB pointer over 24 chunks, a cold checkout completes in
  83 s *with a node-plugin roll landing mid-checkout*, a five-file
  change writes five objects and one chunk rather than the whole
  manifest, and a successor supersedes a frozen holder in 67 s and
  rotates the 100,006-entry manifest in about one second, losing no
  entry. The mid-checkout roll is the case the kind rig could never
  reach: there a 200-file checkout finishes inside the roll's window,
  so the leg passed without ever testing what it claimed.

  At 120 tenants on one node: all Running in 71 s, one worker each,
  every sampled tenant reading, the node-plugin roll under them
  restarting no worker and unmounting nothing, a new tenant admitted
  after it, rotation continuing, and everything reclaimed in 44 s.
  About 28 MiB of node memory per tenant-plus-worker; the plugin's own
  resident set stayed at 64-82 MiB.

### Changed

- **`identity.mode: webIdentity` is now REFUSED on a
  `FlintPassthroughMount`, by name.** It is honoured on a
  `FlintLeanWorkspace` and cannot be on a passthrough mount: Mountpoint
  for Amazon S3 has no web-identity credential provider. Its
  configuration guide lists instance profiles, ECS task roles, `~/.aws`
  profiles, static keys and `--no-sign-request`, and neither
  `AWS_WEB_IDENTITY_TOKEN_FILE` nor a custom STS endpoint. Measured
  against mount-s3 1.24.0: given a JWT token file, a valid `arn:aws:`
  role ARN and an STS endpoint pointing at a listener under our control,
  it sends that listener nothing while `curl` from the same container
  reaches it — the exchange is never attempted. Previously the CR
  accepted the mode and the tenant met a mounter that died with "No
  signing credentials available", which names nothing it can act on.
  The refusal names the reason and the mode to use instead. The lean
  syncer's client is the Rust AWS SDK, which does implement the
  provider and does honour the endpoint override, so it completes the
  exchange against the broker's facade over plain http — which the
  design had assumed impossible.

- **The chart no longer promises an ordering Kubernetes is not
  configured to give.** `workers.priorityClassName` was documented as
  the ordering mechanism for node shutdown — kubelet terminating by
  priority so a worker outlives the tenant still writing into it. That
  ordering *is* kubelet's graceful node shutdown, and it is off unless
  `shutdownGracePeriod` is set, which it is not on a stock kubeadm node
  (0s on Amazon Linux 2023). A lean workspace still drains on a reboot,
  because the syncer drains on SIGTERM whoever sends it and systemd
  signals everything on the way down — measured: the unpublished write
  reached the bucket 5 s after `systemctl reboot`, in a generation
  marked `boundary_source: drain`. What is lost without the kubelet
  setting is the *ordering* and the *budget*. The values file now says
  so, gives the kubelet configuration, and names the one case nothing
  saves: a machine that simply stops, where everything since the last
  publish is gone and `floorSecs` is what bounds the exposure.
- **A cloud instance termination is a graceful shutdown, and the
  passthrough suite no longer calls it a node loss.** `terminate-
  instances` hands the guest an ACPI power button; the drain runs. The
  leg is renamed to what it measures, a planned termination, and points
  at the hardening suite's power-off leg for the hard shape.
- **The drill's bucket observer is pinned to the control plane**
  (`s3csi/e2e/rig-s3.yaml.tpl`). It had been scheduled onto the worker
  a node-loss leg destroys, so six assertions failed on an empty answer
  while the driver had behaved correctly throughout: a window that dies
  with what it watches reports "nothing", and nothing reads as "the
  object is not there".

### Known

- **A preserved undrained tree is kept forever, and nothing reclaims
  it.** When a syncer cannot attest its drain the driver moves the tree
  aside rather than deleting it, which is the right call and is
  deliberate (`state.rs`: an undrained tree is never removed). There is
  no expiry, no cap, no reclaim verb, no listing, and no event as they
  accumulate — only the `DrainNotAttested` event written with each one.
  On a node with a small root disk they add up: three of them took
  1.1 GiB on an 8 GB node during this campaign, kubelet crossed its
  DiskPressure threshold, and it evicted *unrelated* tenants. A
  preserved tree is also an unmounted ext4 image rather than a
  browsable directory, so recovering it means a read-only loop mount —
  a procedure the chart now documents. Sizing guidance and that
  procedure are in the chart's notes; bounding the retention is not
  built.

- **flint forge, phase 1: the per-repo syncer** (`forge/syncer`, the
  `flint-forge` crate; design of record
  `docs/plans/flint-forge-design.md`). A git server per repository with
  S3 behind it: agents are stock git clients, the server is real git,
  and this crate is the one process that stands between the repository
  on local disk and the bucket. Not wired into any image or chart yet.
  `receive-pack` serialises nothing between pushes, and under
  `receive.procReceiveRefs` git performs no old-oid check and no
  `denyNonFastForwards` for the handed-off commands, so the hook
  decides nothing and relays: the syncer batches the pushes that arrive
  together, judges each command against both the local ref and the
  last-synced snapshot AND the batch's own running view, runs
  `refs/for/*` merges and packs the loose objects they create, renews
  the lease once, uploads the packs, CASes the snapshot once, applies
  one `update-ref` transaction, and only then reports. A snapshot 412
  under the writer lock can only be a second server, so it fences —
  reads included. The lease heartbeats on a timer, adopts its own lost
  renew response rather than fencing on it, and a successor rotates the
  snapshot before serving. Restore re-reads once before believing a
  pack is missing and refuses to serve a repository it cannot `fsck`.
  27 tests, including four that run a real `git push` through the real
  `proc-receive` hook.

- **`s3.csi.chert.us`: the passthrough suite on real nodes**
  (`s3csi/e2e/aws-passthrough.sh`, fourteen legs). On an all-spot EC2
  cluster against three real buckets (plain, SSE-KMS by bucket default,
  cross-region): throughput, a 5000-object prefix, sixteen tenants on
  one node, a container restart, a kubelet restart mid-read, a real node
  reboot, node loss under a Deployment tenant, a thirty-minute rotation
  soak, ambient identity, SSE-KMS, cross-region, a VPC gateway endpoint,
  an S3 partition, and a broker roll mid-read. Ambient identity is gated
  on the platform rather than judged: a pod on the node with nothing
  injected must obtain an identity from the default credential chain,
  otherwise the leg is recorded as skipped with the chain's own words.
  The chart's notes now say what a lost node does to a DaemonSet roll
  on a cluster without a cloud controller, and that the driver neither
  proxies nor injects a platform's metadata credentials.

### Fixed

- **A mounter that died before serving lost its last words.** The node
  plugin sees a dead FUSE endpoint the instant its descriptor closes and
  read the worker's `mount.error` before the supervisor had written it,
  so the tenant's FailedMount event said only "mounter died before
  serving the mount" while the file held the credential chain's own
  verdict. The plugin now waits up to three seconds for the file and
  the event carries it. Found on EC2 with an ambient mount whose
  platform could not complete the chain.


## [1.45.0] - 2026-09-04

The passthrough and lean sidecar-injection webhooks are gone. Both
front ends are delivered by one CSI node DaemonSet, `s3.csi.chert.us`. Every
machine identifier moved off `flint.io` to `chert.us`.

### Changed

- **Machine identifiers moved from `flint.io` to `chert.us` (BREAKING).**
  The API group is `chert.us/v1alpha1`, the S3 CSI driver is
  `s3.csi.chert.us`, and every label, annotation and StorageClass parameter
  key moves with them — `nfs.chert.us/server-ip`, `pnfs.chert.us/layout`,
  `chert.us/share`, `chert.us/mount`, and the rest. The product is still
  Flint: crate and binary names (`flint-s3-csi-node`, `flint-s3-broker`,
  `flint-sync`), Docker Hub repositories, chart names (`flint-s3-csi`,
  `flint-lean`, `flint-passthrough`) and the CRD kinds
  (`FlintPassthroughMount`, `FlintLeanWorkspace`, `FlintShare`) are
  unchanged. Identifiers carry the bare domain rather than
  `flint.chert.us` deliberately: a future brand change should touch
  charts and docs, never an API group. `home:` on every chart is now
  <https://flint.chert.us>, with the repository moved to `sources:`.

  This project's SemVer surface names the StorageClass `parameters`
  schema and the `volume_context` key namespace, so this is breaking by
  the definition at the top of this file — and no migration is attempted.
  `flintshares.chert.us` is a different CustomResourceDefinition from
  `flintshares.flint.io`, so an existing cluster would gain an empty new
  CRD while its old CRs sat untouched under the old one; a PV annotated
  `nfs.flint.io/server-ip` is simply invisible to a driver reading
  `nfs.chert.us/server-ip`. On a cluster already running Flint: drain the
  volumes, uninstall the old charts including their CRDs, then install
  the new ones. The node driver's plugin directory moves as well
  (`/var/lib/kubelet/plugins/s3.flint.io` becomes `.../s3.csi.chert.us`), so
  no mount may be live across that step.

  Drill captures under `tests/chaos/artifacts/` keep the old identifiers
  on purpose; they are dated evidence, not current configuration. See
  `tests/chaos/README.md`.
- **Lean manifests are chunked, on by default, and the pointer's wire
  format changed shape (forward-only).** `.flint/lean/current` is now a
  tagged union: either `entries_key` + `entries_seq` (one whole-manifest
  generation) or `chunks` (a list of content-addressed chunk refs); a
  document carrying both or neither is refused. A publish costs
  O(changed) — a chunk whose address is already in the pointer is not
  sent — and a small project is one chunk, so it pays what the single
  generation already cost. A workspace migrates on its next barrier;
  legacy layouts stay readable forever. **A pre-1.45.0 `flint-sync` or
  gateway cannot read a pointer a 1.45.0 writer publishes** — the
  migration is fail-closed by design, a one-way format decision per
  workspace, not a tuning change; roll the readers before the writers.
  Superseded chunks are reaped after every successful install, and the
  reaper judges a candidate's age by a HEAD immediately before the
  delete, never from the listing it started with (an adopted chunk is
  refreshed, and a pre-fence listing cannot see the refresh). Known and
  deliberate, unchanged by this release: the arbitration half of design
  §6.3, no history window for chunks (needs pointer snapshots, §9), and
  the gateway's HITL CAS not carrying `prev_chunks`. Design of record:
  `docs/plans/flint-lean-chunked-manifest-design.md`.
- **Drill leg S12** closes the lean gap the CSI cutover opened: the
  in-band publish verb, driven from the tenant pod. A tenant writes a
  nonce to `.flint/publish` in its own mount and the leg holds the ack
  to that nonce within 90 s, `status: ok`, a manifest that advanced and
  cites the new file, and the bucket carrying the bytes — over a
  workspace whose 1-hour floor means the cadence cannot have done it.
  It also asserts §3.2's replacement for the in-pod exec surface that
  CSI removed: `flint-sync ctl` is unreachable for a tenant now, so the
  leg proves the control socket is a socket in the tenant's own view of
  the tree, on the SAME inode the worker bound, and that
  `flint-sync ctl status` and `flint-sync status` answer in the worker.
- **The lean manifest is an immutable generation plus a small mutable
  pointer.** `<prefix>/.flint/lean/manifest` was ONE object holding every
  entry, read whole and rewritten whole under `If-Match` — ~264 MiB at 1M
  entries, where an idle barrier tick once cost 27 s and 1.3 GiB. The
  cost of everything scaled with the size of the project rather than with
  what changed. Entries now live in write-once
  `.flint/lean/manifests/<seq>-<flush-uuid>` objects, and
  `.flint/lean/current` — a few hundred bytes naming the live one — is
  the only mutable metadata object and the only thing a publish CASes.

  The immediate win is the takeover. `rotate_for_takeover` existed to
  invalidate a straggler's outstanding handle, and did it by rewriting
  the whole document: a multi-MB GET and PUT per claim, which also moved
  the ETag every other syncer's no-change early exit compares against, so
  one claim cost every follower a full fetch and parse of a document in
  which nothing had changed. It is now one small CAS — asserted at **at
  most three requests total**, with the entries object's ETag unchanged
  across it. `seq` moves so a stale handle is still stale; `entries_seq`
  does not, which is what will let a cross-cluster follower skip the
  fetch entirely.

  Worth knowing about the mechanism, because it is not what it looks
  like: the manifest CAS is `If-Match` on the object's **ETag**, and
  nothing on the write path ever compared epochs — `cas_write_stamped`
  only *stamps* one. So the `seq` bump was never the fence. It existed
  because a PUT of identical bytes reproduces the same MD5 ETag, the same
  reason the epoch cell carries a salt. Moving `seq` elsewhere and
  leaving the manifest alone would have silently removed the protection
  rather than made it cheap.

  Migration is fail-closed, and deliberately does not delete the legacy
  key: `manifest::load` maps a missing object to `Ok(None)`, `None` means
  *first write*, and a barrier answers that with `If-None-Match: *` — so
  an old `flint-sync` pointed at a migrated workspace whose legacy key
  had simply been removed would conclude the project is empty and re-seed
  over it. The key is instead overwritten with a document that cannot
  parse as a manifest, so an old binary refuses. A pointer naming a
  missing generation is likewise an error, never an empty workspace.

  Superseded generations are reaped, and the reaper is not symmetric:
  below the live pointer everything is superseded and a window of five is
  kept so a reader mid-resolve is never yanked; above it, an object may
  be a publish still in flight, so age decides (one hour) and an object
  the store cannot date is left alone — a leak beats deleting a live
  publish. That also collects the orphan a crash between the entries PUT
  and the pointer CAS leaves behind.

  Design of record: `docs/plans/flint-lean-manifest-pointer-design.md`.
  Chunked entries — a publish costing O(changed) rather than O(entries) —
  build on this and land separately, so each can be attributed.
- **A lean workspace's `sizeLimitGib` is now enforced by a filesystem —
  before this it was enforced by nothing.** Under the webhook delivery
  the workspace tree was an `emptyDir`, so kubelet's own accounting
  evicted a pod that overran its `sizeLimitGib`. Under the CSI delivery
  the tree is a plugin-owned DIRECTORY on the node's root filesystem,
  and the field described nothing at all: `VolumeState::tree_image` —
  *"the loop image backing the tree, if quota mode is on"* — was `None`
  at every site that constructed it. A runaway workspace's only limit
  was the node's disk, which it shares with the kubelet, the container
  runtime and every other pod on the machine.

  The ceiling is now a sparse ext4 image, one per volume, loop-mounted
  at the tree (`s3csi/quota.rs`), so overrunning it is `ENOSPC` inside
  the tenant's own `write(2)` and the bound holds by construction. The
  image is formatted only when the plugin created it — reformatting one
  that survived a plugin restart would erase a live workspace — and the
  tree's ownership is applied after the mount, since a `chown` before it
  lands on the directory the mount then hides. It is torn down at
  unpublish and during publish cleanup, or the mount holds the state
  directory busy and the volume can never be retried.

  Sparse, so an image costs what is written rather than what is
  declared: the ceilings on a node may sum to more than the node's disk,
  exactly as `emptyDir` sizeLimit did. It bounds one tenant's blast
  radius; it is not a reservation. A workspace whose ceiling cannot be
  built is **refused**, not published unbounded — `workers.quota=false`
  on the chart, or `sizeLimitGib: 0` on the CR, are the ways to ask for
  an unbounded tree on purpose. `e2fsprogs` is now named explicitly in
  the node image, where `mkfs.ext4` had been arriving as an implicit
  dependency of the base.
- **Drill legs S14 and S18.** S14 is lean holder identity in two arms
  that control each other: a syncer killed over the same tree
  self-recognises (same holder id, `seq` unchanged, epoch still bumped),
  while a pod replacement after the holder is SIGSTOPped — frozen, so it
  stops renewing and never releases — claims under a new holder id,
  rotates the manifest, keeps every entry, and takes at least the
  quiet-poll floor to do it. Waking the straggler then requires it to
  fence itself on the 412 and exit `Succeeded`/0 without reclaiming the
  cell. The epoch bumps on both arms, so an assertion on the epoch alone
  would pass either way; and a worker cannot be force-deleted to fake an
  unclean death, because the admission policy admits DELETE only from
  the node ServiceAccount, that node's kubelet and the kube-system GC.
  S18 is the quota above, with a `sizeLimitGib: 0` sibling as the
  control — ENOSPC alone proves nothing, since a node with a full root
  disk answers a write the same way.
- **Worker termination is ordered by a PriorityClass and a `preStop`
  hook — there is no PodDisruptionBudget.** A budget over the workers
  (`minAvailable`, the "never voluntarily evict" idiom for bare pods)
  was built and then removed: it guarded only the eviction API, and a
  worker is separated from its tenant on three paths, of which eviction
  is the one a pure-spot fleet is *least* likely to take. Node reboot
  and spot reclamation go through kubelet's graceful shutdown, where a
  budget is inert; a panic goes through nothing at all. It also
  contradicted the workers' own
  `cluster-autoscaler.kubernetes.io/safe-to-evict: "true"` — a budget
  refusing every eviction and an annotation inviting one resolve, in the
  autoscaler, as "never scale this node down" — and it blocked drains
  for as long as any tenant declined to terminate, which ends with
  `--disable-eviction` and no protection at all.

  What replaces it is one mechanism per path that has one. A chart-owned
  PriorityClass `flint-s3-worker` (`value: 100000`, `preemptionPolicy:
  Never`) ranks workers above tenants, and kubelet's graceful-shutdown
  manager terminates by priority, lowest first — so on a reboot or a
  spot reclaim the tenant stops writing before its worker goes. On the
  eviction path, every worker now carries a `preStop` hook running
  `flint-s3-worker await-release`, which blocks until
  `NodeUnpublishVolume` writes a `released` marker into the worker's
  `comm` directory: an evicted worker keeps serving its tenant's mount
  until the volume is genuinely released. The eviction is *accepted* —
  nothing is refused and the drain converges — it simply does not take
  the mount with it. The wait is bounded by `workers.prestopSecs`
  (default 60), **added to** `terminationGracePeriodSeconds` rather than
  carved out of it so a lean syncer's final publish keeps its full drain
  budget, and the hook exits 0 when the budget expires so it can never
  wedge a node.

  `safe-to-evict: "true"` and the Node `ownerReference` with
  `controller: true` both stay, and the annotation is now load-bearing:
  a Node ownerReference is not a controller kind the autoscaler
  recognises, so a bare worker without it blocks scale-down outright.
  Drill leg S16 asserts the ordering behaviourally — the worker
  outranks its tenant, an eviction is accepted and sets a
  `deletionTimestamp`, the tenant reads the same checksum ten seconds
  into the worker's own termination, and a real `kubectl drain`
  completes with no worker and no orphaned mount — and asserts that no
  budget exists in the workers namespace, so one cannot creep back and
  make the leg pass for the wrong reason. The broker keeps its budget:
  a Deployment, `minAvailable: 1`, rendered only above one replica.
- **The lean operator's `StagedWorkRecovered` message named a place that
  no longer has the binary.** It said to run `flint-sync recover-staged`
  "in a pod on this workspace"; under CSI that binary exists only in the
  worker pod in `flint-workers`, which a tenant cannot exec into. The
  condition now gives the reachable recipe, and a unit test fails if it
  stops naming one.
- **Two lean protocol suites were wrongly marked retired.**
  `lean/e2e/run-verbs.sh` (B1-B25) and `run-chaos.sh` (C1-C12) never used
  the lean webhook — they create no CR, read no label, need no operator,
  and drive `flint-sync` directly against MinIO on hand-authored pods.
  The banner claiming otherwise is the worse failure mode: a suite nobody
  runs because it says not to. Corrected in place, with why a worker pod
  could not host those legs anyway.
- **The block/pNFS driver is `disk.csi.chert.us` (BREAKING).** Formerly
  `flint.csi.storage.io`, a domain that was never ours. Its keys move to
  `disk.chert.us/*` — `disk.chert.us/lvol-uuid`,
  `disk.chert.us/replica-sync-state`, `disk.chert.us/role`,
  `disk.chert.us/rejoin-bounce`, the `disk.chert.us/bounce` taint, and
  the node topology key `topology.disk.chert.us/node`. The S3 driver
  moves with it, from `s3.chert.us` to `s3.csi.chert.us`, so both drivers
  read the way the ecosystem writes driver names (`ebs.csi.aws.com`,
  `disk.csi.azure.com`) and are visibly distinct from the key prefixes
  that share their domain. Key prefixes never carry the `.csi.` infix;
  driver names always do.

  Note that `disk.chert.us/role` (`block` | `nfs-shared`, the
  volume_context role hint) and `chert.us/role` (`lite`, the hub
  operator's label) are different keys with different meanings. They were
  distinct before this rename and are kept distinct by it — folding the
  driver's keys into the bare `chert.us/*` family would have merged them.

  BREAKING, and unlike the group rename there is no version of this that
  an existing volume survives: `spec.csi.driver` is immutable on a PV, so
  volumes bound to `flint.csi.storage.io` cannot be adopted by the
  renamed driver — they must be drained and reprovisioned. The kubelet
  plugin directory moves with the name
  (`/var/lib/kubelet/plugins/disk.csi.chert.us`), and so does the
  per-driver staging path under `plugins/kubernetes.io/csi/`, so no mount
  may be live across the upgrade.

- **`s3.csi.chert.us` (new chart `flint-s3-csi`) replaces both mutating
  webhooks.** A pod gets an S3 prefix or a lean workspace as ONE
  `csi:` ephemeral volume naming a `FlintPassthroughMount` or
  `FlintLeanWorkspace` in its own namespace — no label, no injected
  container, no Secret in the pod's namespace, no privilege in the pod,
  and the tenant namespace can enforce PodSecurity `restricted`. The
  privileged part is one DaemonSet that performs the `mount(2)` and
  hands the FUSE fd to an unprivileged, flint-owned worker pod (the
  AWS Mountpoint CSI v2 shape); the lean syncer runs unchanged in the
  same kind of worker. `spec.consumers.serviceAccounts` on both CRDs
  names who may mount (ABSENT = nobody); the pod's ServiceAccount is
  kubelet-asserted, never chosen by the pod. Design of record:
  `docs/plans/csi-node-mount-design.md`. Drills: `s3csi/e2e`
  (single cluster, 18 legs) and `s3csi/e2e/multi` (two clusters, one
  S3 endpoint outside both).
- **`flint-s3-broker`**: an STS-shaped identity exchange
  (`AssumeRoleWithWebIdentity`) that turns the kubelet-minted, pod-bound
  ServiceAccount token into short-lived keys — backend `static`, `sts`,
  or `rest` (the application's own JWT-enforcing REST API decides). The
  worker reads its keys from a loopback credential door; the pod never
  sees them.
- **flint-passthrough chart is now the CRD alone** (0.2.0): no
  Deployment, no RBAC, no webhook. **flint-lean chart** keeps the thin
  controller (claim, posture, sweep) and drops the webhook, its cert
  Secret and its Service. `flint-passthrough-operator` no longer exists;
  `flint-lean-operator` injects nothing.
- CRDs: `FlintPassthroughMount` gains `consumers`, `identity`;
  `FlintLeanWorkspace` gains `consumers`, `identity`, `uid`, `gid`
  (`uid` is REQUIRED under the CSI delivery: the syncer runs as the
  app's uid).
- Release scope `passthrough` is now `s3csi` (accepted as an alias):
  the `flint-s3-csi` chart and its three images, the CRD chart, the
  mounter base.

### Fixed

- **flint-lean: five holes around the manifest CAS, from the 2026-09-03
  integrity audit** (`docs/plans/flint-lean-integrity-audit-2026-09-03.md`).
  The cadence barrier's between-chunk renewal never fenced a deposed
  writer (it returned Ok on exactly the deposed condition), so a
  straggler taken over mid-barrier completed every remaining data PUT
  over the cited generation; the barrier now fences on the cell that
  renewal already read, at no extra request — the gated lanes were
  already fenced. A renew whose own previous response was lost read
  the resulting 412 as a deposal and self-fenced (exit 0; permanent
  under CSI): one read now tells a cell that still names this holder
  from a takeover. The SIGTERM drain attests a completed drain in the
  tree (`.flint-sync/drained.json`), retries for the budget the
  delivery stamps (`FLINT_SYNC_DRAIN_BUDGET_SECS`) and, on failure,
  leaves the lease UNRELEASED instead of attesting a clean handoff.
  State and control files are fsynced (file and directory), and the
  tree is `syncfs`ed before the marker or a baseline that vouches for
  materialised files, so a power loss cannot leave a baseline
  describing zero-length files that the next scan publishes. With
  `FLINT_SYNC_PROJECT_ID` stamped (the operator's env list and the CSI
  delivery both stamp it), the syncer reads the claim cell before its
  first claim step and refuses a prefix claimed by another project —
  refuse-foreign is enforced on the data plane, not only by the
  operator's verdict. The refusal is `LeanError::Refused` and exits
  **78** (`EXIT_REFUSED`, sysexits `EX_CONFIG`): the one code the CSI
  delivery treats as final. The first S22 run showed why that contract
  has to exist — a refusal exiting 1 was restarted by `OnFailure`,
  relaunched by the supervisor from its persisted launch record, and
  reported to the tenant as "checkout in progress" for as long as the
  pod lived.
- **`s3.csi.chert.us`: a dead mounter's source mount outlived its
  volume.** `unmount_all` tested `path.exists()` before the mount table,
  and a dead FUSE mount answers `stat` with `ENOTCONN` — so the one mount
  that most needed unmounting was skipped, the state file was removed,
  the directory removal failed on the busy mount, and every kubelet retry
  found no state and did nothing: the source stayed mounted for the life
  of the node. Found by leg S9 + SU on a real node (EC2, 2026-09-04),
  where the worker kill is a SIGKILL of the container's processes and the
  mount really dies; on kind the same leg never produced a dead mount.
  The loop now judges by the mount table alone, and a volume directory
  with no state file (a half-removed volume) finishes its own teardown on
  the retry instead of being skipped as "nothing of ours".
- **Drills: the same legs on real nodes.** `run-s3csi.sh` and
  `multi/run-multi.sh` gained the substrate knobs `STORE=minio|s3`
  (a real bucket: `BUCKET`, `S3_REGION`, `S3_KEY_FILE`),
  `NODE_EXEC=docker|nodesh` (`scripts/nodesh.sh` in place of `docker exec`
  into the kind node) and `NODE` (default: the first worker, not the
  tainted control plane); fixtures are rewritten at apply time, the
  bucket literal is `$BUCKET`, `rig-s3.yaml.tpl` is the real-bucket rig
  (its seed wipes every version first — a versioned bucket keeps the
  last run's objects and the legs count), `build-images.sh PUSH=1
  ARCH=amd64` pushes the images a real cluster pulls, and
  `s3csi/e2e/aws-drill.sh` drives the campaign on two all-spot EC2
  clusters from trove. Two legs learned what a real node is: the worker
  kill is now a SIGKILL of the pod's cgroup when there is no `crictl`
  (Amazon Linux 2023 ships `ctr` only), and the plugin pod is selected
  by `spec.nodeName`, not list order — on a two-node cluster
  `items[0]` was the control plane's, so S17 rolled one pod and read
  another. `scripts/nodesh.sh` no longer lets kubectl's `pod "…" deleted`
  line into captured output. The multi-cluster drill ran end to end for
  the first time on EC2: its seed gate had demanded 11 objects of a
  12-object seed since the file was written and exited before any leg
  — fail-closed, so no kind multi-cluster number was ever produced,
  right or wrong (MinIO lists twelve as well, measured) — and its
  manifest resolver now reads the chunked layout. The kind multi path
  is unproven end to end until it is run there. EC2 evidence (us-west-1, m6i.large spot,
  real S3): three single-cluster runs on s3a — 177 ok / 6 bad (every failure a kind assumption in the drill), 181 / 2 (S17's own vacuity guard, and the leaked dead mount above, found by SU), then 182 / 1 with the fix, the 1 being S17's guard exactly as on kind; and the multi-cluster drill 22 / 0 across s3a and s3b (M1–M3) once its seed count and manifest resolver matched a real bucket and the chunked layout.
- **`s3.csi.chert.us`: four more from the same audit.** A lean worker
  that exited on its own (`Succeeded`: fenced) under a still-mounted
  tenant is relaunched on the next republish, not only a `Failed` one.
  A syncer whose container exited **78** (a refusal a restart cannot
  change) is recognised as final from its termination record even
  though `OnFailure` keeps the pod `Running` — the publish fails
  `FailedPrecondition` with the syncer's own last line (both project
  names), a `SyncerRefused` event carries it too, and the worker is torn
  down rather than relaunched in place; the tenant's mount succeeds on
  kubelet's next retry once the claim is its own, with no restart. A `Running` worker whose supervisor lost its launch
  record (a node reboot empties the memory-backed comm dir) gets the
  launch sent again in the same pod (`SyncerRelaunched`). And
  `NodeUnpublishVolume` removes a lean tree only when the syncer's own
  drain attestation post-dates the SIGTERM; a drain that failed every
  attempt or was fenced leaves the pod just as gone, and its tree is now
  preserved under `<plugin>/undrained/` (`DrainNotAttested`) instead of
  removed. Drill legs S20–S22 cover the four; S14 tolerates the
  relaunch landing before it looks.
- **`s3.csi.chert.us`: a plugin restart no longer restarts a lean
  checkout in progress**, a published lean workspace can no longer be
  started over by a republish that finds its target unmounted (it is
  rebound, or refused by name), a lean syncer lost at the pod level is
  relaunched over the same tree from the next republish
  (`SyncerRecreated`), the final drain exchanges for a key that covers
  its whole budget before the SIGTERM, and an undrained tree is
  preserved under `<plugin>/undrained/` and named in an
  `UndrainedTreePreserved` event instead of being removed. Republish
  liveness comes from one watch per node instead of a GET per volume,
  its probe no longer LISTs the bucket, and a broker outage at
  exchange time is `Unavailable` (retried) rather than
  `PermissionDenied`.

- **A published lean workspace could have its tree deleted under the
  running pod.** `is_mountpoint` compared device numbers, which cannot
  see a bind mount whose source and target share a filesystem — which
  is every lean bind (both live under `/var/lib/kubelet`). A republish
  therefore missed the "already published" branch, took the "unfinished
  publish, start over" path, and removed the volume directory while an
  agent was writing into it; the next publish captured only the files
  written after the wipe. The test now reads `/proc/self/mountinfo`,
  and the cleanup path refuses a published lean volume outright. Found
  by the kind drill, in code that had never been released.
- The credential document the worker serves on its loopback door now
  always carries `Token` (empty when there is none): the CRT tolerates
  its absence, the AWS Rust SDK the lean syncer uses does not, and both
  read the same file. The door is also bound before the child process
  is spawned, and every credential arm carries a region.
- The broker's registration table is in memory, so the node plugin
  re-registers before every credential refresh; a broker restart no
  longer leaves mounted pods unable to refresh until their keys expire.
- The workers admission policy admits the kubelet's own delete of a
  pod bound to its node — without it every exited worker sat
  `Terminating` forever, retried every ten seconds.

### Removed

- The webhook e2e rig `passthrough/e2e`. The lean rigs under `lean/e2e`
  still describe the label-injected shape and no longer run as written;
  the protocol suites there are to be re-targeted at the worker pod
  (design §10.2 S12).

## [1.44.0] - 2026-09-02

Two silent data defects that shipped in 1.43.0, and a session handshake
that refused its own advice.

Nothing here announces itself on the wire. A striped read could return
**zeros with `NFS4_OK`** — no error, no log line, no client-visible
anomaly — and the MDS could serve a tiered file's **stub** as if it were
the file. Both are 1.43.0 regressions and both are invisible from the
client, so there is no mitigation to apply: the upgrade is the fix.

### Fixed

- **A bounded LAYOUTGET advertised the wrong stripe width — zeros with
  `NFS4_OK`.** The width came from `segments.len()`, which is a
  per-unit count, so a bounded READ grant advertised width **1**. The
  client then read stripe index 0 while the bytes actually lived on
  `file_id % 3`, and the DS answered the read of an unwritten region
  successfully. The width now comes from the pinned placement.
  Root-caused and field-verified on a live fleet: 98 read errors before,
  **0 after**, with the serving gate at 548/548.

- **F68: the MDS served a striped file's tier stub.** A separate hole in
  the same area, and equally quiet — the client receives stub bytes and
  a success status.

- **EXCHANGE_ID named a `csa_sequence` that CREATE_SESSION was
  guaranteed to reject.** A case-1 (`ExistingConfirmed`) EXCHANGE_ID
  replied with the legacy `sequence_id` field — always 0 for a confirmed
  record — while CREATE_SESSION validates against `initial_cs_sequence`.
  The server told the client to send a value it would then refuse with
  `NFS4ERR_SEQ_MISORDERED`. It bites any client that needs a **new**
  session against a **surviving confirmed** record: a fresh connection
  after a server restart, or a client whose session was lost but whose
  record was not. A Linux client that renews its persisted session never
  reaches the path, which is why no suite caught it.

- **Tier eviction starved by delegated reads, and a leaked descriptor
  per delegated file.** Both are reachable only with delegations
  enabled, which is off by default in this release. The eviction probe
  asked the fd cache "is there a writable descriptor for this inode"
  rather than asking the open state "does a client hold a write open";
  since the READ path opens read+write deliberately and a delegation
  holder reads under the delegation stateid — a key CLOSE never reaps —
  one read pinned a file as non-evictable permanently. The probe now
  consults `file_has_write_open`, and delegation teardown releases the
  cached descriptor on every removal path.

### Added

- **NFSv4.1 READ delegations — implemented, and shipping DARK behind
  `FLINT_NFS_DELEGATIONS`.** OPEN_DELEGATE_READ with the full
  recall-or-die machinery: CB_RECALL, DELEGRETURN, DELEGPURGE,
  TEST_STATEID/FREE_STATEID, `SEQ4_STATUS_RECALLABLE_STATE_REVOKED`,
  an anti-flap cooldown and a circuit breaker. Measured on the wire
  against a kernel client: inside the attribute-cache window warm
  re-access metadata traffic goes **80 → 0**, and across a full tier
  evict/hydrate cycle a holder re-reads **nothing** (40 → 0 READs).
  pynfs and nfstest both clean for the read-delegation set; every
  remaining failure in those suites is a WRITE delegation, which is an
  explicit non-goal. **The feature is not finished** — several restart
  and DS legs are open — and the default stays off.

- **Concurrency model checking** of the delegation table with AWS
  `shuttle`, behind the non-default `shuttle-test` feature. Nothing
  ships with it enabled.

### Gates at the tag

Rust suite **2391/0 on macOS, 2363/0 on Linux** (the Linux suite is the
one that counts — a third of this code is `cfg(target_os = "linux")`).
pynfs full NFSv4.1 conformance on both binaries. Tier leg passes on its
data claim with a demonstrably loud control arm.

**One measured number that does not match the design, recorded rather
than smoothed over:** across a tier cycle past `acregmax`, a delegation
holder's *metadata* RPCs do not go to zero — they are unchanged in total
(42 vs 42) and merely reshaped from OPEN/CLOSE into GETATTR/ACCESS. The
data path delivers what was claimed; the metadata path does not. This is
open, and it is one reason delegations stay dark.

## [1.43.0] - 2026-08-31

The write path, the small-file path, and a deadlock that shipped dark.

v1.42.0 measured `splice(2)` and shipped it dark; this release turns it
on by default — after fixing the deadlock the dark path was hiding.
**The kernel counts pipe capacity in slots, not bytes**, so a full-size
READ at an unaligned offset could park the server on a pipe that looked
half-empty. Any v1.42.0 image serving a client whose block size exceeds
`rsize` (a 4 MiB O_DIRECT read is enough) can wedge permanently;
upgrading is the fix, and `FLINT_NFS_SPLICE=1` should not be set on
1.42.0 images.

### Fixed

- **splice pipe-slot deadlock**: the splice path counted pipe capacity
  in bytes where the kernel counts buffer slots; a READ spanning more
  slots than remained free wedged the connection permanently. Reachable
  on v1.42.0 with `FLINT_NFS_SPLICE=1` and any reader whose block size
  exceeds `rsize`. No test suite covered the shape (it needs a
  full-size read at an unaligned offset); the perf gate's rig now does.
- **EXCHANGE_ID purge storm against the pNFS data server**: the DS
  answered DESTROY_CLIENTID and DESTROY_SESSION with no-op acks, so a
  destroyed client's confirmed record survived, every fresh incarnation
  of that client drew `EXCHGID4_FLAG_CONFIRMED_R`, and Linux answers
  that with PURGE_STATE — an EXCHANGE_ID bearing the all-ones boot
  verifier, forcing a case-5 discard on every re-association and
  leaking client records, leases, and sessions without bound. Both
  verbs are real now (CLIENTID_BUSY / STALE_CLIENTID / BADSESSION arms
  per RFC 8881 §18.37 and §18.50); a full drill runs at zero case-5
  discards.
- **A layout write moves the change attribute**: LAYOUTCOMMIT now bumps
  the change counter and sets mtime, so a client that wrote through the
  data server no longer reads its own stale cache back; and an MDS
  restart re-seeds device control endpoints instead of orphaning them.
- **A reused S3 prefix refuses to serve the previous project** (B12):
  the tier stamps `<prefix>.flint/owner` with the share's identity and
  refuses foreign starts before the epoch claim; `adoptData` is the
  explicit takeover path.
- **NodePublish self-heals the stale-staged CSI wedge** (F29): publish
  re-drives NodeStage from its own inputs, so an operator rollout that
  raced staging no longer wedges the volume until a suspend/resume
  cycle.

### Performance

- **NFS WRITE at parity with knfsd** (was 0.46x): the request tail was
  copied one byte at a time (15% of total system CPU), UNSTABLE writes
  hopped threads they did not need, and large inbound records now land
  in pooled buffers (page faults per burst: 6540 → 1).
- **READ: `splice(2)` on by default** — the payload moves file → pipe →
  socket without entering userspace, at 2.8x less server CPU per byte;
  with it, mimalloc as the default allocator, `block_in_place` for READ
  bodies, and the per-operation deep clone of the session (slot table
  plus every cached reply, cloned to read three u32s) is gone. The perf
  gate's read ratio vs knfsd stands at 1.28 this release (baseline
  floor 0.39).
- **Fore-channel headroom**: advertising exactly 1 MiB as the maximum
  request halved the effective RPC size the Linux client would use;
  +2 KiB of headroom lets a 1 MiB READ actually be 1 MiB (READ op
  count halved at bs=1M).
- **Small-file metadata at knfsd wire parity**: repeated stats are
  answered from a counter-validated attribute cache, containment checks
  use one `openat2` instead of a realpath walk, READDIR hands out the
  filehandles LOOKUP was minting (`ls -l` over 1000 files: 1002 LOOKUPs
  → 2), and directory caches are no longer told they are stale on every
  visit (the ACCESS storm is gone).
- **The pNFS data server adopts the standalone lane's READ path**
  (clamp → splice → pooled fallback): the DS-lane differential moves
  from 0.63x to 0.95x of knfsd.

### Changed

- **One wire layer, two lanes**: the standalone server and the pNFS
  data server now share a single RPC-record ingress (fragment
  reassembly, whole-record ceiling, pooled reads), one
  segment-to-socket writer, one `channel_attrs4` decode and
  fore-channel negotiation, and one READ fast path. Policy — dispatch,
  trust and stateid models, fd caches, the DRC — stays per-lane. The
  DS gains the fragment reassembly and the 2 GiB reply guard it never
  had.

### Gates at the tag

- Perf differential vs knfsd: read 1.280 / write 0.910 / meta 0.743
  (floors 0.390 / 0.514 / 0.565); falsifiability arm refused as
  required.
- pynfs 4.1 full suite: **171/0/91 on both binaries** (standalone and
  hub posture), exact baseline match.
- fsx + fsstress torture: PASS (fsx 20000 ops buffered + O_DIRECT,
  fsstress 4x500 namespace storm).
- macOS suite 2228/0; Linux-in-lima suite 2249/0 (unprivileged).

## [1.42.0] - 2026-08-28

The read path, and a conformance defect it uncovered.

`splice(2)` was the last untried lever on NFS READ, and it is worth what
the measurement said it was: **the payload costs 36% of the CPU per byte
it used to**, which closes almost all of the distance to the in-kernel
server — the CPU gap to knfsd goes from **1.83x to 1.07x**. It ships
**dark**, behind `FLINT_NFS_SPLICE=1`, because the conformance suites
have only been run with it off.

Chasing an unrelated pynfs failure while measuring it turned up a real
EXCHANGE_ID defect: a client colliding with another principal's
confirmed record was answered `NFS4ERR_OK` where RFC 8881 requires
`NFS4ERR_CLID_INUSE`.

### Added

- **Zero-copy READ behind `FLINT_NFS_SPLICE=1`** (default **off**). A
  READ stages file → pipe inside the blocking task it already used, and
  the reply path moves pipe → socket. The payload never enters
  userspace.

  Measured in-server, 5 interleaved reps per arm, paired per-rep ratios,
  4 readers on an idle 2-vCPU Linux VM:

  | arm | cpu-ms/GiB | MiB/s | CPU vs knfsd | throughput vs knfsd |
  |---|---|---|---|---|
  | copy path | 495 | 3195 | 1.83x | 56% |
  | **splice** | **290** | **4935** | **1.07x** | **86%** |
  | knfsd | 270 | 5753 | — | — |

  Median CPU ratio **0.358** (range 0.344–0.397 over 10 paired reps) and
  **+59% throughput** (median 1.59x).

  Scoring is **cpu-ms/GiB, not MiB/s**: an earlier throughput-scored
  measurement of the same change came back 0.989x — indistinguishable
  from nothing — because a single-stream read is not CPU-bound. The
  server-CPU metric is what the change is actually about, and the
  throughput gain is a consequence of it rather than the thing measured.

  Three constraints hold the path off by construction rather than by
  remembering a guard: `can_splice` **defaults to false**, so every
  in-process consumer of a READ result is correct without knowing splice
  exists (this immediately caught the HTTP File API, which consumes READ
  bytes in-process and touches no socket); GSS is excluded, because a MIC
  computed over the body needs a body in userspace; and a slot-cached
  reply is excluded, because the cache must be able to replay contiguous
  bytes.

  **The retract is structural, not added.** The tier's post-read consult
  already runs inside that blocking closure, so every error return
  between staging and success *drops* the staged pipe, and dropping
  retracts it — the pipe is destroyed and not one byte reaches the
  client. Staging to a pipe rather than straight to the socket is the
  whole reason the existing control flow stays correct.

  Still off by default: pynfs and nfstest have only been run with the
  flag **off**. Running both with it **on** is the gate before this
  becomes the default.

- **The tier refuses to start against an object store that does not
  enforce conditional writes.** The hub trusted, without ever checking,
  that its store honours `If-Match`/`If-None-Match`. Against a store that
  accepts those headers and ignores them — some S3-compatible backends,
  and any proxy that strips them — the tier's compare-and-swap
  arbitration silently degrades to last-writer-wins, with no error
  anywhere to explain the lost writes. `flint-lean` already refused to
  start on a non-conformant bucket; the hub started and trusted.

  The probe is deliberately a strict *subset* of the existing version
  probe: it does **not** require bucket versioning, which a hub does not
  need and which would refuse a perfectly good deployment at startup.

### Fixed

- **`EXCHANGE_ID` answered a client collision with `OK`** where RFC 8881
  §18.35.4 case 3 requires `NFS4ERR_CLID_INUSE`. A second principal
  presenting an existing `co_ownerid` was allowed to supersede the
  incumbent, with the incumbent's teardown merely *deferred* until the
  newcomer confirmed.

  Deferral is case **5**'s rule — client reboot, principal *unchanged* —
  and applying it here was both non-conformant and **weaker than the
  answer the RFC asks for**: the superseded clientid stayed usable
  (pynfs `EID5e` caught exactly that, `CREATE_SESSION` answering `OK`
  where `NFS4ERR_STALE_CLIENTID` was due), and it bought no safety,
  because the colliding peer can confirm its own new clientid one
  operation later and trigger the cascade anyway. Refusing outright is
  strictly stronger.

  Both arms of case 3 are fixed, not one: the RFC's record pattern for
  it is `{ ownerid_arg, *, old_principal_arg, ..., confirmed }` — the
  **verifier is a wildcard** — so a confirmed record held by a different
  principal is a collision whether or not the verifier matches. The
  matching-verifier arm was previously labelled "case 9 alt" in the
  source, which is the `UPD_CONFIRMED_REC_A` update family and does not
  apply without that flag.

  The answer now turns on the incumbent, as the RFC specifies: live
  state under an unexpired lease → `NFS4ERR_CLID_INUSE` and nothing is
  touched; no state, or an expired lease → the record is deleted
  **immediately** and a new shorthand clientid is minted.

- **Seven POSIX-fidelity defects in the tier**, each seen red before the
  fix. Every one is silent — nothing fails, and the damage surfaces
  later as missing data or a wrong attribute:

  - the server's own `.flint-nfs` directory was publishable and
    evictable, so truncating `fh.key` would have made every filehandle in
    the export permanently stale;
  - the evictor opened a client-writable path **by name** with no symlink
    guard and no identity re-check, so a substituted inode or a planted
    symlink could be truncated instead of the intended file;
  - unlinking one name of a hard-linked file deleted the shared object
    and dropped the surviving name's rows, leaving the remaining link
    pointing at nothing;
  - a re-key deleted a bucket object another live row still cited;
  - a failed hydration permanently rewrote the file's mtime, so a
    transient S3 error silently changed metadata nothing would restore;
  - a DR restore stripped setuid and setgid from every file it restored;
  - `FATTR4_LINK_SUPPORT` was hardcoded true while `LINK` is refused with
    `NOTSUPP` under the tier. Clients *ask* before they try — `tar`,
    `pax`, `rsync -H` and `cp -a` read that bit to choose between linking
    and copying — so the lie turned a supported fallback into a hard
    error in the middle of an extract.

- **A two-month-old test flake, correctly diagnosed this time.**
  `read_count_is_clamped_to_the_response_ceiling` was green alone and red
  in the full suite, and had been recorded as `(dev, ino)` marker
  aliasing — a tier test's eviction marker landing on the test file's
  reused inode. Instrumenting an actual failure refuted that: the failing
  run showed **no marker on the file at all**.

  The real cause is the *other* half of the window guard. `MARKER_CYCLE`
  is one process-global counter bumped by every marker insert anywhere,
  and a read window is valid only if it has not moved — so **any** test
  planting **any** marker inside the window breaks it, related or not. It
  needed the full suite because two process-global statics have to line
  up: `capture::enable()` is deliberately sticky with no disable, so one
  tier test leaves the consult live for every test after it; and
  `is_evicted` is gated on capture being enabled while `marker_cycle()`
  is not.

  The product behaviour is deliberate and unchanged — a global counter
  means an unrelated file's eviction costs one spurious `DELAY` retry,
  which the eviction module weighs explicitly against per-identity
  narrowing. Production evicts rarely; a test binary evicts constantly.
  So the fix is test isolation: every cycle-bumping test now takes the
  rig lock this one already held. The assertion is also self-diagnosing
  now, reporting the capture state, the marker, and the cycle delta, so a
  recurrence names its own cause instead of pointing at the response
  ceiling it has nothing to do with.

- **A Linux-only red suite that macOS could never have shown — and this
  one *was* inode aliasing.** `tier::import`'s adopt test lost
  `src/nested/main.rs`: the row never reached the backend while the
  adopt still reported it marked. Not a race — it reproduced 5/5 in the
  full suite, in `tier::import` alone, and **single-threaded**, then
  narrowed to an exact pair of tests.

  The preceding test marks one file durable and drops its `TempDir`.
  Measured on the VM rather than argued: the next `TempDir`'s
  `src/nested/main.rs` gets **inode 268093 — the same inode** ext4 just
  freed. `capture::queue_mark` returns early for a known-durable
  identity, so the mark is dropped in silence, and `adopt_local_tree`
  counts at the note call rather than at the queueing, so it reports 3
  files while 2 rows exist. APFS allocates differently, which is exactly
  why every macOS run was green.

  **Production is not exposed:** a real unlink goes through the server,
  `tier::identity` fires, and `capture::forget` clears the memo — that
  function's own comment already names ext4 inode reuse as the hazard.
  Only a tree that vanishes without the server knowing leaves a stale
  entry, and a `TempDir` drop is precisely that. So the fix is a
  test-only reset of the capture maps at rig construction, not a change
  to how the product keys them.

### Changed

- **A READ payload is no longer required to be memory the server holds.**
  The reply path carries *segments* rather than buffers, and a segment is
  either bytes or a staged pipe. Consequently `ReadResult` and
  `OperationResult` lose `Clone` — a pipe has one reader — which is only
  affordable because the size check no longer clones every result to
  measure one length. That clone was itself the copy that mattered on the
  read path; segmenting the RPC layer alone measured 0.989x.

### Not published

**No images or charts have been pushed for this release yet.** The tag
records the code; `1.41.1` remains the current installable release until
`dilipdalton/flint-driver:1.42.0`, `dilipdalton/flint-pnfs:1.42.0` and
the three charts are published. This is called out because the same gap
shipped silently once before — `1.41.0` was tagged and never published,
and had to be superseded by `1.41.1`.

## [1.41.1] - 2026-08-27

**Supersedes 1.41.0, which was tagged but never published.** The
in-flight byte bound 1.41.0 introduced was not reachable: the sidecar
read `FLINT_SYNC_FETCH_INFLIGHT_MB`, but the webhook — which builds the
sidecar's entire environment — never stamped it, and no CR field carried
it. Every workspace ran the binary default and there was no place a user
could change it. The changelog called it tunable; it was not.

Install 1.41.1. No `flint-lean` images were ever pushed for 1.41.0.

### Fixed

- **`spec.fetchInflightMb`** (default 512) now exists on
  `FlintLeanWorkspace` and the webhook stamps it, so the knob the
  read-path release headlines is actually settable. Declaring it env-only
  instead was considered and rejected: the webhook owns the sidecar's
  environment, so "env-only" in the operator path means *unsettable*, and
  putting it in the guard test's exceptions list would have documented a
  dead knob rather than a deliberate one.

This is the same class as the fan-out default the release itself fixed —
a value that exists in the binary and cannot be reached from the CR — and
it shipped in the very commit that fixed the other half. The repo's own
guard test, `every_knob_the_sidecar_reads_is_stamped_by_the_webhook`,
names exactly this and was red at `v1.41.0`; it had not been run after
the perf commit, because lean work runs the lean crate's battery and
never the operator's. Green now, with the whole hub suite: 1973 passed,
0 failed.

## [1.41.0] - 2026-08-27

**A lean-scoped read-path release: the agent-blocking checkout, measured
rather than reasoned about.** Scope matches 1.38.0/1.39.0 — only the
`flint-lean` chart (now `0.5.0`) and the two images it pulls are
published. The CSI driver chart, the lite charts and the SPDK target
image are untouched and stay at 1.37.0.

`1.40.0` is reserved for the passthrough front end, in flight when this
was cut; this release deliberately skips it rather than claim it.

### Changed — the read path

The headline is one default. A fresh checkout of 20,000 small files at a
25 ms round trip runs **39.6 s → 21.3 s (−46%)** purely from the fan-out
default moving 16 → 32. That is time the agent spends unable to read its
first file.

The number that outlives the release is the model behind it. Measured
wall clock fits `(files / fanout) × RTT + floor`, and setting the two
terms equal puts the useful ceiling at **`RTT / per-file-floor`,
independent of file count** — so ~32 for same-region S3 and higher only
as the round trip grows. `fanout` is no longer a guessed constant; there
is a rule for when to change it.

- **Fan-out default 16 → 32**, in the CRD (which is the one that counts:
  the API server applies that default when a workspace omits the field,
  so the Rust constant alone would have shipped inert).
- **An in-flight byte bound on the checkout window**, default 512 MiB,
  tunable with `spec.fetchInflightMb` (see 1.41.1 — as shipped in
  1.41.0 the knob was NOT reachable). `fanout` bounded the
  number of concurrent fetches but never their size, and each whole
  object is held in RAM before it reaches disk — so peak RSS was
  `fanout × largest object`, an unbounded product, in a sidecar that
  ships with no memory limit. Measured on 32 × 32 MiB: **916 → 533 MiB**.
  It also ran **25% faster**, which was not predicted: holding 916 MiB
  in flight costs more in memory pressure than the extra width buys.
- **Slice-by-8 CRC-64/NVME**, same digest, **3.36× faster** (352 → 1184
  MiB/s). This is the resume row's inner loop — a restarted container
  re-CRCs every present file before it can serve — so a 1 GiB workspace
  resume goes **3.20 s → 1.18 s**, and a 20 GiB one 58 s → 17 s.
- **Compact JSON for the manifest and the baseline.** Same 3,001-file
  tree: **830 → 601 KiB (−27.6%)**, 283 → 205 B/entry. It moves on every
  checkout, every barrier merge and CAS, and every gateway read verb.
  `mc cat`, `jq` and the raw quoted-path greps the rigs use are all
  unaffected — verified against a live bucket.
- **`GET /status` answers by HEAD, not GET.** It reports scalars and not
  one entry, so it had been downloading and parsing a document that runs
  to tens of MB to return three numbers. Request count is unchanged at
  three.
- **Largest-first checkout admission.** Path-order admission appended the
  biggest object's transfer to the tail of the fan-out window. Worth
  −4.4% at 25 ms here, and it scales with the largest file rather than
  the tree.
- **A read timeout on the S3 client.** The default provider bounds only
  connect, and stalled-stream protection arms after headers arrive, so
  nothing bounded the wait for a response that never starts. One pooled
  connection to a reclaimed peer hung a fan-out slot, and because the
  window is collected whole, the entire checkout waited and the
  agent-start marker never landed.
- **Two allocations off the barrier**: `Bytes::from(body.clone())` was a
  full memcpy of every published body, and the gated lane rewrote the
  whole O(files) baseline every tick whether or not the scan set moved —
  the guard the cadence path has always had.

### Fixed

- **A takeover rotation dropped the manifest's boundary-source stamp.**
  `rotate_for_takeover` clones the standing manifest and re-CASes it
  through a path that passes no source, so the DOCUMENT carried e.g.
  `sentinel` through the clone while the OBJECT STAMP lost it — exactly
  the GET/HEAD divergence the writer documents as forbidden. Invisible
  for as long as every reader used GET; it surfaces as a null
  `boundary_source` the moment one HEADs, which `/status` now does. Both
  halves land together, and the writer now stamps the document's own
  source so the two cannot disagree.
- **The `flint-store` test target had not compiled since `f2080edd`.**
  Three `self.bump(...)` calls sat in free test functions where there is
  no `self`, so the whole lib-test target failed to BUILD and every test
  in that crate was silently unrunnable. Nothing caught it because lean
  work runs its own crate and never builds this one's tests.

### Upgrade notes

- **`fanout` default changes for workspaces that do not set it.** A
  `FlintLeanWorkspace` with no `spec.fanout` moves from 16 to 32 on
  chart upgrade. Set `spec.fanout: 16` to hold the old behaviour. Nothing
  else in the CR surface changed.
- **The measurements are from a local rig with injected latency**, not
  real S3: MinIO behind a latency proxy on one machine. Ratios under
  controlled round trips are sound; the absolute per-request floor is
  rig-specific, and on a faster S3 path the useful fan-out ceiling is
  higher than 32, not lower. A proxy-shaped re-measurement is still the
  open item it was in 0b.
- Checkout now reports phase timing (manifest / fetch / commit) on
  stderr, which is what makes any of the above attributable.

## [1.40.0] - 2026-08-27

**The third front end, published: an S3 prefix as a directory in a pod,
with one mounter and no control plane behind it.** Scope is
passthrough-only — the new `flint-passthrough` chart (`0.1.0`), the
shared operator image, and a new mounter image. The CSI driver chart,
the lite charts, the lean chart and the SPDK target image are untouched.

Cut AFTER `1.41.0`, and numbered below it on purpose: `1.41.0` reserved
this number for exactly this work rather than claim it. One consequence
worth stating plainly — the operator image published here is built from
a tree that already contains `1.41.0`'s lean read-path work, because
that is what the branch holds. Nothing routes lean traffic through it:
the `flint-lean` chart pins `1.41.0`, whose images are a separate,
still-pending publish.

### Added — flint-passthrough

`FlintPassthroughMount` names a bucket subtree; a pod opts in with one
label; a mutating webhook injects a native sidecar that FUSE-mounts it.
There is no checkout, no manifest, no claim, no publish boundary and no
controller — reads and writes go straight to the bucket. Reach for
`flint-lean` instead when a pod wants a working tree with a boundary.

Two costs, both structural and both enforced rather than documented:

- **The sidecar is PRIVILEGED.** `mountPropagation: Bidirectional` is
  the only way a mount made in a sidecar reaches the app container, and
  the API server allows that mode only on a privileged container. In a
  namespace enforcing PodSecurity `baseline` or `restricted` the mutated
  pod is REJECTED — proven by a drill leg, not asserted.
- **A mounter crash is not recoverable in place.** Every container
  already running holds a private copy of the FUSE filesystem; when the
  mounter dies that copy goes ENOTCONN and the replacement's fresh mount
  does not reach it. The sidecar detects the case and reports itself NOT
  READY so the pod leaves its Service endpoints, and the pod must be
  recreated. Also proven by a drill leg, including the pod-level
  `Ready: False`.

### Changed — one mounter, not two

The component was built with a `driver` field selecting s3fs or
Mountpoint for S3. It ships with **Mountpoint for S3 and nothing else**,
and the field is gone.

The reason is not line count, though ~200 lines went with it — including
the launcher fragment that re-exported a Secret's contents into
s3fs-shaped environment variables, which was the only place a credential
touched a shell. It is that a second driver was offering a POSIX
*approximation* — rename as copy+delete, append by rewriting the whole
object — with no coordination behind it: two pods writing one key is
last-writer-wins, undetected. That is a worse answer than "use the front
end that has a publish boundary". So passthrough now does fast lazy
reads and whole-object sequential writes, and `git`, `pip install` and
sqlite do not work here at any setting, on purpose.

Removing a field is where this could have gone wrong quietly, and both
directions are now closed:

- **`spec.driver` is a TOMBSTONE in the CRD, not an absence.** Deleting
  it outright would have the API server PRUNE it — a CR asking for s3fs
  stored without the ask, mounted with a write model it did not choose,
  first noticed by a workload failing at 3am. Declared-and-refused makes
  it a `kubectl apply` error naming the field.
- **`spec.mountOptions` containing `-o` is refused at admission.**
  mount-s3 takes only `--long` flags, so `-o` is an s3fs option that
  outlived s3fs; unrefused it is a privileged sidecar in
  CrashLoopBackOff whose reason exists only in a container log. Found by
  running the rig against a CR written before the removal.

### Fixed — the mount was owned by root, so the workload could not write

mount-s3 reports the MOUNTING user as the owner of everything in the
mount, and the mounter is root. An unprivileged pod therefore read all
of its objects and got EACCES on its first create — which reads as a
bucket policy problem and is not one. `spec.uid`/`spec.gid` were the fix
and nothing pointed at them.

The pod's own `securityContext.runAsUser`/`runAsGroup` is now the
default for both, which is the right answer wherever the question comes
up at all; the CR still wins when it says something, and a pod that
declares nothing runs as root and needs neither. Found by the rig's
write-through leg, which failed for this reason and no other.

### Fixed — two release-machinery holes, one of them live

- **`stage-prebuilt.sh` never staged `flint-passthrough-operator`**,
  which `Dockerfile.operator.prebuilt` COPYs unconditionally. This is
  not "an operator image without passthrough" — it is no image at all,
  a release that dies at `docker build` with `COPY failed`. Staged in
  every scope now, alongside a new `passthrough` scope for the three
  scripts.
- **`publish-images.sh` read `--dry-run` from `$2` alone.** Once a scope
  argument existed, `publish-images.sh <ver> passthrough --dry-run` —
  the natural way to write it — set the scope variable to
  `"passthrough"`, left `dry` empty, and PUBLISHED FOR REAL from a
  command whose author had just asked it not to. Both flags are now
  scanned over the whole argument list.

`release.sh` gained the passthrough chart gate: both images published at
the chart's appVersion, the lean-named operator alias proven to be the
same DIGEST as the lite image, the recipe proven to install
`/usr/local/bin/flint-passthrough-operator`, and the mounter recipe
proven to install a PINNED mount-s3 and to verify `fusermount` exists.

### Verification

kind drill **42/42** (`passthrough/e2e/run-passthrough.sh`), against the
chart and a locally built image, every read leg asserting CONTENT written
before the pod existed and every refusal carrying an accepted control ·
lib tests **29/29** for the module.

Two of the drill's reds were real and are fixed above; two more were the
rig telling the truth about itself — MinIO's `emptyDir` had lost the seed
after a restart, and leg A1 refused to let any read leg be trusted until
it was reseeded. One leg was VACUOUS and is not any more: A7 asserted
that unlink removes an object without first proving the object existed,
which passes trivially when the write never happened.

New gates that keep the hand-written CRD honest, since this spec is
plain serde with no schemars derive to diff against:
`the_crd_and_the_struct_agree_on_every_field` compares the shipped CRD's
properties against the struct's fields in BOTH directions — a struct
field the CRD lacks is a knob the API server prunes, and a CRD property
the struct lacks denies every pod that opts into the mount. It found a
dead `spec.resources` on its first run: accepted, stored, and never read
by the injector, which takes the sidecar's resources from the chart.

## [1.39.0] - 2026-08-26

**A lean-scoped correctness release, and it supersedes 1.38.0 for anyone
running `boundaryMode: gated`.** Scope matches 1.38.0: only the
`flint-lean` chart (now `0.4.0`) and the two images it pulls are
published. The CSI driver chart, the lite charts and the SPDK target
image are untouched and stay at 1.37.0.

### Fixed — two independent paths that destroyed committed data

Both are `gated` only, which is opt-in — but gated exists to protect
coherent views, so the exposure landed exactly on the workspaces that
asked for the strongest guarantee. Both shipped in `flint-sync:1.38.0`.

- **The reaper deleted the successor's cited version.** The rule was
  "delete every version of a touched key that is neither the cited one
  nor current". Its `is_current` guard protects exactly ONE version, on
  the assumption that at most one foreign generation appears between the
  lane and the citation — and a successor in gated mode does not stop at
  one, because its cadence is stage → cite → stage. A straggler thawing
  inside the reaper therefore found the successor's CITED version
  noncurrent-and-not-`keep`, and deleted it. The plan asserted the
  opposite in four places. Now narrowed to the version this workspace's
  own record names, fenced every 8 paths, and fail-closed when the
  installed manifest names no version.

- **The lane deleted versions the boundary cites, and no crash was
  required.** The upload lane reclaimed superseded versions under ONE
  guard — "not the version I just wrote" — with no reference to the
  installed manifest. `citation_pass` clears the pending stage LAST,
  after the CAS, the reaper, the baseline save and the intent clear, and
  four ordinary `?` returns sit in that window (the withheld-delete GC's
  `store.delete`, its HEAD arm, `append_conflict`, and
  `reclaim_superseded`, which itself awaits `renew_if_due` and
  `verify_not_deposed`). One transient store error after a successful
  CAS left a stage naming versions the boundary now cites, and the next
  lane pass reaped them. There were TWO such sites — the supersede path
  and the cancel path, the second with no manifest reference at all.
  Both now RECORD into `pending_reclaims`; only the citation-time reaper
  deletes, under its four guards.

### Fixed — availability and correctness

- **A restarted claimant stranded its agent.** An incarnation that came
  up owing an ack it could never honor sat in `claim` without answering,
  leaving the agent blocked on a sentinel forever. It now writes
  `refused-fenced` and flips the capability marker before waiting.
- **A stale ack retired a fresh boundary.** `ack_matches` compared too
  loosely, so an ack from a previous honor could satisfy a pending
  record written after it.
- **A FIFO at a sentinel path wedged the poll arm.** `read_bounded` now
  opens `O_NONBLOCK | O_NOFOLLOW` and refuses anything that is not a
  regular file, with a once-per-process notice rather than a log flood.
- **`recover-staged` ignored the durable orphan summary** and paid for a
  prefix-wide version listing every time.
- **`/status` paid for a HEAD it already had** — the manifest's
  last-modified now rides the single GET.

### Changed

- **`stagedBacklogCapObjects` now counts recorded version reclaims as
  well as staged paths.** The old predicate counted paths only, so one
  hot file rewritten every tick was a single stage entry forever and the
  cap never fired on it — while `drain_need_secs` sized the pod's
  termination grace from that same cap. Measured: recorded reclaims grow
  at exactly 1 per hot path per tick, and the cap now beats the
  visibility-lag bound above roughly 83 continuously-rewritten paths at
  `floorSecs: 60` / `visibilityLagBoundSecs: 3600`. Below that it is
  inert. Total `DeleteObjectVersion` count is unchanged — the deletes
  moved from the lane to the citation rather than doubling.

### Release machinery

- **`flint-sync`, `flint-lean-operator` and `flint-lean-gateway` are now
  staged and published by the scripts.** Until this release the image
  every workspace pod RUNS was built by hand from a recipe in a comment
  at the top of its own Dockerfile — absent from `stage-prebuilt.sh` and
  from `publish-images.sh`, with only `release.sh`'s after-the-fact "is
  the tag on the Hub?" check standing between it and a silent wrong-code
  release. `stage-prebuilt.sh` gained a second staleness clock for the
  lean crate that includes `crates/flint-store`, because the sidecar
  links it and a store edit changes the binary with no `lean/sidecar`
  file touched.

### Verification

`lean/sidecar` battery **117/117** · TLC gate **65/65** (up from 63:
`ProbeGatedGC` proves a withheld delete actually LANDS at a citation,
`ProbeGatedRestart` makes `Inv_NoResurrection` falsifiable in the mode
that widens the resurrection window) · bucket drill **28/28** with the
roster reconciled · kind boundary drill 14/14 · agent-pod rig 9/9
against the released chart.

## [1.38.0] - 2026-08-26

**A lean-scoped release.** Only the `flint-lean` chart and the two images
it pulls are published; the CSI driver chart, the lite charts and the
SPDK target image are untouched and stay at 1.37.0. The CSI chart has
zero files changed since v1.37.0, and republishing every image at a new
tag to ship a lean chart would be cost without meaning.

**One image, two names** (chart `0.3.0`). The lean control plane ships in
the same build artifact as `flint-lite-operator` — same crate, same
build, the chart picks the binary — but asking someone to pull an image
called "lite" to install flint-lean reads as a dependency it does not
have. That image is now also published as `dilipdalton/flint-lean-operator`,
copied cross-repo from the **identical manifest digest** (no rebuild), and
the lean chart names it. Nothing else changes: same bits, same tags, and
`scripts/release.sh` refuses to publish the chart unless the two digests
are equal, so the names cannot drift.

### Added

- **Boundary verbs.** An agent declares a coherent point by writing
  `.flint/publish` (or `.flint/sync`) into the workspace it already has,
  and reads the answer from `.flint/publish.ack`: `ok` means the named
  boundary is installed in the bucket, `partial` means it installed
  without something the agent declared, `refused-fenced` means this
  sidecar lost the lease and is saying so rather than leaving the agent
  waiting. No port, no client, no credential — the workspace is the
  interface. A workspace that ignores them pays nothing: measured at 20
  bucket requests per 22 s idle with the verbs off and 20 with them on.
- **`.flint/remote.seq`** — a local news ticker fed by the barrier's own
  HEAD, so N agents learn of foreign publishes at zero added bucket
  cost, with a heartbeat field that separates "no news" from "sidecar
  dead".
- **Gated boundary mode** — durability split from visibility. The upload
  lane makes every changed file durable as a new object version
  immediately; ONE compare-and-swap then decides when readers may see
  the whole set. A reader sees the entire change or none of it. Gated is
  REFUSED without a visibility lag bound, so unbounded staleness is
  impossible by construction.
- **`flint-sync recover-staged`** and a durable `orphans.json`, so
  uncited work is recoverable from bucket truth alone after a pod
  replacement takes the emptyDir with it.
- **The lean operator, CRD and mutating webhook** — one
  FlintLeanWorkspace per subtree; the webhook injects the sidecar as a
  native sidecar so the workspace is materialized before the agent's
  first line, and an opted-in pod whose workspace is missing or refused
  does not schedule.
- **Opt-in `/metrics`**, rendered from the same struct as
  `gauges.json`, with the label set fixed at `{workspace, namespace}`.
- **A production image recipe for `flint-sync`**
  (`docker/Dockerfile.sync.prebuilt`) and the lean binaries added to the
  operator image — see Fixed.

### Fixed

- **The chart could not install.** `flint-lean-chart` execs
  `/usr/local/bin/flint-lean-operator` from the `flint-lite-operator`
  image, and that binary was not in it — an install was a
  CrashLoopBackOff on "no such file or directory". The sidecar image the
  webhook injects had no production recipe anywhere. Both fixed, and
  `scripts/release.sh` now gates the lean chart the way it gates the
  others: `grep -c flint-lean scripts/release.sh` was 0.
- **The sidecar could not reach a TLS S3 endpoint.** `flint-store` builds
  the AWS SDK with `rustls`, which resolves to `rustls-native-certs` —
  the system trust store — so a certless base image fails every HTTPS
  endpoint. Every drill missed it because MinIO is plain HTTP. The image
  carries `ca-certificates`, and a shell, which the injected startupProbe
  needs.
- **A long barrier starved the lease renewal.** The run loop is one
  `select!`, so while the floor arm ran — and that call contains the
  whole upload loop — the renewal arm could not fire. A deposed straggler
  therefore could not learn it was deposed, and worse, a HEALTHY sidecar
  could outrun the 60 s takeover window and have a standby take the lease
  off a live writer. The upload phase is chunked and renews between
  chunks if due: no added requests on a short barrier.
- **The gateway ignored `pinned_reads`** and returned 409 for every
  staged-but-uncited file — the human read path went dark exactly when
  gated mode was doing its job. It now resolves the cited `version_id`
  the way `checkout` does.
- **No boundary recorded which clock installed it** outside gated mode,
  and once stamped, the drain's ack and its manifest named two different
  clocks. Both fixed; the ack and the bucket now agree.
- **`flint-sync status` could never report a pending sentinel** — it
  spelled the record's filename a second time and got it wrong.
- **A resumed checkout could adopt a stale generation**, stamping the
  baseline with the cited etag over older content, leaving a divergence
  nothing would reconcile. Present files are now verified by size and
  CRC before adoption.
- **The webhook's mount injection collided silently.** A pod declaring
  its own mount at the workspace path failed admission with the API
  server's "must be unique", naming neither flint nor the knob. It now
  refuses with the path, the offending volume and `spec.mountPath`.

### Verification

Lean battery 102, lean formal gate 61 runs, boundary drill 14 legs on
kind, bucket drill 27 legs against a real MinIO in one pass, and the
published chart installed on a three-node AWS cluster publishing to real
S3 over TLS. Verbatim logs are committed under `lean/e2e/results/`.

### Known limitations

- The S3 proxy is the plan of record and is not built; until it lands,
  `credentialsSecretRef` gives the sidecar a scoped credential (the agent
  container holds none in either posture).
- No enforced data-plane fencing: a deposed sidecar's writes land as
  uncited versions that destroy nothing and no coherent reader resolves,
  but a raw-key reader can still see them.
- One writer per workspace subtree; full checkout only; the v1 file-count
  cap is ~250k and falls out of manifest size, not byte count.

## [1.37.0] - 2026-08-24

**The conformance release.** The NFS server was written from scratch and
had only ever been measured by one suite, pynfs, whose last recorded run
was four months and 17,827 insertions ago. Pointing a second suite at it
— `nfstest`, and only two of its seventeen tools — found three shipped
bugs in an afternoon. Re-running pynfs found that three of the four
failures it had recorded as "deliberately deferred" were already fixed
and nobody had noticed, and the fourth was misdiagnosed. pynfs is now
**171 passed, 0 failed, 91 skipped** — the first clean run this server
has had, against a Linux server running as root, which is the only
posture whose results are attributable.

Minor rather than patch: `CREATE` now honours attributes it previously
discarded, and a directory listing returns cookies of a different shape.
Nothing on the declared SemVer surface (CSI gRPC verbs, StorageClass
`parameters`, `volume_context` keys) changed.

### Fixed — the NFSv4 wire, found by pointing a second suite at it

- **Every byte-range lock from a non-zero offset to end-of-file was
  refused.** `length == 0` was treated as the to-EOF sentinel; RFC 8881
  §18.10.3 says the sentinel is all 1s and that zero is invalid. A Linux
  client sends all 1s, so `offset.checked_add(u64::MAX)` overflowed for
  every offset above zero and answered NFS4ERR_INVAL — the plain `fcntl`
  `l_len = 0` idiom. Offset 0 worked, which is why it survived: the one
  case anyone tries by hand is the one case that works. LOCKT and LOCKU
  had no range validation at all; an unvalidated LOCKU trims nothing and
  still answers NFS4_OK, so a client believes it released a lock it still
  holds. `nfstest_lock`: 324 failures to **5296/5296**.
- **`mkdir(0700)` produced a world-readable directory.** The COMPOUND
  decoder parsed `createattrs` and threw them away — "consumed for wire
  alignment" — and the dispatcher handed the handler a hardcoded empty
  attribute set, so every requested mode was dropped and every directory
  came out `0755`. The handler was correct the whole time, which is why a
  unit test at that level passed with the bug fully present.
- **A directory listing could silently omit a file.** READDIR cookies
  were positions in a fresh re-enumeration, and the cookie verifier had
  one-second granularity, so a mutation in the same wall-clock second
  shifted every later index while the verifier still said "unchanged" —
  an entry present for the whole listing was left out of an NFS4_OK
  reply. Cookies now come from the directory stream's own offsets and
  the verifier is nanosecond-granular. The old design also re-`stat`ed
  the entire directory on every call, so a full listing cost
  O(entries²) syscalls.
- **A non-reclaim OPEN succeeded during the grace period.** Grace
  answered two questions with one boolean: whether the window is open,
  and whether anything is reclaimable. These are now separate, so an
  unreclaimed 4.1 client sees NFS4ERR_GRACE (RFC 8881 §18.51.3) while
  the hub's own file API — which dispatches with no session and can
  never *be* reclaim-complete — is held only when something really is
  reclaimable. Closes pynfs RECC3.
- **Unbounded allocation from an unauthenticated frame.** Array counts
  were read off the wire and passed to `Vec::with_capacity` before any
  bounds check, so a ~30-byte COMPOUND declaring `op_count = 0xFFFFFFFF`
  requested a multi-hundred-GiB allocation from any host that can reach
  port 2049 — decode runs before the RPC credential is inspected. On
  Linux with default overcommit the reservation is lazy and the server
  survives, so this is memory amplification rather than a remote kill in
  that configuration; it aborts under strict overcommit. Bounded at all
  six unguarded sites against what the remaining bytes could describe.

### Fixed — the hub and the standalone server had drifted apart

The flint-lite hub runs `flint-pnfs-mds`, not `flint-nfs-server`. Three
recovery hooks had been wired into the latter only.

- **Byte-range locks were never persisted or restored on the hub.** The
  lock *stateids* survived and EXCHANGE_ID answered CONFIRMED_R, so a
  client concluded "session lost, state intact", never reclaimed, and was
  never told — while a second client's conflicting LOCK was granted. Both
  front-ends now bind and restore through one call, so the two cannot
  drift again.
- **F33 self-fencing was never armed by the binary flint-lite ships.** A
  node fault that stops kubelet but not the pod left the process alive
  with wedged I/O while clients hung on established flows.
- **An unusable `state.db` crash-looped the hub on a share whose data was
  intact.** Reachable from an ordinary image rollback, since there are no
  migrations. Now quarantined and recreated when the database holds only
  bookkeeping, and still refused when it is a data map (pNFS/block
  volumes, or any tiered hub, where `tier_evicted` rows decide whether a
  file reads as its contents or as zero bytes).

### Changed

- CI runs `cargo test --lib --tests`. Roughly 49 integration tests were
  compiling and passing and were never being executed by the job.
- `make test-nfs-protocol` is gated by `scripts/check-pynfs.py` against a
  committed floor, and no longer ends in `|| true` — under which a suite
  that collapsed to zero passes, or never started, still exited 0.
- `tests/delegation_test.rs` deleted: two tests that printed a list of
  assertions living in another module and then called `assert!(true)`.

## [1.36.0] - 2026-08-23

**The many-clusters release.** The common shape for an agent fleet is
several k8s clusters mounting one hub, and it had never been drilled at
cluster level. Doing so found six defects, all shipped, all in the same
place: NFSv4 client identity is `Linux NFSv4.<minor> <nodename>` and
nothing else, so two clusters routinely present byte-identical bytes and
the server is required by RFC 8881 §18.35.5 to read the second as the
first one rebooting.

Minor rather than patch for one reason: `phase: Ready` now carries an
additional precondition. Nothing on the declared SemVer surface (CSI gRPC
verbs, StorageClass `parameters`, `volume_context` keys) changed, but a
share's observable status did, and a version number is a poor place to
hide that.

### Fixed — NFSv4 client identity, one hub and many clusters

- **A colliding `co_ownerid` cost another cluster its locks, permanently.**
  The RFC 8881 case-5 cascade tore down sessions, stateids, delegations
  and the client record but not the locks — and structurally could not,
  because the session handler held no reference to the lock table. Since
  `remove_client` drops the lease first, and the only reaper iterates
  expired *leases*, what was left behind was unreachable by every code
  path there is. Persisted and re-seeded at startup, so a restart did not
  clear it either, and the range was refused to every agent in every
  cluster for the life of the volume.
- **An `nconnect >= 2` mount silently dropped the case-5 cleanup
  obligation.** The Linux client sends one EXCHANGE_ID per connection for
  trunking detection, so the second one hits case 4, replaces the
  unconfirmed record, and started the replacement's `pending_replaces` at
  `None`. This cut both ways: it *masked* the cross-cluster steal, which
  is why a rig kept failing to reproduce it, while breaking reboot cleanup
  for everyone. pynfs EID5f passes over it because it uses one connection.
- **The owner-index guard reached one removal site of four.** The
  conditional removal went in on the EXCHANGE_ID replacement path and
  stopped there; the public `remove_client` — reached from
  DESTROY_CLIENTID, the lease sweep and the case-5 cascade — kept an
  unconditional remove, so a departing client evicted a *live* peer and
  that peer's next EXCHANGE_ID minted a second clientid for an owner that
  already had one.

### Fixed — the lease sweep

- **A sweep that stripped a client's locks could then decline to retire
  it.** `courtesy_release_expired` read the expired set, stripped those
  clients' locks, then called `cleanup_expired()`, which read the expired
  set *a second time*. `renew_lease` is lock-free, so a SEQUENCE landing
  between the two reads renewed the lease and the second read no longer
  saw the client: its locks were already gone, its session and stateids
  survived, and `sr_status_flags` is hardcoded 0 — so nothing told it. It
  carried on believing it held a byte range the server had handed away.
- **A lock granted mid-sweep was orphaned forever.** Stripping locks
  before retiring the record left a window in which a lock could still be
  granted to a client the sweep was about to destroy — after which it had
  no client, no lease and no reaper. The record is now retired first,
  which closes the window because `LOCK` can no longer be founded on a
  client that is gone.
- Both were found by TLC, not by review, and neither is reachable with a
  single client: both counterexamples require a second agent, because the
  sweep runs at the top of every COMPOUND and reaps *every* expired
  client rather than the caller's.

### Fixed — the operator

- **`Ready` could be reported with no address.** The phase was decided
  from `available_replicas` alone while `status.address` was resolved
  separately, so a `type: LoadBalancer` share with no `advertiseAddress`
  published `Ready` with a null address for as long as the cloud provider
  took to populate `status.loadBalancer.ingress` — minutes, on AWS, and
  cross-cluster consumers are exactly the population that needs a
  LoadBalancer. The address is now an input to the phase.
- **The lite chart's NetworkPolicy admitted the wrong port.** It rendered
  `.Values.service.port`, so a non-default port admitted the Service's
  port while the pod listens on 2049 — every allowlisted CIDR denied.
- **`nfsClientCIDRs` cannot restrict cross-cluster consumers**, and both
  charts said it could. With `externalTrafficPolicy` unset, kube-proxy
  SNATs: measured 1486 of 1486 connections arriving from the hub's own
  gateway address and none from either remote node. Corrected in place
  rather than removed, because the knob still works for same-cluster
  consumers.

### Fixed — the front door told cross-cluster consumers the wrong thing

- **`mountHazard` was `null` in exactly the configuration a real cluster
  proved unsafe.** The gateway returned "no hazard" whenever
  `suspendWithSessions: false` was set, on the reasoning that "the ladder
  will hold while any client holds a lease". It does not: a lease
  *expires*, so a client that is partitioned rather than gone stops
  renewing, the count reaches zero on its own, and the guard stops
  guarding — which the CRD's own documentation says two files away, and
  which was measured across a one-way packet cut as guard-holding at
  t=49–99, lease 1 → 0 at t=99, suspended at t=111 with the mount still
  held. Both branches now warn, and both lead with the keepalive, because
  that call crosses a different network path from the mount and so still
  arrives when the mount's path is cut. Wire-visible: `mountWarning`
  appears where it was previously null.
- The unit test asserting that silence was itself pinning the defect
  (`assert_eq!(mount_hazard(...), None)`); it now asserts the requirement,
  with an anti-vacuity check that the two branches still say different
  things.
- **`activeLeases` is documented as "unexpired NFSv4 leases" and is not.**
  It is `LeaseManager::active_count`, which is `leases.len()` — lease
  *rows*, including expired-but-unswept ones — so it runs up to one 30s
  sweep behind. The error is in the protective direction and is precisely
  why the drill's guard survived to t=99 rather than lapsing at the 90s
  lease, which is the reason it could not be left wrong: anything
  reasoning about how long the guard lasts must add the sweep interval.

### Added — a design of record for idle-suspend under a remote mount

- `docs/plans/idle-suspend-cross-cluster-design.md`, from a 20-agent
  design workflow. **Not implemented, deliberately.** Its own verdict is
  that a hub-side signal *narrows* this window and cannot close it: at the
  wire, a hard node loss and a partition are the same event, and any rule
  that holds for one holds for the other for as long as it cannot tell
  them apart. The remaining work changes idle behaviour and cost on a
  pure-spot fleet and includes a deliberately unbounded hibernate latch —
  trades to be made deliberately, not inherited from a release.
- It also corrected two things worth recording: a credential-free wake
  path already ships (`flint-hub-gateway`'s `/wake`, one bearer token, no
  Kubernetes credential) and was simply absent from the documentation a
  cross-cluster operator would read; and the obvious fix — hold when a
  client vanishes without saying goodbye — would have partly reverted the
  periodic lease reaper added three days earlier, which exists because a
  partitioned share sat pinned awake for 770 seconds.

### Added — a formal model of the client-record lifecycle

- `formal/FlintClientIdentity.tla`, 15 gate runs, written because the
  drill found three defects in one state machine by hand in an afternoon
  — that density, not a design question, is the signal that says model it.
  It then found three more, including the one-removal-site-of-four above,
  which it caught by *shape* rather than by a run: the model applies its
  index guard at every removal site uniformly, so the asymmetry in the
  code had nowhere to hide.
- Two required-pass vacuity probes keep it honest. `NoCollide` runs with
  unique owners and **all three defects switched back on** and finds
  nothing, which is the machine-checked statement that the natural
  abstraction makes every theorem in the module vacuous. `Nconnect1` does
  the same from the other side and explains the pynfs gap.
- The lease dimension models no clock at all — a lease lapses by a
  nondeterministic action — so no result can be an artifact of the 90s/30s
  numbers, which is exactly what a rig cannot control.
- One design result worth knowing before implementing the obvious fix:
  `sr_status_flags` is addressed to a **client id**, and two clusters
  sharing a `co_ownerid` share one, so a revocation notice can be consumed
  by the wrong cluster entirely. Unique client names are a *precondition*
  for that report being deliverable, not merely a way to avoid state loss.
- Gate: 189 → 196 runs, 14 spec modules.

### Added — tests

- `tests/regression/many-clusters-one-hub.sh`, a three-cluster drill:
  baseline, identity collision (counting distinct owner *byte arrays*,
  since the hub logs them as `{:?}` and grepping for the literal string
  matches nothing), the idle ladder, and a partition leg driven with
  `iptables -j DROP` — AWS security groups are stateful and cannot
  express a one-way cut.
- The partition leg carries a control: a connected, quiet share at 3.4x
  the idle threshold that never suspends. Without it the leg would pass on
  a hub that suspended for any reason at all.

### Documentation

- The agent-fleet guide gains "One hub, many clusters: give every client a
  unique name" — the wire capture, which name matters for a
  kubelet-mounted PV versus a pod that mounts itself, and how to audit a
  running fleet.
- **The guide's HTML and PDF never contained that warning at all.** They
  were known to be stale; they were in fact missing the single most
  important item for the shared-hub case. Both regenerated.

## [1.35.1] - 2026-08-22

Four fixes, two of which cost data or client state rather than time.
Nothing new — the SemVer surface (CSI gRPC verbs, StorageClass
`parameters`, `volume_context` keys) is unchanged.

### Fixed — the tier

- **A hub that could not read the bucket's manifest could still publish
  an empty one over it.** On a manifest it cannot parse, the hub logs
  "The hub will serve an empty export; do NOT let it publish over the
  bucket" — and nothing enforced that. `set_import_refused` wrote a
  status string read only by two status surfaces, the flush loop was
  spawned unconditionally twelve lines later, and every tick ended in
  `write_manifest_barrier`, whose only guard was the epoch fence.
  Directories, symlinks and every mode/uid/gid exist **only** in the
  manifest, so one barrier from an empty export erases the tree's shape;
  `rpo::evaluate` then reports clean and the idle ladder deletes the PVC
  holding the last copy. The orchestrator now carries a publish fence,
  checked before the epoch check and again inside the barrier itself.

- **The write reserve was defeated by write speed** (drill item D8).
  `admit_bytes` compared a 2s-stale gauge against one write's length, and
  it is called **per WRITE op**, not per PUT — so a streaming write had
  each chunk admitted against a snapshot that had not seen its
  predecessors. Measured: 600 MiB onto 539 MiB free returned 201 on
  every chunk and consumed the whole reserve with `nospcWriteRefusals`
  still 0. Admissions are now tallied against the cached gauge, which
  forces a real `statvfs` before the reserve is spent. The tally never
  decides a refusal — that is still made on fresh numbers, so this
  cannot produce a false NOSPC. Cluster-measured on a 1Gi volume with a
  991 MiB PUT: unfixed left 158 MiB of a 256 MiB reserve, fixed left 256.

### Fixed — pNFS

- **Every flint-lite hub advertised the same NFS server identity.** The
  MDS passed an empty `volume_id`, landing in the arm that returns the
  constants `flint-nfs` / `flint-nfs-standalone`, while each hub mints
  clientids from 1 out of its own table. The kernel's
  `nfs4_detect_session_trunking` treats same-owner servers as one server
  across addresses and requires EXCHANGE_ID to return the same clientid
  on each, so an agent mounting **two workspaces** was handed two
  unrelated hubs under one identity. The hub now advertises its
  persistent server id, which already lives in the state.db on the PVC
  and is therefore stable across restarts.
  **Upgrade note:** a hub that has already served clients changes its
  advertised identity once, on the restart that adopts this build.
  Clients re-establish state rather than reclaiming it — a stall on a
  hard mount, within the restart the upgrade already costs.

### Fixed — the operator

- **Hibernation released the disk up to 30 minutes late.** The reclaim
  runs on a later reconcile because the hub needs its full termination
  grace to drain, but only one follow-up was scheduled (15s) against a
  120s default grace, and on that near-certain miss the share fell into
  the parked requeue (1800s). Nothing rescued it: Pods are not watched,
  so the hub pod finally disappearing raised no event. Measured before
  the fix: `Hibernated` at 19:44:21, PVC still Bound eleven minutes
  later. After: reclaimed 11s after `Hibernated`. No data was ever at
  risk — but `status.phase: Hibernated` read as "disk released" while it
  was still allocated.

### Added — tests

- Seven regression drills under `tests/regression/`, including a
  fence-and-identity drill whose control is wired into its exit code: it
  runs the same legs against the previous image and **fails if they pass
  there**.

## [1.35.0] - 2026-08-21

The gateway release. Every hub already served an HTTP file API, but
reaching one meant knowing its in-cluster address and holding its
per-share bearer token — so a projects service that browses 3000 of
them needed 3000 addresses and 3000 credentials. `flint-hub-gateway`
is one door in front of all of them.

### Added — flint-hub-gateway

- **One addressable door for every hub's file API**, off by default
  (`gateway.enabled`). It ships **inside the `flint-lite-operator`
  image** — same crate, same build, a different `command` — so
  enabling it pulls no new image and there is no second thing to
  publish, scan and keep in step.

- **One credential instead of one per project.** Each share's token is
  `HMAC-SHA256(root, endpoint:bucket:keyPrefix:version)`, so a single
  root key derives every hub's credential and there is nothing to store
  or fan out. Whoever provisions a share writes the same value into
  that share's Secret; `flint-hub-gateway --derive-for <ns>/<name>`
  reads the CR and prints what the serving gateway will compute, so
  there is one implementation and not two. Revoke one project by
  bumping its `chert.us/api-token-version` annotation.

- **A project may have several volumes.** Addressed as
  `/v1/projects/<id>/volumes/<v>/files*`, resolved from
  `chert.us/project-id` + `chert.us/volume-id`. Nothing in the operator
  reads the project id — uniqueness keys on the bucket prefix subtree —
  so N hubs per project was always legal and is now addressable. A bare
  path on a multi-volume project answers **409 naming the choice**,
  never a silent pick.

- **`/status` is unreachable through it, by construction.** The hub
  serves an unauthenticated `/status` on the same listener as the file
  API — tier recovery point, epoch holder, lease list, lifecycle phase.
  The gateway's verb table is a closed enum whose upstream path is a
  `&'static str`, so no caller byte reaches the path and no request
  shape — including handwritten `..` traversal that `reqwest`
  normalises away — can reach it.

- **RBAC is `get,list,watch,patch` on flintshares and nothing else.**
  No Secrets, which is the point: the workspace namespaces hold every
  tenant's S3 credentials beside the per-share API tokens. No `create`
  (provisioning stays the front door's decision), no `delete`, no
  `update`.

- **`POST …/wake`** returns a mountable `{address, serverId}` and
  **re-stamps on every call — it is the keepalive**. The idle ladder
  suspends when the wake annotation is stale AND the hub's own activity
  clock is quiet, and **an agent holding an NFS mount while computing
  in memory trips both**. A suspended share under a `hard` mount blocks
  in uninterruptible sleep and nothing wakes it, because an NFS client
  cannot write a Kubernetes annotation. Pair with
  `idle.suspendWithSessions: false`, which is opt-in — the default
  suspends even while a client holds a lease.

- **`?wake=false` for fleet crawls.** A projects service iterating
  every project must not start 2700 parked hubs, each `Hibernated` one
  a full DR import. It refuses with `503 Parked` and no `Retry-After`,
  in under a second, stamping nothing. A typo is a `400`, never a
  default. `GET …/volumes` touches no hub at all and reports `serving`
  per volume.

- **Bodies stream, in both directions.** Measured through a 128Mi
  container: a 256 MiB round trip moved peak RSS from 10 MiB to 12 MiB,
  byte-identical, with the checksum as the guard that anything moved at
  all. A cold read relays the hub's `503` **with its `Retry-After`
  intact** rather than substituting a bare 502 at the proxy's own
  deadline — a download waits the hub's `hydrateWaitSecs` plus a
  margin, because the two budgets otherwise race and default to the
  same 30s.

### Added — operator

- **`status.apiEndpoint`: an addressable door for the hub's 8080.** A
  per-share **headless** Service (`clusterIP: None`), because 3000
  routable Services cannot be guarded and would exhaust the service
  CIDR. Exposure is concentrated in the one gateway instead.

- **A conflict loser is told who won.** `status.conflictWith` carries
  `{namespace, name, prefix, relation, subPath}` and a `CONFLICT`
  printer column — machine-readable, instead of a sentence to regex out
  of a condition message. The address is published **only when the
  winner is in the same namespace**, since handing out a mount target
  across namespaces would answer a typo'd prefix with a pointer at
  another tenant's live data.

- **`/status` now says whether the file API is actually serving.** A
  `fileApi.enabled: true` share whose token Secret uses the wrong KEY
  produced a hub indistinguishable from a healthy one: the route table
  is never assembled, so every `/files*` call answers **404, not 401**,
  while `/status` answers 200 on the same socket and the pod is Ready.
  `fileApi.routesMounted` distinguishes the three cases, and absent
  still means "never asked for".

### Fixed

- **The hub's phase gate answered strangers before auth ran.** The gate
  composed ahead of authentication, and it rejects with a 503 whose
  body NAMES the phase — so anyone who could reach the port learned
  `Starting`, `ClaimingEpoch`, `Importing`, `Reconciling` or `Draining`
  without a credential, and the `Retry-After` confirmed the gate had
  fired even when the body did not. Those are exactly the phases that
  say a share is mid-DR-import or mid-drain. Auth runs first now; a
  valid token still gets the 503 and the `Retry-After`, which is the
  whole value of the gate.

- **`.flint/` was reserved only at depth 1, not throughout the tree.**
  Flush and import both tested only the first path component, so
  `nested/.flint/epoch` read as an ordinary client file in both
  directions. Reachable when one share's prefix is an ancestor of
  another's — the case the store-side epoch **cannot** fence, because
  it keys on the exact prefix string so `t/` and `t/sub/` never contend
  — and then import materializes the inner share's live control objects
  as client files, and flush publishes them back over that share's live
  epoch cell. Neither side ever errored. Reserved at every depth now;
  nothing legitimate is refused.

- **`networkPolicy.apiClientSelectors` had never produced a valid
  peer.** One `nindent` short, and it failed in two shapes: two keys
  (the shape a real front door needs) aborted `helm template`
  outright, while one key — the shape the operator guide documented —
  rendered *successfully* as `{podSelector: null, matchLabels: {…}}`,
  which is not a NetworkPolicyPeer. An empty podSelector selects every
  pod in the namespace and the stray `matchLabels` is ignored, so the
  rule admitted something nobody wrote and helm exited 0. Any claim
  that a browse front door was admitted to a hub's 8080 by selectors
  was false in the field.

### Security

- Eight kubeconfigs are no longer tracked, and internal hostnames are
  gone from the tree. This repository is public.

### Upgrading

- **⚠ CRD schema version 4 → 6.** The self-bootstrap refuses to
  downgrade a newer schema, so a mixed fleet is safe, but a 1.34.0
  operator does not know `status.conflictWith` or `status.apiEndpoint`.
- The gateway is **off by default**; nothing changes for an install
  that does not set `gateway.enabled`.
- Enabling `networkPolicy` admits the gateway to the hubs' 8080
  automatically. That peer fails **closed** — a wrong selector times
  out every file request while the policy still reads correctly — so
  it is now asserted against an enforcing CNI rather than only
  rendered.

## [1.34.0] - 2026-08-20

The disk-sizing release. **This section was reconstructed after the
fact**: 1.34.0 was tagged and published without a changelog entry, and
the release policy requires one because the GitHub release notes
mirror it verbatim. The content below comes from the chart's own image
notes, written at the time.

### Added

- **`persistence.autoExpand`** grows a share's claim from what the
  bucket actually holds. The hub publishes the project's
  `logicalBytes` and `largestObjectBytes`; the operator sizes against
  them (`bufferPercent` default 100, `maxSize` required). Growth is an
  in-place expansion — same PVC, no data movement, no outage. **The
  operator still does not write spec**: the target rides an annotation
  recording the `persistence.size` it came from, so editing size always
  wins. ⚠ Needs a StorageClass with `allowVolumeExpansion: true`, or
  the API server refuses and the share reports `ExpansionRefused`.
- **`persistence.reprovisionOnShrink`** honours a smaller size by
  rebuilding the disk: verify the bucket can restore the tree, release
  the claim, create a new one, import. Refused for a share with no
  bucket (its PVC is the only copy) and for an adopted `existingClaim`.
  New `Reprovisioning` phase; costs a wake, so a fresh `serverId` and
  every client remounts. Deliberately **not** abortable by
  `requested-at` — a rebuild was asked for explicitly.
- The two cannot fight: a shrink that `autoExpand` would simply grow
  back is refused with its reason, rather than spending an outage and a
  DR import to end up where it started.

### Fixed

- **⚠ A shipped bug: an object larger than the PVC minus its reserve
  hung the read FOREVER.** The demand hydration lane could not tell "no
  room now" from "no room ever" and treated the first as wait — but
  eviction reclaims only what is already local, so that wait could
  never end. The marker stayed set, every content lane answered
  `NFS4ERR_DELAY`, and a hard mount hung with no error, no counter and
  no condition. Now `NOSPC` at request time, counted, and surfaced as a
  `HydrationUnblocked` condition naming the size to raise the claim
  past.
- The operator read the hub's tier gauges from the **wrong path** (top
  level, not under `tier`). With `#[serde(default)]` a wrong path
  parses cleanly and yields `None` forever, so `HydrationUnblocked`
  reported a vacuous `True` and `autoExpand` would have shipped inert.

### Upgrading

- **⚠ CRD schema version 2 → 4.** A 1.33.0 operator does not know the
  `Reprovisioning` phase, and `status.phase` is a schema enum.

## [1.33.0] - 2026-08-20

The fleet-survival release. The flint-lite operator can now hold its
stated design target — 3000 `FlintShare`s with ~300 live hubs — which
it demonstrably could not before. Every claim below was measured on a
real cluster, and the two that could not be measured cleanly are named
as such rather than quoted.

### Fixed — the operator OOMKilled at fleet scale

- **At 3000 shares both operator replicas were `OOMKilled`** (exit 137)
  against the 256Mi limit, 8 restarts each and still climbing 26 minutes
  in, having brought up 131 of 300 hubs. The fleet **never converged**,
  because the CrashLoop re-enters the same herd.

  The cause was one unset knob: the controller's reconcile concurrency
  is `Config::default()`, which is `0` — documented as UNBOUNDED — and
  this binary never overrode it. A cold start therefore admitted ~3000
  simultaneous reconciles, each snapshotting the whole fleet: ~2–3 GB of
  transient allocation.

  Capped at 32 with a 250 ms debounce. Same cluster, same fleet: **0
  restarts, 49–53 MiB, 300 of 300 hubs, converged in 1389s.**

### Changed — arbitration stops being the dominant CPU term

- `conflict::admit` is O(rank²) and ran on **every reconcile**,
  re-deriving an answer that only changes when the fleet changes.
  Measured at N=3000: **13.5 ms** for the median share, **52.6 ms** for
  the newest, so a full fleet pass was **~47 seconds of CPU** — 0.16 of
  a core steady against a chart request of `50m`, and 3.1 cores at the
  fastest legal requeue setting.

  Replaced by a table built once per fleet change: **~2.5 ms for the
  whole fleet, 18,516×.**

  **A note for anyone reimplementing this: a `BTreeSet` successor lookup
  is wrong.** Sibling prefixes are incomparable, so several descendants
  of one prefix are admitted together — and `admit` names the **oldest**
  overlap while a successor lookup names the **lexicographically
  first**. The winner is published in the rejection message, so naming
  the wrong one is a user-visible lie. The index takes the minimum age
  rank over the descendant range.

### Fixed — the operator triggered itself

- Two condition **messages** embedded a live seconds counter, so
  `status` changed on nearly every reconcile, which fired the operator's
  own FlintShare watch, which scheduled another reconcile. Gain is
  `1/(1 − min(1, d))` for a reconcile of `d` seconds: **non-terminating
  once a reconcile takes a second**, and invisible at the four-share
  scale every prior drill ran at.

### Changed — a parked share now costs approximately nothing

- Measured before: 3000 shares with 300 live produced **~99 apiserver
  writes/s with nothing changing** (11.83 flintshare applies + 87.15
  child-object applies), most of it 2700 parked shares re-applying four
  identical objects on a timer.

  Parked shares now re-check when their next ladder rung is **near**
  rather than every 300s (clamping the raw threshold capped at 300s, so
  `hibernateAfterSecs: 86400` meant 288 wakeups a day to say "not yet"
  287 times), and skip the applies while a render-hash stamp matches.

  Measured after, 3000 parked shares settled: **0.006–0.024 flintshare
  writes/s and ~0.22/s child objects**, operator at **1m CPU / 77 MiB**.

  **A stale stamp still forces a full apply.** This operator is
  level-triggered — its correctness argument is that it re-asserts
  desired state regardless of what it believes — and a hash cannot see a
  hand-edited ConfigMap or a stripped label. The stamp carries a
  timestamp, and a stale (or future-dated) one re-applies everything.

### Changed — hubs now request resources, and that is visible

- A hub with no `spec.resources` rendered `resources: None` and ran
  **BestEffort**: the scheduler saw a zero-cost pod, packed by pod count
  alone, and every hub was first out under node memory pressure. Hubs
  now default to **100m / 128Mi, requests only, no limits** (a hub is a
  filesystem server whose working set belongs to the caller; a limit
  makes it killable mid-operation).

  **⚠ Plan for it.** 300 live hubs now ask for **30 vCPU**. On a cluster
  that cannot supply it they sit `Unschedulable` instead of silently
  oversubscribing — which is the point, but it is a change in what a
  fleet requires. Measured directly: 162 of 300 pods `Insufficient cpu`
  on a 16-vCPU rig.

- The operator is sized to `500m` / `128Mi` request with a **512Mi**
  limit and **no CPU limit** — reconcile wall time is the denominator of
  the self-trigger amplification above, so throttling the operator
  pushes in exactly the wrong direction. 512Mi is ~5.8× the worst
  measured spike (a watch relist briefly holds the store twice: ~88 MiB
  at 3000 shares).

### Not changed, deliberately

- **The epoch lease stays at 10s × 6 (60s).** `30 × 4` was implemented
  and reverted before release: it is a third of the PUTs for the same
  failure tolerance, but it **doubles takeover latency after an unclean
  death**, and nothing had measured it on a cluster. The last real
  number for a foreign takeover is 80s at 10 × 6; there is no matching
  one for 30 × 4. A timing change to the fencing lease should not ship
  on a unit test. Both remain per-share knobs.

### Verification

1704 tests on macOS, **1712 on real Linux**. Two rig runs on real
clusters (~$2.10 total, both torn down).

**Stated plainly: two things are not measured.** S7 has not been
re-measured against a converged fleet *with* live shares — the
parked-only experiment isolates the term but is not the mixed steady
state. And real exponential backoff (`error_policy` returns a flat 30s
under a doc comment claiming otherwise) is **not built**: a fleet that
cannot converge still pays 15s requeues forever, which is exactly what
made one rig run void.

## [1.32.0] - 2026-08-20

The cold-read-guard release. One shipped bug, found while validating
1.31.0 on a real cluster, plus two documentation defects
that between them stopped a new user mounting a share at all. The
validation run that found it also passed all seven of its own legs
against 1.31.0 — the three fixes that release shipped are now
cluster-proven, not just unit-proven.

### Fixed — a cold read failed its own guard

- **After a hibernate/DR wake, the first `GET` of every file answered
  409.** The download's terminal check refuses when the file's `change`
  attribute moved under the read. That is how a rename-over is caught,
  and it is worth keeping: a caller handed the new file's bytes under
  the old file's `ETag` has been given the wrong object.

  But the tier REWRITES THE LOCAL INODE when it hydrates a stub — a
  pwrite into the marker inode, which `hubfs::render_etag` already
  documents for the `If-Match` case — so on a cold file the hub's own
  hydration moved `change` and the read failed its own guard.

  Measured on a real cluster after a hibernate that deleted the PVC:
  13 of 13 files answered 409 on first touch and 200 on the immediate
  retry. Every caller paid two round trips for bytes that were never in
  doubt, and a caller that did not retry saw a cold project as broken.

  On the streaming path it was worse. Past a committed `200` the
  mismatch cannot be a clean 409 any more, so it poisoned the body
  instead — a reset connection on a file nothing had touched.

  The hydration is now forced with a one-byte probe, parked on exactly
  as the read loop would, BEFORE the guard's baseline is taken. A
  replacement landing in that window is still caught: `fileid` and the
  logical size must both survive it, which a rename-over or a resize
  does not. The `ETag` published is the post-hydration one, because the
  pre-hydration tag names bytes the caller did not receive and would
  412 on their next `If-Match` for no reason they could see.

  Re-proven on the same cluster, same corpus, same hibernate: 13 of 13
  files 200 on first touch, all byte-identical, with `stubsCreated: 13`
  in `/status` confirming the tree really was cold.

### Changed — success is no longer reported under an `error` key

- `POST /files/folder` answered `{"error":"created"}` on a 201, and
  `PUT` / `DELETE` / `POST /files/move` did the same with their own
  words. Success came back through the error body because both shared
  one helper. Anything scanning for `error` to detect failure read every
  successful mutation as a failure.

  They now answer `{"status":"created"|"written"|"removed"|"moved"}`.
  **This is a wire-format change** on the four mutating routes; the
  status codes are unchanged.

### Fixed — neither documented mount command worked

- The operator chart's `NOTES.txt` told users to mount
  `<address>:/data/exports`. That is a path INSIDE the container, and
  the server refuses it with `NFS4ERR_NOENT`. The export is the server
  root.
- The operator guide told users to mount `<status.address>:/`. That
  expands to `host:2049:/`, which `mount` refuses outright.

  Both now show the form that works, and that also survives a
  `spec.service.advertiseAddress` on a port other than 2049 — the host
  before the colon, the port in `-o port=`.

- The operator guide never named the keys `credentialsSecretRef` must
  carry. They are loaded with `envFrom`, so they must be
  `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` verbatim. Anything else
  leaves the SDK with no credentials at all; it then falls back to the
  instance role, and where IMDS is unreachable from pods that surfaces
  as a startup crash loop reading `bucket <name> unreachable: dispatch
  failure` — which names the bucket rather than the cause.

### Known gaps (unchanged, stated so they are not rediscovered)

- `streamThresholdBytes` has no `FlintShare` field, so operator-managed
  shares take the 8 MiB default. (This entry used to say it was
  "settable in `flint-lite-chart`". It was not: `values.yaml` documented
  the knob and `_helpers.tpl` never emitted the key, so chart installs
  took the default too. Believing the chart half worked is what talked
  us out of adding the CRD field. The chart half is fixed; the
  `FlintShare` field is still owed.)
- The write reserve (`admit_bytes` reading a stale gauge with no
  in-flight accounting) is still open.

### Verification

1691 tests on macOS, **1699 on real Linux**, zero failures. The
cold-read fix was re-run with `hydration_is_benign` reverted to the old
always-refuse behaviour and its test fails first.

## [1.31.0] - 2026-08-20

The drill-fix release. Three shipped bugs, all found by the flint-lite
real-cluster drill against real S3, plus a memory fix that matters once
hubs are 1:1 with projects. Every fix here was checked against its own
absence: each regression test was re-run with its fix reverted and
confirmed to fail first.

### Fixed — a stale device number silently emptied the manifest

- **Generation rows are keyed `(dev, ino)`, and `dev` is stable only by
  luck.** A CSI restage can hand the volume back on a different device
  minor. When it moved, every row still LOADED — so this never looked
  like data loss — but none of them matched what the tree walk found, so
  `manifest::build` counted every file as `beyond_rpo`, dropped it from
  `entries`, and the barrier published that manifest over the good one.

  Measured on a real cluster: `dev` moved 66311 → 66312 and
  `tenant-a/.flint/manifest` went from **7919 bytes and 37 entries to
  534 bytes and 4 entries** — one directory and three symlinks, not a
  single entry carrying an S3 key — while all 33 data objects sat intact
  in the bucket, no longer named by it. A share created fresh for the
  drill wrote a literally empty `{"entries":[]}` one second after its
  first restart. Proof it is the device and not the inode: `a5-probe.bin`
  has the *same* inode in the row and the live file.

  It never healed on its own — `dirtyFiles` is 0, so nothing republishes.
  Rewriting one file by hand moved `beyondRpo` 33 → 32 and left a second
  row for it, then it froze.

  The manifest is the sole input to manifest-first cold import, and
  `mode`, `uid`, `gid`, `mtime` and symlink targets live **only** there —
  the foreign-key sweep can recover bytes but not those. What kept the
  drill from losing them is that `rpoClean` also goes permanently false,
  so `HibernateDeferred` refused to reclaim the PVC.

  Startup now re-homes rows to the export root's live `st_dev`. The prune
  is not optional: deletes match on `dev` too, so a file removed during a
  drifted boot leaves its row behind, and re-homing alone would let it
  collide with a REUSED inode and claim another file's object. A row
  survives only if its inode is still live under the export root.

### Fixed — the clean epoch release was never issued

- **`is_fenced()` meant two opposite things** — "a rival deposed us" and
  "we closed the barrier on our own way out" — and the clean shutdown
  sets it one line before calling `release()`. So `store.epoch_release()`
  was never called on any clean shutdown, ever; the cell stayed HELD and
  the next hub waited out `heartbeat × lease_misses` instead of claiming
  instantly.

  Deterministic, 3/3 on hubs 10–12s old, proved by the ABSENCE of the
  inner branch's own warning, and corroborated twice: B7 measured **79s
  versus 13s** for an identity-changing wake, and E5 saw a 79s takeover
  after a demonstrably clean shutdown. That is the whole hibernate-wake
  path, since hibernate destroys the PVC and with it the `serverId`.

  The guard now records WHY it was fenced. Note the CAS was always the
  real arbiter — a deposed hub's release 412s because the token rotates
  on every renewal — so the pre-check bought nothing and cost every
  hibernate-wake about 66 seconds.

### Fixed — expired client leases were only reaped by inbound traffic

- **`nfs.activeLeases` is a raw `leases.len()`, and the only production
  caller of `cleanup_expired()` was the top of every COMPOUND.** That is
  inbound-driven, so it cannot reap the one case that matters: a volume
  whose only client is gone or partitioned sends nothing.

  The periodic sweep looked like it covered this and did not — it retires
  LAYOUT GRANT ROWS, and standalone (flint-lite) turns layouts off
  entirely, so in that posture it swept nothing at all.

  Measured under a real `iptables` partition: `activeLeases` held at **1
  for 770 seconds** against a 90-second lease, then dropped to 0 the
  instant a single file-API compound arrived, and the share suspended
  208s later — so the ladder was working throughout and the stale lease
  was the only thing pinning it. With `suspendWithSessions: false` a
  partitioned agent fleet would pin its share awake permanently.

  The courtesy-release pass now runs on both the COMPOUND path and each
  sweep tick, so the dead client's locks, sessions, stateids and
  delegations are released too rather than just the gauge being fixed.

### Changed — downloads above 8 MiB stream instead of buffering

- **Buffering every response body bounded hub memory by the DOWNLOAD CAP,
  which defaults to 5 GiB.** A 512 MiB request took `VmHWM` from 30 MB to
  541 MiB, and the same GET under a 256Mi limit was OOM-killed — which
  takes the NFS export down with it, because one process serves both.
  Hubs are 1:1 with projects, so that was a per-project cost across the
  fleet.

  Downloads now split at `streamThresholdBytes` (8 MiB). At or below it
  nothing changes: the body is buffered whole, so the status code and
  `Content-Length` are decided with every byte in hand and a shrink or
  rename-over mid-read is refused as a clean 409 before any byte ships.
  Above it the body streams and memory is O(chunk) whatever the file size.

  What streaming gives up is stated plainly: the status line ships before
  the last byte is read, so a mid-read change ends the stream with an
  error — a reset connection, never a clean short body under 200. The
  terminal re-stat that catches a rename-over still runs.

  Both the cap and the threshold are checked against the RANGE, not the
  file, so a small `Range` of a huge file keeps the buffered path.

### Known, and NOT fixed in this release

- **The write reserve is defeated by write speed.** A single 600 MiB PUT
  onto a volume with 539 MiB free returned `201 Created` and drove `df`
  to **0 bytes available** — the entire reserve consumed, with
  `nospcWriteRefusals` still 0. `admit_bytes` reads a gauge refreshed
  every 2 seconds and has no accounting for bytes already admitted but
  not yet landed; `admit_warm`, twenty lines below in the same file,
  already implements exactly that accounting and documents why. The hub
  self-healed here via publish-and-evict, but the reserve exists to keep
  the state database writable and it did not hold.

## [1.30.0] - 2026-08-19

### Fixed — an exclusion that could take a file out of service permanently

- **`tier::gate::exclude` had no deadline, and gave up nothing when it
  hung.** It set `excluded = true` and THEN waited for `in_flight` to
  reach zero with no bound, so one write syscall stuck in D-state left
  the file refusing every entrant — with no `ExclusionGuard` yet in
  existence for a `Drop` to clear. The gate is a process-local map with
  no release path and no operator surface, so the state had exactly one
  remedy: restart the hub. F33's watchdog does not cover it either —
  that probes the backing store, and this wedge happens on a healthy
  disk. Reachable today through eviction and hydration, the only two
  callers.

  `exclude` now takes a deadline (30s, generous because it waits out
  in-flight syscalls on ONE file) and returns `Option`. **There is
  deliberately no unbounded form left.** On the give-up path it clears
  the flag and notifies before returning, which is the part that
  matters and is mutation-verified: leaving the flag standing
  reproduces the original wedge exactly, and the test says so. Both
  callers already had the vocabulary for "not now" — eviction refuses
  `Busy` and retries next tick, hydration falls into its existing
  backoff — so an unrecoverable wedge becomes an ordinary retry.
- A `gate_exclude_timeouts` meter counter, bumped inside the gate so no
  future caller can forget it. The gate is otherwise uninstrumented;
  this is the only signal that a file's writes are not draining, and
  without it the caller's retry hides the symptom entirely.

### Added — conditional requests on the hub's file API

- **Every object now carries an `ETag`**, on downloads, on upload
  responses, and on every listing entry. It renders the fattr4 CHANGE
  attribute together with the fileid, which makes it the SAME validator
  a mounted client uses to order its cache: an entity-tag held by a UI
  and a change value held by a mounted process name one version of one
  file rather than two schemes that happen to agree. Listings carry it
  so a caller can browse a directory and write conditionally afterwards
  without re-reading each file.
- **`If-Match` on PUT / DELETE / move becomes a VERIFY (RFC 5661
  §18.30) inside the same compound as the RENAME or REMOVE it guards.**
  A compound stops at its first error, so a file that changed under the
  caller is never replaced: 412, and the write is refused whole. Before
  this, two PUTs to one path both answered 201 and one was discarded
  with nothing anywhere recording it — the lost-update half of the bug
  whose interleaving half was fixed in 1.29.0 by giving each upload its
  own temp name. A refused upload takes its temp with it.
- **`If-None-Match: *` is create-if-absent; `If-None-Match` on a GET
  revalidates to 304.** On an evicted file that 304 is the difference
  between a header and real, billed S3 egress, because revalidation
  answers before hydration is triggered.
- Entity-tags this server did not mint are refused with 400 rather than
  412, so a caller with a bug learns it instead of retrying forever.
  Weak validators are refused on writes (`If-Match` is defined on strong
  comparison); a multi-tag `If-Match` is refused rather than reduced to
  its first element, which would report a check that never ran.

  **What this is NOT is a lock, and the docs say so where a caller will
  read it.** An NFS compound is explicitly not atomic, so a writer on
  the mount can still land between the VERIFY and the mutation. This
  closes the lost update between API callers — two browser tabs, a
  retried upload, two agents on one project — which is the race this
  surface actually runs into. It is not a degraded imitation of an
  object store's compare-and-swap: it is exactly the strength of NFS's
  own optimistic concurrency control, which the HTTP door was alone in
  not exposing. `If-None-Match: *` is weaker still — NFS has no
  operation that fails a compound BECAUSE a name resolved, so it is a
  stat with a race window, documented as such.

  **A concurrency drill measures the guarantee rather than asserting
  it, and it rewrote what the docs claim — three times.** Eight writers
  doing read-modify-write against one file, 200 appends. The
  unconditional control loses 168-174 every time. `If-Match` loses
  **32-66 on an idle machine and 90-102 under CPU load**: a benefit
  ranging from 5x down to under 2x, and a residual from 16% to 51%.

  The first figure published here came from a SINGLE sample and read
  "5x, 16% residual". Repeating it gave a range. Repeating it under load
  gave a much worse one, and explained the spread: a COMPOUND is not
  atomic, and **CPU contention widens the server-internal VERIFY→RENAME
  gap by descheduling a task inside it** — so the guard is weakest
  exactly when concurrent writers are most likely, which is the opposite
  of the comforting assumption. The front-door contract now says to size
  expectations from the loaded number.

  The test asserts DIRECTION only. Every fixed ratio tried — 3x, then
  2x — was flaky at about 1 in 5, because the benefit is not a constant;
  encoding one load's ratio as a threshold is a lie the suite tells
  intermittently. An earlier version of the leg
  asserted zero loss and failed; the leg was wrong, not the code, and
  the front-door contract now publishes the measured residual so nobody
  builds a multi-user editor on top of a safety net believing it is a
  serialiser. The drill's leg 1 is an anti-vacuity control that proves
  the storm actually races before leg 2 claims credit for surviving it.

  Two defects the drill found on the way, both real and both fixed
  here: a download's `ETag` came from the opening stat while its bytes
  came from per-chunk LOOKUPs, so a rename-over mid-read shipped one
  file's bytes under another file's validator (now re-stat'd and 409'd);
  and `Stale`/`FhExpired` answered `500`, which under concurrent writers
  is ordinary and retryable — a conditioned mutation now answers `412`
  like any other "the object you named is not the object here", and a
  racing read answers `409`, so callers meet one contract instead of an
  unexplained server error the first time two tabs save at once.

  Two things the drill ruled OUT, recorded so they are not re-chased:
  making the change counter a global sequence fixes nothing (tried
  against the failing leg, no effect, reverted), and moving the VERIFY
  adjacent to the RENAME with LOOKUPP does not work at all — the
  filehandle it yields is not one RENAME accepts, and every conditional
  write answered STALE.

  Nineteen tests, four of which drive the fs layer directly rather than
  HTTP. That is deliberate: the upload handler pre-checks a precondition
  with a stat before writing a temp, and that shortcut alone satisfies
  every HTTP-level test — delete the VERIFY from the compound and they
  all still pass while the guarantee is gone. Verified by mutation:
  removing the VERIFY fails 4 tests, and dropping just the fileid half
  of the tag (which is what catches a rename-over onto a fresh inode)
  fails 1.
- **The front-door contract now states what a caller owes**
  (`docs/flint-lite-operator.md`): send `If-Match` and handle 412 by
  re-reading, treat `If-None-Match: *` as a check rather than a lock,
  and know that a 304 is still activity — a UI revalidating on a timer
  pins a project awake exactly as re-downloading does. Written where
  ensure-live and keepalive already live, because the front door is a
  web service handling untrusted input and two tabs on one project is
  its ordinary case, not its exotic one.
- **Fixed a pre-existing ~1-in-5 flake across the tier test rigs.** The
  capture pending queue is process-global and `durable::drain_pending`
  takes ALL of it into whichever backend called, so two tier tests
  running in parallel stole each other's marks and the loser found its
  own files missing from its own backend. The tests already guarded the
  case where another test's notes land here; the inverse was never
  covered. All four rigs (import, hydrate, evict, flush) now hold a
  `capture::test_exclusive()` guard from construction to drop, covering
  queue AND drain as one critical section — serialising only the drain
  leaves the theft window open. Costs ~13s of suite time and buys a gate
  that means something: 0 occurrences in 34 runs, against roughly 1 in 5
  before.
- The delete drill is `#[cfg(target_os = "linux")]`. On macOS racing
  `remove_file` against one path reports success to several callers at
  once — harness, not server; the identical drill on Linux yields
  exactly one winner every round, verified before gating it. The
  macOS-suite-is-not-the-suite rule earned its keep twice in this
  change.
- Documented one behaviour that is otherwise unexplainable from the
  outside: on a TIERED share an entity-tag changes when a file is
  evicted or hydrated, because both rewrite the local inode and the tag
  derives from its change attribute. It fails closed (re-read, never a
  lost edit) and a tier-less share is exact, but a caller seeing a lone
  412 must not read it as evidence of a concurrent editor.

## [1.29.0] - 2026-08-19

### Fixed — four defects found by designing the cluster drill

- **Both charts' default-deny NetworkPolicies FELL OPEN.** Per the
  NetworkPolicy API an ingress rule whose `from` is empty or missing
  matches ALL sources; both charts rendered `from: []` under a comment
  claiming it "admits NOTHING". Enabling the policy without setting a
  client list published 2049 and the read-write file API to the entire
  cluster, while reading as protection. Rules are now omitted entirely
  unless they have a peer — a policy with no rules denies everything.
- **Every operator Event was refused 403, silently.** kube-runtime's
  Recorder publishes `events.k8s.io/v1` and the chart granted only the
  core group. Publishing is best-effort by design so a lost event never
  fails a reconcile, which is exactly why nothing noticed:
  `AdoptionBlocked`, `ReclaimRefused`, `IdleSuspended`, `Woken`,
  `CredentialsMissing` — all dropped, while the docs told operators to
  read them.
- **Two concurrent PUTs to one path corrupted the file.** The upload
  temp name was keyed on the process id, and one hub serves every
  request for a share — so both uploads opened the same temp file,
  interleaved their bytes into it, and each renamed it over the target.
  Both callers were told 201 Created. A UI retrying a slow upload is
  enough to trigger it.
- **`suspendWithSessions` was documented inverted.** The CRD said to
  "set it" to refuse suspending under a live mount; the code arms the
  guard on `false`. Anyone following the documentation got a share that
  suspends out from under its own mount.


### Added

- **The front-door contract** (`docs/flint-lite-operator.md`): the
  deterministic share name (`fs-<project-id>`) that makes ensure-live
  idempotent across racing replicas, the `chert.us/project-id` label
  and its PROJECT printer column, the ensure-live loop, mandatory
  keepalive, and how to answer "why did my project suspend". Plus a
  narrow `frontDoor` ClusterRole (off by default): get/list/watch/
  create/patch on flintshares and nothing else — no delete, no pods,
  no PVCs, no secrets — with read-only access to the operator's
  leader-election Lease, which is the only way to tell "this share is
  waking" from "no operator is running, so nothing will ever happen".
- **`status.serverId`**, mirrored from the hub's `/status` (which now
  publishes `serverId` and `podName`). A change means the share came
  back on a fresh PVC and every stateid a client still holds is stale:
  a front door that records it with a brokered mount can tell "the hub
  bounced, carry on" from "remount before trusting that handle". The
  operator carries the last known value forward rather than blanking it
  on a pass that made no round trip.
- **`chert.us/wake-intent: warm|cold` is now wired**, having been a
  parsed-but-inert annotation a front door might reasonably have
  started writing. It overrides `hydrateWarmAfterImport` for exactly
  one boot — what the front door knows and the operator cannot, namely
  whether a person is about to open the project — and is consumed once
  the hub is serving. An unrecognised value reads as no intent rather
  than as `cold`; guessing "do less" on a typo would surface only as a
  slow project.
- **Operator high availability**: two replicas by default, one a warm
  standby, with a PodDisruptionBudget and soft anti-affinity. Wake is
  level-triggered and only the operator writes the annotation the
  render keys on, so while no operator reconciles, no share in the
  cluster can be woken — the whole wake path used to ride on one pod's
  node. Leader election already gated the reconcile itself, so the
  standby costs a lease renewal. `replicas: 1` restores the old
  posture; the chart refuses `replicas > 1` with leader election off.
- **`spec.service.advertiseAddress` on FlintShare**: what `status.address`
  should say, verbatim. It is the only way a consumer OUTSIDE the
  cluster gets a mountable address — every derived value is
  in-cluster-only except a LoadBalancer's ingress, and NodePort is the
  quiet trap: it returns the `.svc` DNS name rather than a node
  address, so a foreign client reads the address, mounts it, and fails
  on a name it cannot resolve. Admission requires an explicit port,
  because an NFS client handed a bare host silently uses 2049. IPv6
  must be bracketed. The Service itself is untouched — this changes
  only what is advertised, so the in-cluster path is unchanged.
- **`networkPolicy` in the flint-lite and flint-lite-operator charts**:
  default-deny ingress with explicit holes. Off by default (an empty
  client list breaks every mount, and only the cluster's operator knows
  its node CIDRs) and it needs a CNI that enforces NetworkPolicy. The
  operator chart also renders a no-ingress policy for the operator pod,
  which serves nothing at all. Note the shape: a NetworkPolicy covers
  only its own namespace, so `networkPolicy.hubNamespaces` lists the
  namespaces to render a hub policy into — a share in an unlisted
  namespace is unprotected and nothing detects that.

### Changed

- **A boot-only setting no longer forces a rollout.** The pod-template
  checksum is computed over the config with `hydrateWarmAfterImport`
  stripped: the warm fill runs during import and never again, so a hub
  that is already serving gains nothing from a restart that changes it
  — while the restart costs mounted clients a ~90s grace window. Without
  this, consuming `wake-intent` would roll the hub minutes after it
  woke, hanging the agent the wake was for.
- **The Secret watch is label-selected** (`chert.us/credentials`).
  Unselected, it held every Secret in the cluster in the operator's
  memory — service-account tokens, other tenants' credentials — to
  notice changes in the few a FlintShare names. A missing label is not
  a correctness problem: the checksum comes from a direct `get` during
  reconcile, so a rotation still rolls the hub on the next periodic
  pass. Shares report `CredentialsWatched: false` and say how to fix it.
- **`poll_hub` no longer lists every pod in the namespace.** It runs
  once per poll per share, so at fleet scale it is the dominant
  API-server term, and in a namespace holding many shares each poll
  paged in every other share's pods to discard them client-side. The
  selector comes from the Deployment rather than from the share's
  labels, because those differ for an adopted share and a Deployment's
  selector is immutable.
- **The MDS gRPC control plane no longer starts in `mode: standalone`.**
  It binds `0.0.0.0:50051`, carries `DeleteVolume`, and is
  unauthenticated unless `FLINT_PNFS_CONTROL_TOKEN` is set — which
  nothing in the flint-lite charts sets. A standalone hub refuses
  `dataServers` by construction and has no CSI driver in front of it,
  so every verb on that port was either meaningless or destructive,
  reachable by any pod in the cluster, against a hub whose PVC may be
  the only copy of the data. pNFS deployments are unaffected: they keep
  the port, the bearer token, and the CSI chart's `flint-pnfs-control`
  NetworkPolicy.

### Fixed

- **`reclaim: Delete` no longer destroys an adopted PVC.**
  `spec.existingClaim` and `reclaim: Delete` are both the user's words
  and they contradict each other. Hibernation has always resolved that
  by refusing — "the operator did not create it and does not get to
  delete it" — while CR deletion resolved it by deleting and logging
  `adopted=true`, so one field meant two different things depending on
  the route that reached it. Adoption is the documented migration path
  off a helm release, so an adopted claim is evidence something else
  still believes it owns that data; refusing leaks a PVC, which is
  visible and removable with one command. Now refused in both paths,
  with a `ReclaimRefused` event.
- **A request stamp from the far future no longer pins a share awake.**
  `chert.us/requested-at` is clamped to "wanted right now" when it is
  in the future, which is the right reading of ordinary clock skew and
  the wrong one without a ceiling: a front door running an hour fast
  held the share up for an hour, reporting `requested 0s ago` every
  pass — indistinguishable from real demand, and logged nowhere. Past
  one full `suspendAfterSecs` ahead the stamp is discarded rather than
  clamped, the hub's own activity clock decides, and the operator warns
  and emits an `ImplausibleRequest` event naming the skew. Waking is
  unaffected: it is presence-only, so a skewed stamp still wakes a
  suspended share, which is the safe direction.
- **The crypto-provider guard now walks `[[bin]]`** instead of naming
  two files by hand. It covered 2 of the 9 binaries that build, so the
  guard's own rationale — "what shipped broken was a binary that never
  called it" — still described a gap it had: the next binary to grow a
  kube client would have shipped unchecked, which is exactly the shape
  of the 1.26.0/1.27.0 startup panic.

## [1.28.0] - 2026-08-19

### Added — the idle lifecycle and the hub's HTTP surface

- **`/status` on the hub**: the operator's answer to questions
  Kubernetes cannot answer — is anyone using this share, are its bytes
  safely in the bucket, is a DR import halfway through. Served on its
  own port, ClusterIP-only and deliberately NOT on the consumer-facing
  Service, which carries NFS and may be a LoadBalancer. **`rpoClean` is
  `null` for a share with no bucket, never `true`**: absence means "the
  question does not apply", and a controller reading it as "clean"
  would delete the only copy of the data.
- **The file API** (`spec.monitoring.fileApi`): browse and edit a
  share over HTTP without mounting it, dispatching NFS compounds
  in-process so the bytes are the same filesystem a kernel client sees.
  Bearer-token auth against an existing Secret; there is no
  token-optional mode for a surface that can rewrite every file. A
  symlink is returned as DATA and never followed (409), and writes
  during the post-restart grace window answer 503 with a `Retry-After`
  rather than failing opaquely.
- **The idle ladder** (`spec.idle`): a quiet share scales to zero and
  keeps its PVC (`IdleSuspended`); stamping `chert.us/requested-at`
  brings it back. Hibernation additionally deletes the PVC and is
  refused at admission without `spec.bucket`, because that PVC is then
  the only copy. **Suspending requires two independent signals to
  agree** — the front door's heartbeat is stale AND the hub's own
  activity clock says idle — and an unreachable hub is never treated as
  idle. State lives in CR *annotations*, not spec and not status: the
  reconciler is level-triggered and re-renders `replicas` on every
  pass, so a suspend recorded where the renderer does not read it is
  undone seconds later, forever.
- **`spec.idle.suspendWithSessions`**: refuse to suspend while a client
  still holds a lease. Off by default — an idle NFSv4 mount renews its
  lease forever, so defaulting it on would pin every mounted share
  awake, which is the state the ladder exists to end.
- **Manifest-first tier import**: a bucket whose manifest the hub
  cannot read is now REFUSED rather than imported as a flattened tree
  and then published back over the real one. Only the manifest carries
  directories, symlinks, modes and owners.

### Added — the flint-lite operator (`FlintShare`)

- **`chert.us/v1alpha1 FlintShare`** and `flint-lite-operator`: one
  custom resource per volume instead of one helm release per volume.
  The operator renders the same four objects the lite chart renders
  (ConfigMap, RWO PVC, Service, single-replica Recreate Deployment) —
  a golden test compares its output against a fixture regenerated from
  `helm template` by `scripts/check-render-parity.sh`, and the fixture
  records the chart's hash so a chart edit without a regenerated
  fixture fails the suite. New chart: `flint-lite-operator-chart`
  (RBAC, leader-election Lease, CRD bootstrap). Docs:
  `docs/flint-lite-operator.md`.
- **The tier knobs are schema**: `spec.settings` is an all-`Option`
  mirror of the server's `TierKnobs` with ZERO schema defaults, so a
  typo is refused at admission while an unset knob still takes the
  SERVER's default (a CRD default would be materialized into stored
  objects at admission — stale-values-by-construction). Parity with
  the server type is unit-tested in both directions. `TierConfig` is
  split into identity + `TierKnobs` (`#[serde(flatten)]`; the on-disk
  `tier:` block is unchanged).
- **Fleet uniqueness**, enforced by the controller because CEL cannot
  see other objects: at most one share per `(endpoint, bucket, prefix
  subtree)` across all namespaces, oldest wins, losers carry a
  `Conflict` condition and are scaled to zero. Unarbitrated duplicates
  are not merely wasteful — when one hub dies for a lease window the
  other TAKES OVER the prefix and serves that data at its own address.
- **Chart→CR adoption** (`spec.existingClaim`): the operator adopts an
  existing release's objects in place and holds `AdoptionBlocked`
  while any foreign pod still mounts the claim. RWO is node-granular,
  so a second Deployment can share a node with the first — two sqlite
  writers on one `state.db`, which the epoch provably cannot fence
  (both pods self-recognize as the holder).
- **The operator owns its CRD**: it server-side-applies its compiled-in
  copy at startup, guarded by a `chert.us/crd-schema-version`
  annotation so an old operator cannot stomp a newer schema. Helm never
  upgrades `crds/`, and a frozen structural schema silently prunes
  every knob added later.

### Fixed

- **⚠ The NFS server followed symlinks — a shipped credential-theft
  hole.** `ln -s /data/state/state.db s && cat s` through any mount
  read the hub's entire state database, and the IRSA token was equally
  reachable; the export root (`/data/exports`) and the server's own
  state (`/data/state`) are siblings, so a link out of the export
  landed on both. `CREATE` also truncated the link TARGET. Path
  resolution is now beneath the export root, and a symlink is data.
- **SEEK lied about evicted stubs**, so `cp --sparse` and `tar` copied
  a cold tiered file as zeros and reported success — wrong data, not an
  error.
- **A created-never-written file was invisible to the tier.** `touch
  .gitkeep` produced no capture note and no dirty row, so `rpoClean`
  read true with the file existing nowhere but the PVC. Hibernation
  would have deleted it permanently.
- **The idle ladder was evaluated every 300s regardless of
  `suspendAfterSecs`.** `Decision::Hold` fell through to the settled
  requeue, so a share set to suspend after 20s was recorded `Held` at
  "activity 0s ago" and not looked at again for five minutes. The
  decision function was pure and correct throughout — the defect was
  entirely in when it got called, which is why the unit suite could not
  see it and a real API server found it immediately. Each rung is now
  re-checked on its own knob, floored so a small threshold cannot
  become a poll per second and capped so arming the ladder never costs
  more than leaving it off.
- **Hibernation could act on another pod's answer.** `hibernatable()`
  authorised the PVC delete on `rpoClean` alone while `epoch.held` was
  parsed and unused. `rpoClean` describes A volume; it is an answer
  about THIS pod's volume only if this pod holds the tier epoch, and
  self-recognition is gated on the state directory's occupancy lock, so
  a second live process on the same PVC genuinely does not hold it.
- **`spec.idle.suspendWithSessions` was a knob nothing honoured**: the
  CRD advertised it and the decision implemented it, but the reconciler
  passed a hardcoded `None` for the session signal, so it could never
  fire. The hub had published `nfs.activeLeases` all along.
- **The foreign-key sweep moved out from behind the NFS listener**, so
  a large bucket no longer holds the export closed while it folds
  keys in. The eviction marker is now placed BEFORE the name is linked
  — a stub with rows but no marker reads as a 0-byte file, and its
  first small write publishes over the real S3 object under an
  `If-Match` that succeeds — placement is no-replace so a client's
  fresh bytes cannot be swapped for a stub, and the marker cycle is not
  bumped, which would storm every concurrent reader on a large bucket.
- **⚠ The CSI driver could not start in 1.26.0 or 1.27.0.**
  `csi-driver` panics on its second statement —
  `Client::try_default()` — with "Could not automatically determine the
  process-level CryptoProvider from Rustls crate features". rustls 0.23
  picks its provider from its own crate features and refuses to guess
  when several are enabled; until 1.26.0 only `ring` was in the tree,
  and the AWS SDK added for the S3 tier brought `aws-lc-rs` alongside
  it. Every binary that builds a kube client is affected (the driver in
  all modes, and the dashboard backend it hosts). **NOT affected**: the
  hub (`flint-pnfs-mds`) — it does not use kube, and the S3 tier passes
  its provider to the SDK explicitly rather than through the process
  default, which is why every tier drill and the real-S3 gate stayed
  green while this was broken. Fixed by installing the provider
  explicitly at the top of `main` (`install_crypto_provider()`), with a
  test that reads each binary's source and fails if one builds a kube
  client without installing it first.
- **flint-lite chart: a settings change now reaches the running hub.**
  The pod template gained a `checksum/config` annotation, so
  `helm upgrade` with changed `tier.settings` (or `logLevel`, or the
  prefix) rolls the Deployment. Before this, the ConfigMap updated and
  nothing else happened — the server parses `--config` once at boot and
  has no reload path, so `kubectl get cm` showed the new value while
  the hub kept the old one indefinitely.

## [1.27.0] - 2026-08-18

### Added — the cold-read release (flint-lite S3 tier, agent workloads)

- **Cold-read fan-out**: restores fetch up to
  `tier.settings.hydrateFetchParallel` (default 6) ranged GETs
  concurrently. One S3 stream is ~80–200 MB/s — the L4 gate measured
  whole-file hydration at 72.5 s/GiB sequential; the fan-out divides
  it (~10–13 s/GiB expected at 6 streams). Completions are consumed in
  offset order, so the stream CRC, sequential writes, and every
  crash/retry/adopt contract are byte-identical to the sequential
  posture. Peak restore buffering ≈ `hydrateConcurrency ×
  hydrateFetchParallel × 8 MiB`.
- **Warm fill** (`tier.settings.hydrateWarmAfterImport`, default off;
  `hydrateWarmConcurrency`, default 16): after an import that ran (DR
  reinstall, bucket adopt — the all-stubs world), the hub bulk-restores
  the tree smallest-first instead of paying one round-trip per first
  touch — the fix for single-threaded tools (`grep -r`, builds)
  sweeping a cold tree. Runs on a dedicated pool (demand hydrations
  never queue behind it; a client read of a mid-fill file absorbs into
  its restore), stops short of the eviction watermark rather than
  fight it, survives hub restarts via a durable pending note, and
  reports one `tier warm fill done` summary line. Lite chart **0.2.0**
  (the settings schema gained the two knobs).

### Fixed

- **Hydration-completion storms no longer disturb readers**: the
  eviction-marker cycle guard's evidence is now insert-only —
  restore completions stop bumping the global counter that READ and
  COPY/CLONE windows re-verify against. Before this, any hydration
  burst (and the warm fill by construction) could spuriously DELAY
  reads of fully-present files and livelock a long server-side COPY.
  Machine-certified before the code moved: the FlintTierMarker strict
  run holds with clear-bumps off, and a new `InsertBlind` mutation
  pins the insert bump as load-bearing (the formal gate is now 173
  runs, also picking up FlintTierSession, the multi-volume two-level
  lease modeled ahead of its implementation).

Validation for this cycle: the FlintTierMarker tranche 7/7, macOS
1514/1514 + Linux 1521/1521 suites, the 4-phase MinIO tier drill (the
new warm leg restores 36/36 stubs with zero client reads against a
size-equality oracle), the 44-leg chaos drill, and both kind e2es.

## [1.26.0] - 2026-08-18

### Added — flint-lite: the standalone POSIX hub, with an S3 cold tier

**New chart `flint-lite` (0.1.0)** — one pod on any CSI driver's RWO PVC
serving full NFSv4.2 (enforced byte-range locks, close-to-open coherence,
atomic rename) with the pNFS machinery off. Four objects, no CSI stack, no
SPDK, no CRDs; consumers mount plain NFS with nothing installed
(`docs/flint-lite.md` has the recipes). The hub image (`flint-pnfs:1.26.0`)
is the first published tag carrying `mode: standalone`.

**S3 cold tier** (`tier.*` in the lite chart): every mutation captured
durably pre-ack; closed generations publish to a bucket prefix on a flush
cadence (RPO = the flush floor, default 60s, with a DR manifest at every
barrier); cold files evict at a disk watermark and hydrate back on first
touch (readers park on NFS4ERR_DELAY); NOSPC-before-EIO space model with
auto-released ballast; a volume-epoch claim fences a second hub on the same
prefix BEFORE the listener binds. Disaster recovery is reinstalling over
the same bucket+prefix: the namespace imports as evicted stubs and content
hydrates on demand. v1 scope is the standalone posture only — a pNFS MDS
refuses the tier at boot.

Validation shipped with it: a MinIO e2e (18 legs), a 12-phase chaos drill
(44 legs, split-brain/outage/kill-9/degraded-network), a scale drill, two
kind e2es, an L4 run against real S3 from a real cluster, and two new TLA+
modules (FlintTierEpoch, FlintTierMarker; the formal gate is now 165 runs).
Eight bugs were found and fixed by that battery before release — six by
drills, two by the models.

### Fixed

- **First-operation failures on flint NFS mounts**: sqlite's first
  transaction and git's first commit had never worked on any flint mount
  (silent-zeros read of a just-created file; fixed alongside three sibling
  server bugs). Affects every NFS consumer, not just lite.

### Changed

- **Multi-arch images restored**: `flint-driver:1.26.0` and
  `flint-pnfs:1.26.0` publish linux/amd64 + linux/arm64 (1.25.x were
  amd64-only). `spdk-tgt` stays at 1.6.1 (unchanged, amd64).
- The lite profile flag inside `flint-csi-driver-chart` (never published)
  is replaced by the standalone `flint-lite` chart; setting
  `lite.enabled=true` now refuses at render with a pointer. Rationale: a
  Helm chart installs its `crds/` unconditionally, so the profile silently
  planted the VolumeSnapshot CRDs a hub never uses.

## [1.25.2] - 2026-08-15

### Fixed — DATA LOSS: a shrinking `ftruncate` could destroy the whole file

**Affects 1.25.0 and 1.25.1. Upgrade if you use `layout: pnfs-block`.**

Truncating a block-class file to a non-zero size freed the extent containing
the truncation point — including the prefix the syscall promises to keep. The
client then read zeros, and nothing errored anywhere.

It was the common case rather than an edge: extents are merged, so a
sequentially-written file is one row (the rig reports one extent row for a
64 MiB file), and truncating such a file to *any* non-zero size dropped all of
it. Reclaim now frees the intersection of the row and the reclaimed range,
re-mapping whatever survives at its original physical bytes.

The truncation point is also rounded **up** to a block boundary before
reclaiming. The allocator is byte-granular and would otherwise release a range
starting mid-block, then hand that block's second half to another file — and
since clients do whole-block I/O, the next read-modify-write by either file
would clobber the other. The last partial block now stays allocated as slack.

### Fixed — a block-layout PVC could be created ReadWriteMany

CreateVolume's pNFS path returned before the driver's access-mode handling,
while the driver advertises `MULTI_NODE_MULTI_WRITER` fleet-wide, so an RWX
`layout: pnfs-block` claim provisioned cleanly and then behaved as nothing in
particular. Block-class volumes are now refused unless single-node
(`ReadWriteOnce`/`ReadWriteOncePod`) and `volumeMode: Filesystem`.

A block-layout client writes raw extents granted to it exclusively, so a second
node's grant excludes the first rather than sharing with it. `ReadWriteOnce` is
not a simplification here — it is what the layout means. Use `layout: pnfs` for
a shared filesystem.

### Fixed — a composer death left its volumes undeletable

The controller resolved every volume to a single MDS shard by hash, and that
shard is exactly the one a composer death removes (shard and target share a
node). The volume was therefore not only unservable but impossible to delete,
leaving the lvol, the PVC and the namespace finalizer stuck indefinitely.

ControllerPublish, ControllerUnpublish and DeleteVolume now ask the owning shard
first and then its siblings. ControllerPublish is the one that changes an
outcome: it is what a restarting pod calls, so a consumer can now recover onto
the surviving copy while the composer's node is still gone.

ControllerExpand deliberately does **not** fan out — the arena ceiling lives in
the owning shard's state, and a sibling raising a ceiling it cannot back would
report space the array cannot serve.

### Unchanged

`replicas: 2` remains durability, not availability. A composer death still
leaves the client's NFS mount pointed at a shard that cannot be rescheduled;
what changes here is that the volume can now be re-attached by a restarting
consumer and, failing that, deleted.

## [1.25.1] - 2026-08-15

### Fixed

**A surviving MDS shard can now answer a re-attach for a volume it never
created.** This is the last of the four stacked defects that stranded a client
after a composer death. The class gate read `volume_is_scsi`, which is the
geometry cache, which is seeded from that shard's own sqlite — and shards share
nothing. Every survivor therefore answered "not block-class" for a volume that
is emphatically block-class, and the sibling fan-out shipped in 1.25.0 bought
nothing. The gate now falls through to the composition witness when its local
rows come up empty: **a seat is proof of class**, since seats are minted only by
the block composition path, so no files-class volume can have one. The consult
runs only on the empty-rows path, so a volume's own shard still answers with no
witness round trip and still works through a witness outage. An unreadable
witness fails **closed**, with a message distinct from the class refusal.

Proven on hardware (4-node cluster, real NVMe/TCP): with the composer's whole
node powered off, a sibling answered the re-attach 39s later and the client
moved to the survivor's target at +119s, with zero "not block-class" refusals.
The intervening lease refusal is the eviction horizon working as designed.

**A failed `nvme connect` now says why, when the cause is a host-identity
clash.** The kernel keys a host on the pair (hostnqn, hostid) and refuses a
second id under the same NQN, but nvme-cli surfaces only `Failed to write to
/dev/nvme-fabrics: Invalid argument` — the reason goes to the kernel ring
buffer. Both connect paths now name the incumbent controller, its hostid, what
it is serving, and the remedy. Flint does **not** disconnect the incumbent: that
controller is carrying a live volume.

### Upgrade note — drain NVMe-oF connections when upgrading from < 1.25.0

Rolling csi-node is not sufficient. Connections established by a pre-1.25.0
driver were made without `-I`, so nvme-cli invented a hostid for them, and a
kernel controller is **not** torn down when its pod is replaced. A node with such
a connection still alive will refuse *every* later NVMe-oF attach — including
volumes unrelated to the incumbent — until it is drained. Restart the pod
consuming that volume so it is unstaged and re-staged by the new driver. From
1.25.1 the failure explains itself; before it, it is a bare `Invalid argument`.

### Unchanged, and still true

`replicas: 2` remains durability, not availability, and this release does not
change that. The redirect restores the **NVMe data session** to the survivor.
The client's NFS metadata mount still targets the dead shard's ClusterIP, whose
Service has no schedulable endpoint (the shard is node-pinned with RWO state),
so a consumer still stalls after a composer death. Measured, not inferred.

### Testing

`make test-pnfs-replica-mdsdeath-rig` adds the node-death shape to the
replication rig. The existing rig killed only the target and left the composer's
MDS running, so re-attaches were always answered by the shard that owned the
volume's geometry — the one shard for which this class of defect is invisible.
It was green throughout the campaign whose client was stranded.

## [1.25.0] - 2026-08-15

### The pNFS block tier (RFC 8154/9561 SCSI layout over NVMe) — EXPERIMENTAL

A block-class volume lets the client kernel (>= 6.11, `CONFIG_PNFS_BLOCK`) do
raw NVMe/TCP extent I/O against spdk-tgt with the MDS out of the data path.
Opt-in: `pnfs.blockLayout.enabled`, plus a StorageClass with
`layout: pnfs-block`. `replicas` defaults to 1.

**What `replicas: 2` does and does not do.** It places two copies on two
distinct targets in one zone, mirrors writes, rebuilds a stale leg, and
arbitrates a promotion through a shared witness (one ConfigMap per volume under
resourceVersion CAS). On a composer death the seat moves in ~20s, the eviction
horizon is honoured, the surviving copy holds the acked bytes, and the client
keeps its device node.

**It does NOT keep a client serving through that death, and we do not call it
failover.** The volume's MDS shard is pinned to its target's node (lvols are
node-local) with its state on an RWO PVC, and the client's mount targets that
shard's Service — so a lost node takes the volume's layout class, geometry,
extent rows and LAYOUTGET lane with it. The witness closed arbitration, not
control-plane failover. Treat `replicas: 2` as durability for re-hydratable or
scratch data, not availability.

### Fixed — four P1s, each found only by running the block tier on hardware

**Every I/O silently degraded to MDS proxying under a containerized consumer.**
The kernel resolves a SCSI layout's device by path under `/dev/disk/by-id`, in
the mount namespace of whichever process first triggers the layout — and a
container has no such directory. All three lookups returned `-ENOENT`, the
client logged `pNFS: no device found`, and files were created and stayed
**zero bytes**. csi-node now warms the device from the host namespace at
NodePublish, and the consumer inherits the resolved deviceid.

**Any csi-node restart broke every later NVMe-oF attach on that node.** Both
connect paths passed a stable host NQN with no `-I`, so nvme-cli supplied a
hostid it auto-generated inside the (ephemeral) container. The kernel keys a
host on the pair and refuses a reused NQN under a different id. **This affects
ordinary SPDK volumes, not just the block tier.** The hostid is now derived
from the host NQN.

**A target restart permanently broke a composed volume's export.** SPDK stamps
the bdev's uuid into the reservation's ptpl record and validates it on the next
`add_ns`; `bdev_raid_create` was called with no uuid, so every restart rebuilt
the composition under a new one and the namespace was refused forever — no
namespace, so no listener, so nothing on 4420. The raid uuid is now pinned and
the ptpl path follows the bdev.

**The block redirect lane could not reach any MDS.** csi-node runs
`hostNetwork` without `ClusterFirstWithHostNet`, so no `.svc.cluster.local`
name resolved at all, and it was never given the control-plane token that
`AttachBlockNode` requires. Both fixed in the chart; the lane also now asks
sibling shards, since a composer death takes out the shard the volume was
created on.

### Also

**The composition witness had an untestable failure mode, and it was hiding a
fail-open.** `drop_leg` — a gRPC entry point reached by DeleteVolume's fan-out
— treated an *unreadable* witness the same as a confirmed no-seat and destroyed
a volume the target was actively composing. A per-method outage overlay now
makes that case reachable in tests.

**`replicas: 2` is installable via helm for the first time.** blockExport is
per-node, so its config had to be per-shard (`blockExport.shards[i]`); a single
shared ConfigMap could only ever describe one target.

### Prior release-candidate content (1.25.0-rc1, 2026-08-05)

The theme is data that was wrong rather than absent. Two of the three headline
fixes produced a plausible-looking result instead of an error, which is why
they survived as long as they did.

**Striped reads could return silent zeros (F67).** A lost placement binding
left the MDS unable to say where a stripe lived, and the read path filled the
gap with zeros rather than failing. Nothing upstream could distinguish that
from a genuinely sparse file. Found during the read-variance investigation,
where six separate instrument bugs shared the same shape — "reads zero instead
of erroring".

**The ~5s cold-open stall was ours, not the client's (F69).** A no-create OPEN
left the current filehandle at the parent directory. The client re-OPENed,
received EISDIR, and waited out a 5-second kernel timeout. It had been read as
client-side slowness for the whole campaign. Measured 6.3s -> 0.75s on lima;
**fleet numbers have not been re-measured.**

**A DS could be granted before it could be reached (F68b).** The data server
bound its listener before registering, so a layout could hand a client an
address that was not yet serving.

**The MDS applies fallback I/O to the stripes instead of refusing it (F66)**,
closing the straggler-EIO case where one unreachable DS failed the whole read.

Also in this release:

- **A READ payload reaches the socket without being copied three times.**
  71.3% CPU reduction, measured; the cost was allocator mmap churn.
- **Operator `mountOptions` replace the driver's defaults** instead of racing
  them, so a class carrying rsize/wsize/vers/timeo lands intact while the
  driver's own sec=sys and nconnect survive.
- **pNFS multipath trunking** — GETDEVICEINFO now carries per-DS address lists,
  with `pnfs.server.dataServers.multipathServices` provisioning K extra
  per-pod Services. Kernel-trunk-proven. **The mount must carry `nconnect>=2`
  or the kernel silently refuses every trunk**, and the scaling rematch is
  still pending.
- **`FLINT_DS_ODIRECT`** — opt-in O_DIRECT read path on the DS. **Default off,
  and never measured against a real DS.** The supporting data is one layer up:
  on fast NVMe a fully-cached buffered read measured *slower* than a cold one
  and less than half O_DIRECT, narrowing to ~1.2x at four concurrent streams.
  Treat as experimental.
- **`spdkTarget.hugepages.enabled=false` now works.** It used to drop the
  kubelet reservation without telling SPDK, so EAL init failed; it now also
  emits `--no-huge -s <MB>`. Default unchanged (`true`) because no real cluster
  has booted the `--no-huge` path. Passing `--no-huge` via `extraArgs` is now a
  render-time error, since by hand it sets the flag without dropping the
  reservation.
- Chart `version` and `appVersion` agree again, and **`flint-driver` and
  `flint-pnfs` move in lockstep** — they had drifted to 1.24.1-rc2 and
  1.24.1-rc6, which is exactly the split `values.yaml` warns about, since the
  MDS half of geometry lives in the same crate.

### Known gaps

- No cluster gate has been run on this bundle. Each change was proven on the
  cluster it was written for; none has been regression-tested together.
- Multipath trunking's scaling rematch is outstanding.
- F69's fleet-level numbers are unmeasured.
- `FLINT_DS_ODIRECT` is unmeasured on a DS.

## [1.24.0] - 2026-08-01

The pNFS-as-a-PVC release. The theme is duplication: two of the three
headline bugs are a copy of something that drifted from its original, and
the third is a build that copied the builder's CPU into the product.

**The MDS had its own RPC layer.** Pointing the general 4.1 conformance
suite at the metadata server for the first time scored 160/171, where the
standalone server scores 171/171. The cause was not in the session code:
`pnfs/mds/server.rs` carried ~510 lines copied out of `nfs/server_v4.rs`
in the original pNFS commit, and the copy never received the SEQUENCE
reply-cache fix or the F55 drain gate. Eight months of fixes landed on
one path and silently not the other, and nothing tested the difference
because nobody had ever run the general suite against the MDS. The fork
is deleted rather than patched. **MDS is now 171/171.** The remaining
RECC3 "failure" is not one — grace is 90s from server start, so that test
is only meaningful inside the window; restart the MDS and it passes 4/4.

**Symlink containment dereferenced the leaf.** The export-containment
check called `canonicalize()` on the whole path, which follows a trailing
symlink — so naming a link inside the export that pointed elsewhere was
judged an escape and refused (`NFS4ERR_RESOURCE` on LOOKUP,
`NFS4ERR_IO` on CREATE). It resolves the parent and rejoins the leaf now.
Never MDS-specific; standalone passed only because its export tree had no
such links.

**pNFS volumes are now tunable per PVC.** New StorageClass parameters
`pnfs.chert.us/{stripeSize,stripeWidth,dirGid,dirMode}`, persisted through
the state backend and reloaded at MDS start so placement survives a
restart, plus online expand. Unknown `pnfs.chert.us/*` keys are REFUSED at
provision — a typo used to be indistinguishable from success. The chart
can finally render a pNFS StorageClass at all (`storageclass.yaml`
hardcodes the four SPDK parameters, so `--set
storageClass.parameters.layout=pnfs` rendered nothing).

**`sec=sys` is the load-bearing fix in that wave.** The mount requested no
auth flavour, so it negotiated AUTH_NULL, no uid reached the server, and
every file a pod created landed owned by root — measured uid=0 before,
uid=1000 after. That is why ownership-sensitive workloads would not start
on a pNFS PVC. The mount is also NFSv4.2 now; the server has supported
4.2 since 1.23.0 and only the mount option said otherwise.

**spdk-tgt 1.6.1: the image's minimum CPU was whichever spot node built
it.** SPDK's `configure` defaults `--target-arch` to `native` and the
Dockerfile never set it, so DPDK was compiled for the build machine and
that became a hard requirement of the shipped image — making flint's
hardware floor an accident of scheduling and unreproducible between
builds. On anything older, DPDK aborts with `This system does not support
"VPCLMULQDQ"`, and because spdk-tgt is an *init* container the whole
`flint-csi-node` DaemonSet init-crashloops and the node has no CSI driver
at all. Measured on i3en.xlarge (Skylake-SP): 7/7 nodes down. Now pinned
to `corei7`, DPDK's own generic x86 baseline.

Also fixes two pre-existing breakages that had `cargo test` red on main: a
clippy `approx_constant` on a test fixture, and a doc example calling
`NfsServer::new(config)?` that became async on 2026-07-07.

## [1.23.0] - 2026-08-01

The correctness-under-measurement release. Two independent waves, and the
thing they have in common is that neither set of bugs was visible from
reading the code — every one of them was found by running the thing and
looking at the wire.

**pNFS truncate.** A truncate applied to the MDS stub and then fanned out
to N data servers leaves a window in which the DSes still hold bytes past
the new EOF; the `truncate_dirty` gate exists to make that window
unobservable, and TLC found that it did not. Closing F65 took nine
further defects, every one of which had been invisible because the
server's own logs reported success — the recall was emitted, refused on
the wire, and scored as an ack.

**NFSv4.2.** Two of these are reachable from an ordinary user command.
`cp --reflink=always` emptied the destination before the ioctl that
decides whether a clone is even possible, so on ext4 a failed rebuild
left the client told "CLONE failed" with the data already gone. And a
single `copy_file_range()` never returned: COPY's hardcoded zero write
verifier never matched COMMIT's, so a Linux client read every successful
copy as a server reboot and reissued it — 264,601 times for one 1 MiB
request, the server performing the full copy each time. Alongside those,
4.2 operations turned out not to be gated on the negotiated minor version
at all, which made the pNFS mount's `minorversion=1` a client convention
rather than a server property.

Two judgements made during this release were overturned by measuring
them: the COPY verifier was deferred as cosmetic (it was an infinite
loop), and `space_used` was deferred pending evidence (the evidence was
`tar --sparse` restoring a 24 MiB file as zeros).

**No live upgrade path is provided and none is needed: flint is not
deployed anywhere.** The MDS state database schema was collapsed to a
single version with the migration machinery removed; a database written
by an earlier build is refused at open with an actionable error rather
than migrated.

### Added

- **A TLA+ model of the pNFS truncate gate** (`formal/FlintTruncate.tla`,
  seven configs). It carries two theorems and is explicit that only one
  holds: `Inv_ClearImpliesFlushed` (the gate's own claim — whenever the
  mark is absent, no DS holds content past the MDS size) is proven, and
  `Inv_NoStaleServe` is deliberately NOT listed in the shipped config,
  because one residual still defeats it. The counterexample that started
  the wave was three steps from `Init`.
- **A synthetic NFSv4.1/pNFS client** (`tests/k8s/pnfs-drills/synth_client.py`)
  that holds layouts across events the harness schedules, answers
  `CB_LAYOUTRECALL`, and has a `--deaf` mode that accepts callbacks and
  never replies. A real kernel returns its layout ~80 ms after each I/O,
  so the states this release is about are not reachable with one. It is
  explicitly **not** a conformance oracle — pynfs remains that.
- **An XDR callback decoder for the drills** (`cb-decode.py`). The drill
  previously ran `grep -c CB_` over binary XDR, which is structurally
  always zero, so it passed by not looking.
- **Regression coverage for the pNFS READ/WRITE stub guard**, which had
  shipped since 2026-07-06 with nothing in the tree that would go red if
  it were deleted. Both dispositions are now asserted by exact status;
  the `FailFast` arm is reachable only because the test double overrides
  `fallback_io_disposition` directly rather than inheriting the trait
  default, which cannot produce it.
- **`docs/plans/v42-copy-sparse-hardening.md`** — the conformance and
  measurement work this release does *not* do, with the deciding question
  stated plainly: nobody has established whether a Linux client falls
  back cleanly when COPY returns `NFS4ERR_NOTSUPP`, and the READ_PLUS
  precedent does not transfer because its fallback target is mandatory.

### Fixed

- **`tar --sparse` backed up striped files as nothing.** A pNFS file's
  MDS stub is `set_len`-only, so `blocks()` is 0 while the size is real —
  the metadata signature of a fully sparse file — and `FATTR4_SPACE_USED`
  reported that verbatim. Measured on a real Linux client: `tar --sparse`
  of a 24 MiB striped file produced a **10,240-byte archive** and restored
  a file containing **zero** non-zero bytes, exit status 0; `du` said 0.
  A backup that silently contains nothing. The MDS now reports
  `space_used = size` for pinned files — per file, so a genuinely sparse
  never-layouted file still reports its real allocation. `cp
  --sparse=auto/always` was verified unaffected (it reads the data).
- **COPY livelocked a real Linux client, and the server did the work every
  time.** `wr_writeverf` was a hardcoded zero, commented "sync copy:
  unused". Linux (verified on the wire, kernel 6.8) issues COPY and COMMIT
  in ONE compound and compares COPY's verifier against COMMIT's; zeros
  never match, so the client read every successful copy as a server reboot
  and reissued the identical COPY forever. Measured: one 1 MiB
  `copy_file_range()` produced **264,601 COPY RPCs**, each of which the
  server actually performed, and the syscall never returned. COPY now
  reports the same per-lifetime verifier as WRITE and COMMIT; the same
  operation now takes **2 RPCs** and returns. Every reply was
  individually well-formed and said NFS4_OK — only the *relation* between
  the two verifiers was wrong, which is why no single-operation assertion
  could have caught it.
- **COPY silently dropped the tail of its own arguments.** `COPY4args`
  ends with `ca_source_server<netloc4>` and the decoder stopped before it,
  so the array's length word was read as the next opcode. For the ordinary
  empty case that word is 0 — reserved — so the COMPOUND was truncated to
  one operation plus an OP_ILLEGAL. Worse, a **non-empty** list is an
  inter-server copy request, and it was ignored while a **local** copy was
  performed and reported OK. The array is now consumed arm by arm (an
  unknown `netloc_type4` is BADXDR, since an unknown discriminant means an
  unknown width) and a non-empty list returns `NFS4ERR_NOTSUPP`.
- **COPY's reply contradicted itself.** `cr_synchronous` echoed the
  client's *request* rather than what the server did, while
  `wr_callback_id` was encoded as an empty array — so an async request got
  a reply that simultaneously said "this is asynchronous" and "there is
  nothing to wait for". flint emits no CB_OFFLOAD and dispatches neither
  OFFLOAD_STATUS nor OFFLOAD_CANCEL; there has never been an asynchronous
  copy to describe. It now reports TRUE, and the fsync is unconditional,
  which makes the hardcoded `wr_committed = FILE_SYNC4` true by
  construction instead of true only when the client happened to ask.
- **COPY and CLONE accepted ranges that run off the end of the source**
  (RFC 7862 §15.2.3 and §15.13.3: "MUST fail with NFS4ERR_INVAL"), and
  **COPY accepted a source and destination that were the same file** —
  which is not merely non-conforming but corrupting, since the chunk loop
  is a memcpy where a same-file copy needs a memmove. CLONE's rule is
  deliberately weaker, matching the RFC: same file is legal there unless
  the ranges overlap. Comparison is by `(dev, ino)`, not path, because the
  filehandle layer follows a rename-alias table.
- **COPY and CLONE never advanced the destination's change attribute**,
  the very ordering key `change_counter` exists to protect — its module
  doc names "two extends of a COPY burst" as the disease. Mostly masked by
  the ctime floor, except when a prior bump landed in the same clock tick.
- **SEEK reported success for offsets past the end of the file, and could
  never report EOF.** Linux returns ENXIO for two different questions —
  "past EOF" and "no more content of that type" — and RFC 7862 §15.11.3
  gives them opposite answers: the first MUST be `NFS4ERR_NXIO`, the
  second is OK with `sr_eof` TRUE. Both were collapsed into a success.
  `sr_eof` was additionally hardcoded false, so the RFC's own worked
  example (`SEEK 0 CONTENT_HOLE` on a dense file → `eof=1, offset=size`)
  was answered wrongly. An unknown `sa_what` was treated as HOLE and now
  returns `NFS4ERR_UNION_NOTSUPP`. `NxIo` and `UnionNotsupp` had both been
  declared and never used.
- **ALLOCATE/DEALLOCATE offsets above `i64::MAX` reached `fallocate` as
  negative values** — a wire `u64` cast straight to `off_t`. Rejected
  before the cast now. ENOSPC also maps to `NFS4ERR_NOSPC` rather than
  a generic I/O error.
- **Two dead attribute encoders deleted** (493 lines, zero callers) that
  disagreed with the live encoder on `space_used`. Two encoders answering
  one attribute differently is how the next reader gets it wrong.
- **CLONE destroyed the destination before it knew it could clone.** The
  whole-file path opened the destination `.truncate(true)` *before*
  issuing the FICLONE ioctl and returned an error on failure with the
  file already emptied. `mkfs.ext4` is a shipped option and ext4 has no
  reflink, so on ext4 **every** whole-file CLONE emptied the destination
  and rebuilt it non-atomically; if the rebuild then failed (ENOSPC,
  EACCES) the client was told CLONE had failed and the data was gone.
  Now FICLONERANGE, which writes nothing on failure. Live on the
  standalone RWX mount (`vers=4.2`) — nothing to do with pNFS.
- **CLONE read one request two ways.** `count == 0` meant "replace the
  whole destination file" in its `(0,0,0)` branch and "to source EOF,
  leave the tail alone" in its range branch. One path now, one reading.
  The range branch also computed `len() - src_offset` on `u64` with no
  `[profile]` overflow checks in the workspace, so a source offset past
  EOF wrapped to ~16 EiB in release; that is now `NFS4ERR_INVAL`.
  `std::fs::copy` is gone from CLONE entirely — it is whole-file, it
  truncates, and it carries the source's permission bits.
- **NFSv4.2 operations were not gated on the negotiated minor version.**
  The COMPOUND decoder and dispatcher routed COPY, CLONE, ALLOCATE,
  DEALLOCATE, SEEK, READ_PLUS and IO_ADVISE purely by opcode number, and
  the only minor-version check rejected `> 2`. The pNFS MDS mount is
  `minorversion=1`, so its safety was a *client convention* — one
  hand-mount against the MDS Service port reached every 4.2 handler.
  They are now `NFS4ERR_OP_ILLEGAL` outside a 4.2 COMPOUND.
- **COPY, CLONE, ALLOCATE, DEALLOCATE and SEEK ignored striped files.**
  Only READ and WRITE consulted the pNFS stub guard. For a placement-
  pinned file the MDS's local file is a sparse size-only stub, so COPY
  read zeros and reported success, DEALLOCATE punched a hole in a file
  that is already all holes, and SEEK answered "the whole file is one
  hole" — the F15 fake-sparse class. All five now return
  `NFS4ERR_NOTSUPP` for pinned files, per file rather than per role, so
  files that were never layouted stay fully usable on an MDS.
  COPY and CLONE are guarded inside the handler rather than the
  dispatcher because their source is SAVED_FH: a `current_fh`-keyed
  guard structurally cannot see it.
- **F65 — a truncate did not recall held layouts.** The gate is a
  LAYOUTGET-time check, so a layout acquired *before* the truncate walked
  straight past it and the read never reached the MDS at all.
  `note_truncate` now recalls and revokes the file's layouts between
  marking the gate and fanning out.
- **C1 — the callback carried the layout stateid verbatim** where RFC 8881
  §12.5.3 wants `seqid+1`, so a conforming client rejected every recall.
- **C2 — `CB_SEQUENCE` hardcoded slot 0 / seqid 1.** Per-session
  back-channel slot sequencing now holds the lock across the reply await
  (§2.10.6.1).
- **C3 — a refused reply was scored as an ack.** This is why C1 and C2
  could hide: `Ok(_reply) => Acked` discarded the status, so the server
  logged success either way. Replies are classified now, including a
  compound in which `CB_LAYOUTRECALL` never ran.
- **C4 — LAYOUTCOMMIT re-extended the truncated stub.**
- **C5 — one back-channel writer per session against an `nconnect=4`
  mount**, so a session's other bound transports were never tried.
- **C6 — a grant could escape both the gate and the recall.** LAYOUTGET
  reads the gate and publishes the layout with no lock between them, so a
  grant could pass the check, have the mark arm under it, and publish
  after the recall's snapshot. The publish now re-reads the gate and
  revokes what it just inserted.
- **C8 — callbacks were sent with AUTH_NONE and refused at the RPC layer**
  (`reply_stat = MSG_DENIED`, in 419 µs — an active refusal, not a
  timeout). `csa_sec_parms<>` was never decoded; the server now answers
  with the credential the client offered.
- **C9 — the back channel was registered but never announced.** RFC 8881
  §18.36.3 makes `csr_flags` the server's answer to `csa_flags`, and
  Linux sends zero `BIND_CONN_TO_SESSION` on a v4.1 mount — so that
  echoed `CONN_BACK_CHAN` bit *is* the entire handshake. Without it the
  client refuses callbacks one layer below the auth check C8 fixed. The
  flag cannot be set alone: `nfs4_verify_back_channel_attrs` only runs
  when it is set, and the old 1 MB `csr_back_chan_attrs` against Linux's
  `PAGE_SIZE` offer would have failed the mount.
- **R2 — a self-recall stalled the connection read loop** for the full
  callback timeout.
- **R3 — a post-recall LAYOUTRETURN was answered SERVERFAULT**, which
  aborts the compound Linux folds it into and leaks the open behind it.
- **R4 — the truncate-dirty gate did not survive an MDS restart.** It is
  persisted and the retry re-armed on load.
- **A truncate's cost was linear in the number of layout holders.**
  Recalls ran sequentially with a 10 s callback timeout each, so three
  wedged holders cost 30 s of blocked SETATTR. Per-session ordering is
  required (a back channel negotiates `ca_maxrequests=1`); nothing
  required it *across* sessions. Measured 30.43 s → 10.45 s at three deaf
  holders — linear to flat.
- **The MDS-fallback delay ceiling was re-armed by every restart.** The
  gate's age lived in a process-local `Instant`, so an MDS that bounced
  more often than the ceiling could DELAY a fallback client without
  bound — the exact livelock the ceiling exists to prevent.
- **LAYOUTGET and GETDEVICEINFO answered layout types they do not serve.**
  Both decoded type 4 (FFLv4) and replied `NFS4_OK` with a files-layout
  body; GETDEVICEINFO echoed the requested type back over it, so a
  type-4 caller got a structure explicitly labelled FFLv4. Both now return
  `NFS4ERR_UNKNOWN_LAYOUTTYPE`. LAYOUTRETURN stays lenient by design — it
  emits no body, so there is nothing to mislabel.

### Changed

- **The MDS state schema is a single version with no migrations.** Nine
  incremental versions and their stepwise `ALTER` chain are replaced by
  one `CREATE TABLE` batch that already contained every column they
  added. The migration code had zero test coverage — every backend test
  builds a fresh schema — so the one path that would run against real
  state was the only one nobody exercised.
- **The dead FFLv4 layout encoder is deleted** (~440 lines). It had no
  callers, was never advertised, and had never been on a wire, but five
  green unit tests asserted its own output. Two documents told a future
  implementer to "re-enable FFLv4, ~3 days"; both now say it must be
  written fresh, and list what a fresh one must satisfy.

### Known gaps

- **`Inv_NoStaleServe` still does not hold**, for one reason that is not
  F65: revocation is server-side, so it binds only clients the recall
  reaches. A client with no live back channel at all cannot be bound
  however well the callback is encoded. Closing it needs the DS to refuse
  reads past the pending size — a DsControl fence before the `set_len`
  fanout — not the MDS to ask more politely.
- **R1 is a liveness exposure, not a correctness one.** A client recalled
  by a truncate that then parks is refused a new layout for as long as
  the park lasts. Measured on hardware: `STILL TRYLATER after 251.01s —
  never converted to an error`, refuting the audit's predicted
  "90 s DELAY then NFS4ERR_IO" (that ceiling is on the *fallback* path,
  not the LAYOUTGET path — both readings were right about different
  code).
- **The fallback-ceiling and schema changes have no live gate.** F65
  itself was gated end-to-end on hardware with wire proof; these landed
  after that cluster was torn down.

## [1.22.0] - 2026-07-30

The maintenance-and-proof release. A routine `helm upgrade` used to be
able to take a replicated volume fully down with zero real failures;
this release makes the csi-node roll a drained, barriered, node-by-node
operation and turns it on by default. Alongside it, the correctness
argument moved from prose to machine-checked models — TLA+ now covers
the replica lifecycle, claims, snapshots, expansion, the availability
envelope, cutover, the pod layer, and the DaemonSet roll itself, and the
models found real bugs the tests could not (F56, F59, F61, F62).

### Added

- **The maintenance drain roll — DEFAULT ON, and the headline behaviour
  change of this release.** The csi-node DaemonSet now runs
  `updateStrategy: OnDelete` and the controller's maintenance roller
  drives every template change node-by-node: drain the node's serving
  legs out of the raid (the FENCE), delete the pod, and advance only at
  full redundancy judged on raid membership rather than pod readiness
  (the BARRIER), with marks that die with their holder (the LEASE).
  Without it, k8s `RollingUpdate` gates on pod readiness, which knows
  nothing of raid membership — TLC refutes that gate directly
  (`FlintReplicationRollUnfenced.cfg`).

  **Rolls are now partial by design.** A node hosting a serving
  composition is REFUSED rather than rolled, and skipped with an
  operator-facing event; the campaign converges "except for N announced
  refusals". A `helm upgrade` may therefore legitimately leave
  consumer-hosting nodes on the old revision until you relocate the
  consumer and re-run. This is deliberate — a loud incomplete roll
  instead of a silent outage. The LOCAL half (rolling the node a
  consumer sits on) remains design-only; see
  `docs/f62-local-half-outage-and-blind-barrier.md`. Restore the old
  unattended behaviour with `maintenance.drainRoll.enabled: false`.
- **Bounce-free RWX admission (S2).** The hot-rejoin window now runs on
  the live serving raid, replacing the NFS-server bounce with ~228 ms of
  quiesce. Live-gated with the kill switch both ON and OFF.
- **kube-Lease leader election for the orchestrators (P1).** The
  single-orchestrator invariant is mechanical now rather than a
  deployment convention.
- **`StorageId` / `StagedHandle` newtypes (P2).** The identity-domain
  crossing that produced the F44/F45/F46 family is a compile-time
  surface.
- **Bounded dead-target detection (P4).** Closes the TCP-blackhole gap
  behind the 150–177 s RWX stall; measured 36 s after the fix.
- **TLA+ models plus a TLC gate (`scripts/check-tla.sh`, run out of
  band — not wired into CI)** covering the replica lifecycle/writer
  set, claims (the F50/F53 multi-process layer), snapshots at
  block-content level, expansion, the availability envelope, cutover,
  the pod layer, and the DaemonSet roll — plus a deterministic
  crash-sweep sim harness for hot rejoin.
- **`rust-ci` actually runs the driver's test suite.** It had been a
  developer belt that nothing enforced on push.
- **Chaos drills 3.12, 3.13, 3.14 and 2.11.** 3.14 is the maintenance
  roll's live gate; 2.11 is the all-at-once upgrade shape.

### Fixed

- **F62 — a roll could destroy a live raid composition.** Rolling the
  node that hosts a serving composition tore it down while `staged`
  stayed set, so NodeStage was never called again and only the periodic
  strike repair could rebuild it. The roller now models the
  composition's lifetime and refuses the step.
- **F61 — the drain PASS was conflated with the MARK**, so a roll step
  could advance on evidence it had not actually earned.
- **F63 — a refusal hole on `plan_roll`'s marked-node completion path.**
- **F60 — the cutover bounce is belted** by a commit-time preflight,
  a bounce claim with a deadline, and attempt backoff. The model
  refuted the first draft belt as check-then-act.
- **F59 — two rollers could double-drain**; found by the model and the
  fix sharpened by it.
- **F56 — partial expand fan-out crossed with the §5 chase produced a
  permanent size livelock.** Catch-up owns size convergence now.
- **F57 standby replacement**, per-leg maintenance suppression, a
  device high-water floor, and forced-stale guards.
- **F55 — an NFS shutdown could truncate in-flight replies**, which
  clients saw as EIO and postgres as a PANIC. Shutdown is frame-atomic
  and drains in-flight replies before exit.
- **F54 §3 — the prestage consumer is identity-verified pre-connect**,
  so a zombie never rides into the window.
- **Two fail-open paths into a single-survivor direct serve (F36c).**
- **The node-agent's data-path repair is free of the monitor pass
  chain**, and a dead component is now fatal rather than silent.
- **Probe-not-parse everywhere it converges (P3)** — the F48/F54 class
  of "parse SPDK's untextual errors" audited out.

### Notes

- Drill 3.14 passed on runap (4 runs) and again on runar against these
  bits: fence + barrier + lease + the F62 refusal proven on the wire
  under load, zero unfenced degrades, never fewer than one in_sync leg,
  consumer through with zero PANIC/EIO/ESTALE and zero restarts.
- Drill 2.11 (all-at-once: every tgt under the volume killed at once,
  raid host included) passed on two clusters — never a degraded-direct
  serve, never an acked-tail risk, composition rebuilt in 105 s.
- This file skips 1.7.0 through 1.21.0; those releases were tagged and
  published without CHANGELOG entries.

## [1.6.0] - 2026-07-04

### Added

- **Two-altitude topology view.** The dashboard topology page is a real
  data-path graph now (React Flow): the volume altitude draws
  consumer → access device → RAID bdev → members → backing disks, with
  edge encodings for health (color), access path (solid local / dashed
  NVMe-oF), and recovery (animation); a rebuild renders as an animated
  source→target edge with live progress. The cluster altitude lays one
  card per node (disk-state ring, replica/capacity counts) with
  replica-placement links between nodes, drill-through into the volume
  view. Node/edge details, sync state, NQNs, and the RAID/NVMe-oF
  explainers live in an on-demand drawer instead of inline walls of
  text.
- **`identity.rs` owns every derived name** (Phases 0–4 + CI lint):
  lvols, snapshots, epochs, NQNs, lvstores — one constructor set, one
  published contract (`docs/identity-contract.md`), and now the inverse
  parser (`lvol_owner`) that maps any lvol name back to its owning
  volume.
- **NFS server-pod liveness reconciler.** A bare NFS server-pod death
  (node loss, OOM-kill, manual delete) now self-heals: the controller
  reconciles the pod back, republishes the Service endpoint, and
  clients resume in ~30–42 s.
- **Incremental replica-rebuild kuttl suite** joins `make test`
  (isolated run, exercises the epoch/catch-up orchestrators
  end-to-end).
- Dashboard sessions survive a page refresh (token in sessionStorage,
  per-tab; still expires server-side and on backend restart).

### Fixed

- **Every live lvol on a replicated cluster was reported as an
  orphaned "cleanup candidate."** Orphan detection allowlisted lvols
  against the legacy single-replica `lvol-uuid` PV attribute, which
  replica-set PVs don't carry — so replicas, user snapshots, and epoch
  snapshots all showed as deletable orphans, cloned onto every disk of
  their node. Classification now parses each lvol's owner from its
  name via the identity contract (robust to `_hr` recovery renames),
  fills the long-empty `provisioned_volumes` per disk, and attributes
  both provisioned entries and true orphans to the disk whose lvstore
  actually hosts them.
- **RWX client unpublish tore down the NFS server's live export.**
  `ControllerUnpublishVolume` treated every departing non-home node
  as a remote block consumer and removed the volume-level NVMe-oF
  target — but RWX/ROX consumers are NFS clients with no block path,
  and that target is the NFS server's backing export. One client
  finishing was enough to strand the server's initiator in a
  reconnect loop with its journal pinned (unkillable server pod
  until `ctrl_loss_tmo`). Unpublish now classifies shared volumes by
  PV access modes (the ControllerUnpublish side of the 1.4.0
  NodeUnstage fix) and leaves their target alone; `DeleteVolume`
  owns its teardown. RWO fencing semantics are unchanged, including
  when the PV is unreadable.
- **RWX teardown ordering.** `DeleteVolume` tore down the backing
  volume's NVMe-oF targets immediately after *issuing* the NFS server
  pod delete — while the pod, the volume's consumer, was still
  flushing its dirty ext4 journal through those targets. The kernel
  initiator then reconnect-looped against the vanished subsystem with
  the journal pinned in D-state, leaving the pod unkillable until
  `ctrl_loss_tmo` (~10 minutes). Deletion now waits (bounded, 90 s)
  for the pod object to be removed — kubelet's signal that the volume
  was unmounted and flushed — before target teardown proceeds.
- Clients are never bound to a Terminating NFS server pod; dead-NFS
  mountpoint probes are bounded and no longer misread as "not
  mounted"; `NodeGetVolumeStats` filesystem calls are bounded so a
  dead NFS mount cannot starve the node plugin.
- Shared (RWX/ROX) volume expansion is refused loudly instead of
  silently corrupting state (client-side expand cannot reach the
  server's backing filesystem).
- Snapshot timeline shows real creation times (VolumeSnapshotContent /
  sync-record timestamps) on two lanes — user snapshots and engine
  epochs — with CR-path user-snapshot deletion that keeps the CR and
  SPDK content in step; the old always-empty "Topology View" tab and
  its dead-code renderer are gone.
- Disk-delete refusals surface the node agent's actual status and
  reason (e.g. 409 "N logical volumes still exist") instead of a
  generic 502; the snapshot detail modal's disabled "coming soon"
  Delete/Clone buttons are removed.

### Changed

- The legacy `spdk-controller-operator` deployment defaults **off**
  (verified unreachable in the identity audit); remove any explicit
  `spdkOperator.enabled: true` override when upgrading.

## [1.5.0] - 2026-07-03

Dashboard release: the operations dashboard gains structure (URL
routing, a real test safety net, this repo's first CI), a coherent
visual system, and sheds its last fabricated data. No changes to the
public API surface (CSI gRPC verbs, StorageClass parameters,
`volume_context` keys).

### Added

- **Deep-linkable dashboard state.** Tabs, cross-tab filters, and
  volume/snapshot detail selections live in the URL (react-router);
  refresh and back/forward are safe, and any view can be shared as a
  link.
- **Frontend safety net + CI.** 73 Vitest/RTL tests with MSW fixtures
  typed against the generated OpenAPI schema (contract drift is a
  compile error), and two GitHub Actions gates: the dashboard suite
  and OpenAPI-spec freshness in both directions (the Rust structs are
  the schema's sole author).
- **Primitive UI kit with one status vocabulary.** Chip, ProgressBar,
  Card, Skeleton, AsyncView, and ConfirmModal primitives; a single
  status-color vocabulary aliased to semantic Tailwind tokens; errors
  never blank present data (stale banner instead); destructive flows
  gate on a typed phrase. Entry bundle code-split 1013 KB → 296 KB.
- **Node agent `POST /api/disks/delete`** — the strict inverse of
  disk initialize: a no-op on an uninitialized disk, a 409 refusal
  while any logical volume still exists on the store. The dashboard's
  delete proxy is now documented in the OpenAPI spec.
- **Committed end-to-end bulk-init drill**
  (`spdk-dashboard/scripts/bulk-init-drill.mjs`) — Step 0 of the
  remote-builder runbook: every fresh builder's pristine scratch NVMe
  exercises the full select → manifest → confirm → LVS-Ready flow
  against a real agent before being repurposed.

### Fixed

- **Epoch snapshots resolve to their volume.** `epoch-<pv>-<seq>`
  names now parse to their PV (right-anchored; the trailing segment
  must be the numeric sequence), so Tier-2 epoch snapshots no longer
  pile into a single "unknown" bucket in the snapshot tree. Tree
  entries are labeled with the PV name; the backend also re-derives
  ids as a fallback for older agents.
- **Disk lvol counts were always 0** (release-check-found). The SPDK
  lvol counter matched a `lvol_store_name` field that
  `bdev_get_bdevs` does not emit; live stores therefore always
  reported zero lvols — which also meant the new delete endpoint's
  refusal guard could not fire. The counter now matches
  `lvol_store_uuid` and the `<lvs>/<name>` alias, and
  `delete_blobstore` re-counts against fresh SPDK state immediately
  before the destructive RPC instead of trusting a discovery
  snapshot.
- Frontend strictness: zero `any` types; `noUncheckedIndexedAccess`.

### Removed

- **Fabricated dashboard data.** The Remote Storage tab (pure
  client-side mocks; no backend routes ever existed) and the snapshot
  list's invented per-snapshot storage consumption are gone. The
  snapshot tree's real backend analytics (SPDK bdev consumption)
  remain.

### Changed

- The frontend image's `nginx.conf` is the single source of truth;
  the chart no longer overlays it with a ConfigMap.

## [1.4.0] - 2026-07-03

Tier-2 hot rejoin ships: non-disruptive standby admission for
attached RWO volumes. Validated at 2–4 replicas through staggered
multi-failure drills: zero acked-write loss across 145,000+ fsync'd
writes, 5 controller deaths, 12+ leg kills, and one full raid
collapse.

### Added

- **Hot rejoin (Tier-2).** Leased quiesce windows (100–200 ms esnap
  path; O(delta) inline fenced-final-delta path, chosen adaptively by
  a delta estimator), epoch catch-up with coverage-aware source
  selection, esnap localization with local-chain resume, crash-decode
  reconciler (adopt/scrub/resume/demote), defensive unquiesce, and
  per-volume rejoin claims. `spdk-tgt` 1.4.0 = SPDK v26.05 + raid
  skip_rebuild / leased-quiesce patch v3. Operator runbook:
  `docs/tier2-operator-runbook.md`. Drill-only fault knob
  `FLINT_HOT_REJOIN_FAULT` (never set in production).
- **NFSv4 state persistence across server replacement** (`state.db`
  on the export volume) — closes 1.3.0's "dirty open state lost at
  bounce" limitation. Locks remain memory-only.
- Node agents reap dead reconnect-looping NVMe-oF controllers.
- Operations dashboard phases 0–2d: backend-enforced bearer auth,
  TanStack Query data layer + backend aggregate cache, live replica
  sync state, live volume detail, engine event timeline with
  hot-rejoin windows, bulk disk initialization, and OpenAPI-generated
  frontend types.

### Fixed

- **Latent 1.3.0 shared-volume unstage bug (found by this release's
  gate).** NodeUnstage classified NFS consumers by `findmnt` on the
  staging path, but RWX/ROX consumers mount at publish time — so
  every shared-volume consumer unstage ran the block teardown, whose
  per-replica sweep could delete the NFS server's live backing
  exports. Classification now reads the PV's access modes (`findmnt`
  only as a fallback).
- Staggered-failure fixes from the 3-failure drill campaign: chase and
  catch-up sources resolve via the record's live uuid and fail over by
  lineage coverage; E_f cuts on each survivor's live head; the
  localization backfill and phase-4 admission sources are
  coverage-probed; the orphan sweep learned the hot-rejoin name
  shapes; esnap-resume prefers the local chain.

## [1.3.0] - 2026-06-12

Self-healing release: every common single-failure (replica node loss,
consumer spdk-tgt restart, lone container restart, same-node reschedule
race) now heals autonomously, typically within ~3 minutes and without
workload restarts. All changes validated live on AWS i4i clusters with
forced failure injection.

### Added

- **Consumer data-path self-healing (4 layers).** Storage-baseline
  recovery re-adopts disks after a lone `spdk-tgt` restart (~30 s);
  data-path-lost detection flags volumes whose raid vanished under a
  live attachment (3-strike, PV annotation + events); in-place repair
  rebuilds the raid and loopback export with a **pinned NVMe namespace
  identity** so the kernel initiator reattaches without a workload
  restart; and the cutover orchestrator bounces as a last-resort
  fallback. Escape hatch: `FLINT_DATA_PATH_REPAIR=disabled`.
- **Scheduling escalation for cutover bounces.** Every bounce applies a
  self-expiring `NoSchedule` taint (`disk.chert.us/bounce`,
  TTL `FLINT_CUTOVER_TAINT_SECS`, default 120 s) to the bounced node so
  the replacement cannot reuse the stale staged volume — reassembly
  bounces are now deterministic instead of scheduler-dependent.
- **Orphan sweep (§10-14).** Node agents reap lvols and NVMe-oF
  subsystems whose owning PV no longer exists (3-strike confirmation,
  strict parsers, ublk-verified ephemeral handling).
  `FLINT_ORPHAN_SWEEP=disabled` to opt out.
- Dashboard backend `/healthz` endpoint; liveness/readiness probes
  moved off the aggregate `/api/dashboard` endpoint.

### Fixed

- **RWX volume identity aliasing (six fixes).** An RWX volume's three
  identities (user PV, synthetic backing PV, volumeHandle) corrupted
  each other: zombie raids at unstage blocked every later restage; a
  permanent data-path false positive drove endless NFS-pod bounce
  loops; duplicate epoch/catch-up streams broke snapshot lineage and
  standby admission; replica exports were squatted under alias NQNs;
  an RWX consumer's unstage could detach the live raid's legs; and NFS
  server bounces invalidated every client file handle (now pinned per
  volume via `PNFS_INSTANCE_ID`; foreign handles answer `NFS4ERR_STALE`
  so clients recover by re-walking).
- Retention pin lifecycle: held until standby admission (not copy
  completion) and advanced with the standby's chase mark — epoch
  history no longer grows unbounded behind a chasing standby.
- Dashboard: unreachable nodes can no longer hang the aggregate fetch
  past the liveness deadline (bounded per-node timeouts), and the
  frontend no longer substitutes mock data when the backend is
  unreachable — it keeps last-known data and shows an error banner.

### Known limitations

- **RWX cutover transparency requires clean client state.** A client
  holding dirty open state (unsynced writes) across an NFS server
  bounce can have those writes dropped: the server's NFSv4 state is
  in-memory and does not survive pod replacement. Read-mostly and
  fsync-disciplined workloads ride through transparently. Persistent
  state (SQLite backend on the exported volume) is the next milestone.
- Migration from ≤1.2.0: existing volumes cross onto the pinned
  namespace identity at their next detach/restage; existing NFS server
  pods mint stable file-handle ids at their next recreation.

## [1.2.0] - 2026-06-11

- **Incremental replica rebuild** (phases 1–5b) and superblock-less
  raids.
- **Bounded unstage umount** — a wedged NFS mount can no longer hang
  `NodeUnstageVolume` indefinitely.

## [1.1.1] - 2026-06-10

- **NVMe-oF fencing admits the consumer node.**
  `ControllerPublishVolume` whitelisted the controller pod's host NQN
  instead of the consuming node's, so every cross-node single-replica
  attach was fenced out with EIO. (1.1.0 introduced the phase-0
  fencing and was superseded by this tag without a standalone
  release.)

## [1.0.0] - 2026-05-04

First stable release. Production-ready for SPDK-based deployments;
no-SPDK deployments supported with documented feature subsets. From
this release onward, breaking changes to the CSI gRPC surface,
StorageClass parameters, or `volume_context` keys require a `MAJOR`
version bump.

### Storage architecture

- **High-performance local block storage via SPDK userspace I/O.**
  Bypasses the kernel block layer; delivers full NVMe bandwidth from
  a userspace target backed by `ublk` on each worker. Per-worker
  hugepage and disk requirements documented in the README.
- **Multi-replica volumes via NVMe-oF RAID across nodes.** RAID-1
  mirrors and optional RAID-5f, transparent to the NFS protocol layer.
  Survives single-disk and single-node loss without client-visible
  outages beyond the underlying NVMe-oF reconnect window.
- **pNFS data path** (RFC 8881 FILE layout). Parallel-server NFSv4.1
  with stripes across multiple data servers; opt-in via StorageClass
  `parameters.layout: pnfs`. Single-host bench shows ~1.6× write
  throughput over single-server NFS at fsync=1 (ADR 0003); cross-host
  scaling measurable via the included Kubernetes bench harness
  (`make test-pnfs-cross-host`).
- **Volume snapshots and clones** in SPDK mode via `bdev_lvol_snapshot`
  and `bdev_lvol_clone`. Instant copy-on-write; space-efficient.
- **Online volume expansion** without downtime.
- **CSI inline ephemeral volumes** for pod-scoped temporary storage.

### pNFS production hardening

- **Persistent NFSv4.1 / pNFS server state** (`Phase B`). Client IDs,
  sessions, stateids, layouts, and pNFS file handles survive MDS pod
  restarts via a SQLite-backed `StateBackend` (WAL + NORMAL crash-
  safe). Kernel clients reconnecting after a restart resume against
  the same record set with no `STALE_CLIENTID` or `BAD_STATEID` storm.
  Verified end-to-end via `make test-pnfs-restart` with byte-for-byte
  hash matching across restart.
- **DS death recovery** (`Phase A`). Heartbeat monitor detects a dead
  data server, fans out `CB_LAYOUTRECALL` to all affected client
  sessions via the back-channel, and forcibly revokes layouts after
  the RFC 5661 §12.5.5.2 deadline if clients don't return them.
  Verified end-to-end via `make test-pnfs-recall`.
- **NFSv4.1 RFC conformance.** Pynfs full suite: 167 PASS / 4 FAIL /
  91 SKIP (5.8× the original audit baseline of 26 PASS). Six suites
  at 100%, nine more above 70%. The four remaining failures are
  documented niche cases that do not cascade or corrupt data.

### CSI integration

- **StorageClass `parameters.layout: pnfs`** opts a volume into the
  pNFS data path. Default StorageClasses use single-server NFS or
  direct SPDK block per existing chart configuration.
- **`volume_context` namespaces.** Production keys live under
  `disk.chert.us/*` (SPDK mode) and `pnfs.chert.us/*`
  (pNFS mode). These namespaces are stable from 1.0.0; new keys may
  be added in `MINOR` releases, removals or renames require `MAJOR`.
- **VolumeSnapshot CRD preflight.** At controller startup, the driver
  checks for the cluster-wide `VolumeSnapshot{,Class,Content}` CRDs
  and logs a one-line warning with the install command if any are
  missing. Non-fatal: non-snapshot RPCs work without the CRDs.
- **Snapshot guards for unsupported volume types.** `CreateSnapshot`
  and `CreateVolume`-from-snapshot/PVC return `FAILED_PRECONDITION`
  (final, non-retryable per CSI) for pNFS volumes, replacing a prior
  `NOT_FOUND`-induced retry loop in `external-snapshotter`.

### Operations & ergonomics

- **Helm chart** for installation under Kubernetes 1.21+. Optional
  pNFS mode (`pnfs.enabled: true`); SPDK enabled by default.
- **Web dashboard** for disk discovery, initialization, and monitoring.
- **`NOTES.txt`** rendered after `helm install` surfacing the
  `VolumeSnapshot` CRD prerequisite explicitly.
- **Test surface:** 330 Rust unit tests, KUTTL system tests across
  SPDK + pNFS paths, Lima e2e harnesses for pNFS protocol / restart /
  recall flows, and a scaffolded cross-host bench harness.

### Deployment modes

| Mode | Storage backend | Snapshots | Replication | Status |
|---|---|---|---|---|
| Production-SPDK | SPDK blobstore | ✅ Native COW | ✅ NVMe-oF RAID | Recommended |
| Production-no-SPDK (single-server NFS) | Filesystem | ⏸️ Roadmap | ❌ Customer-provided | Supported |
| Production-no-SPDK (pNFS) | Filesystem | ❌ Not supported | ❌ Customer-provided | Supported with limits |
| Dev/QE (Kind/Lima) | Loopback | Optional | None | Dev only |

### Container images

Published to Docker Hub under the `dilipdalton/` namespace for
`linux/amd64`:

```
dilipdalton/flint-csi-driver:1.0.0
dilipdalton/spdk-target:1.0.0
dilipdalton/flint-dashboard:1.0.0
```

Aliases: `:1.0`, `:1`, `:latest`. **Production deployments should pin
to an immutable tag (`:1.0.0`).** The chart's `values.yaml` defaults
to `:latest` for development convenience; production users should set
each `images.<component>.tag` to `"1.0.0"`.

### Known limitations

- **pNFS volumes do not support snapshots in any deployment mode.**
  Snapshot RPCs against pNFS sources return `FAILED_PRECONDITION`.
  Workaround: use a non-pNFS StorageClass for volumes that need
  snapshots, or use SPDK mode for performance + snapshot capability.
- **No-SPDK volumes have no Flint-level replication.** Durability
  comes from the underlying block volume (EBS/PD/Ceph RBD/etc.). For
  cross-node redundancy without external durable storage, use SPDK
  mode with NVMe-oF RAID.
- **`linux/arm64` container images are not published in this release.**
  ARM64 is a planned target; x86-64 ships first to match the primary
  deployment fleet (Cloudera customer infrastructure and current QE/CI).
  ARM64 builds will follow in a subsequent release.
- **`VolumeSnapshot` CRDs are a cluster-wide prerequisite** not
  installed by the Flint chart (cluster-singleton; bundling them
  would conflict with other CSI drivers). Without them, the bundled
  `snapshot-controller` Deployment will `CrashLoopBackOff`. See
  README "Snapshot Prerequisites" for the install command. The Flint
  controller logs a startup warning if missing.
- **pNFS Flex Files (FFL) layout is not implemented and is deferred
  indefinitely.** Replication is handled at the SPDK NVMe-oF RAID
  layer (below the protocol); FFL would duplicate that capability
  with client-side write amplification and a separate rebuild
  scanner. Decision recorded in
  `docs/plans/pnfs-production-readiness.md`.

### Upgrade notes

This is the first tagged release. There are no prior stable versions
to upgrade from. Operators running pre-1.0 builds should reinstall
fresh against `v1.0.0`. The pre-1.0 git history is preserved at the
`archive/config` and `archive/disk_mgmt` tags for forensic reference;
neither tag represents a supported upgrade source.

### Security

No security advisories at this release.

[Unreleased]: https://github.com/ddalton/flint/compare/v1.45.0...HEAD
[1.45.0]: https://github.com/ddalton/flint/compare/v1.44.0...v1.45.0
[1.44.0]: https://github.com/ddalton/flint/compare/v1.43.0...v1.44.0
[1.43.0]: https://github.com/ddalton/flint/compare/v1.42.0...v1.43.0
[1.42.0]: https://github.com/ddalton/flint/compare/v1.41.1...v1.42.0
[1.41.1]: https://github.com/ddalton/flint/compare/v1.41.0...v1.41.1
[1.41.0]: https://github.com/ddalton/flint/compare/v1.40.0...v1.41.0
# 1.40.0 was cut AFTER 1.41.0 (which reserved the number), so it is
# compared against v1.41.0 rather than v1.39.0 — that diff is the
# passthrough work and nothing else.
[1.40.0]: https://github.com/ddalton/flint/compare/v1.39.0...v1.40.0
[1.39.0]: https://github.com/ddalton/flint/compare/v1.38.0...v1.39.0
[1.38.0]: https://github.com/ddalton/flint/compare/v1.37.0...v1.38.0
[1.37.0]: https://github.com/ddalton/flint/compare/v1.36.0...v1.37.0
[1.36.0]: https://github.com/ddalton/flint/compare/v1.35.1...v1.36.0
[1.35.1]: https://github.com/ddalton/flint/compare/v1.35.0...v1.35.1
[1.35.0]: https://github.com/ddalton/flint/compare/v1.34.0...v1.35.0
[1.34.0]: https://github.com/ddalton/flint/compare/v1.33.0...v1.34.0
[1.33.0]: https://github.com/ddalton/flint/compare/v1.32.0...v1.33.0
[1.32.0]: https://github.com/ddalton/flint/compare/v1.31.0...v1.32.0
[1.31.0]: https://github.com/ddalton/flint/compare/v1.30.0...v1.31.0
[1.30.0]: https://github.com/ddalton/flint/compare/v1.29.0...v1.30.0
[1.29.0]: https://github.com/ddalton/flint/compare/v1.28.0...v1.29.0
[1.28.0]: https://github.com/ddalton/flint/compare/v1.27.0...v1.28.0
[1.27.0]: https://github.com/ddalton/flint/compare/v1.26.0...v1.27.0
[1.26.0]: https://github.com/ddalton/flint/compare/v1.25.2...v1.26.0
[1.25.2]: https://github.com/ddalton/flint/compare/v1.25.1...v1.25.2
[1.25.1]: https://github.com/ddalton/flint/compare/v1.25.0...v1.25.1
[1.25.0]: https://github.com/ddalton/flint/compare/v1.24.0...v1.25.0
[1.24.0]: https://github.com/ddalton/flint/compare/v1.23.0...v1.24.0
[1.23.0]: https://github.com/ddalton/flint/compare/v1.22.0...v1.23.0
[1.22.0]: https://github.com/ddalton/flint/compare/v1.21.0...v1.22.0
[1.6.0]: https://github.com/ddalton/flint/compare/v1.5.0...v1.6.0
[1.5.0]: https://github.com/ddalton/flint/compare/v1.4.0...v1.5.0
[1.4.0]: https://github.com/ddalton/flint/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/ddalton/flint/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/ddalton/flint/compare/v1.1.1...v1.2.0
[1.1.1]: https://github.com/ddalton/flint/compare/v1.0.0...v1.1.1
[1.0.0]: https://github.com/ddalton/flint/releases/tag/v1.0.0
