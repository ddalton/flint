<!-- An 11-agent investigation, 2026-09-07, into partitioning one repository's
ref namespace across shards with a single writer each. Commissioned after the
log-first review refused multi-writer and named this as the one shape that
could scale, needing its own document. Two of its findings were verified by
hand against the source before this was accepted: that `atomic` is never
parsed anywhere in the syncer, and that the gitqual leg asserting it uses a
non-fast-forward the client rejects locally. -->

# Sharded single-writer for forge — the verdict

**Refused, with a named trigger.** Not because the shape is incoherent — it can be made coherent — but because coherence costs S full replicas of the object graph, a new pkt-line-parsing component in the one module built never to read a body, and a rewrite of the sweep's correctness argument, to multiply a resource forge is using at **2–5% of capacity**.

The document the 14-agent verdict asked for is this one. Its answer is: build more repositories, not more writers.

---

## 1. IS IT COHERENT AT ALL

Two questions decide it, and the object-sharing one is fatal to the design *as stated*. Take them in the order that matters.

### 1.1 The partition does not partition the objects

The proposal is "partition the ref namespace, share the immutable pack pool." The second half is false. Three independent proofs, all measured against git 2.50.1:

**Thin packs.** `git send-pack` deltas against whatever the advertisement claimed the server holds. If the door assembles a union advertisement, the pack shard B receives is delta'd against objects living only in shard A. Measured: a thin pack of one branch with `main` excluded is 276 bytes; `git index-pack --fix-thin --stdin` into a repository lacking `main` fails `fatal: pack has 1 unresolved delta`, rc=128. That happens **inside `receive-pack`, before `pre-receive`, before `proc-receive`, before the syncer is ever contacted** — nothing forge owns can catch, report, or route around it. The door cannot split the pack either: splitting requires a reachability computation over an object store the door does not have.

**Ref enumeration.** A shard cannot even *list* a ref whose object it lacks: measured `fatal: missing object <oid> for refs/heads/main` from `git for-each-ref`. `forge/syncer/src/gitcmd.rs:235` reads the local ref set with exactly that command, so the syncer's own `refs()` would fail on any shard holding a foreign ref name.

**Merge.** `forge/syncer/src/batch.rs:419-420` routes `refs/for/<target>` to `judge_merge` (`batch.rs:471`), which needs the *proposer's* object present while writing the *target's* ref. Shard 0, owning `refs/heads/main`, must hold every other shard's objects to judge one merge request.

So **every shard is a full replica of the object graph.** What remains partitioned is the ref map and the CAS'd snapshot — nothing else. The honest name for the design is not "sharded single-writer." It is **S full replicas, S leases, S snapshots, one façade.** Everything below is scored against that.

### 1.2 Sharing the pool then breaks both garbage collectors — and one of them is the verdict's own objection, inherited unmodified

`forge/syncer/src/sweep.rs:1-29` states four rules and calls each one load-bearing. Rule 1 is implemented at `sweep.rs:71-85`: list candidates, **then** read the reference set, abort if the etag moved. That argument requires **one snapshot read as one atomic cut**. S snapshots give S cuts and no instant at which all S are simultaneously valid. At 15.5 pushes/s spread across S shards, "none of the S moved" essentially never holds — so either the sweep never runs (unbounded storage) or it judges against one shard's snapshot and deletes packs another shard's refs name. The second outcome is not hypothetical: `forge/syncer/src/restore.rs:115` then refuses forever — *"snapshot names N pack(s) the bucket does not hold — refusing to serve a repository that cannot be restored"* — which is the runcd failure reproduced by design instead of by accident.

And `sweep::abort_orphaned_uploads` (`sweep.rs:43-56`) lists `format!("{}/", sc.cfg.git_prefix())` at `sweep.rs:50-51` and aborts **every** pending multipart upload under it, on explicitly single-process reasoning. Its only guard is `if sc.fold.is_some()` (`sweep.rs:46-49`) — process memory, blind across processes. **This is scoped by KEY PREFIX, not by ref.** Partitioning the ref namespace does not touch it. Shard A's between-batch sweep aborts shard B's in-flight 48×8 MiB push. This is precisely the hazard the 14-agent review named as fatal for multi-writer, and ref-sharding inherits it whole.

There is a repair — per-shard key prefixes with full object closure per shard, plus a claims-listing reference set modelled on `undo::referenced` (`undo.rs:157-179`, sound because the copy is written *before* the CAS) — but it costs S× the stored bytes on a system already measured at **47.8 GB vs walgit's 16.5 GB (3:1)**, and S× the 17 s/GiB restore. A 40 GiB repository becomes 160 GiB at S=4, with ~680 s of cold restore per shard.

### 1.3 The wire protocol: feasible, and it forces the door to become what it was built not to be

The good news first: **stock clients need no changes.** The client never learns there are shards. Per-ref read-your-writes holds, which is the genuine merit of partitioning by *name* rather than by hash. Push is still protocol v0 and ungzipped, so a splitter is mechanically possible.

The cost is that the door is structurally blind by design. `spdk-csi-driver/src/lite_gateway/git.rs:583` and `:613` take `warp::body::stream()` and hand it straight to `reqwest::Body::wrap_stream` at `:990`; `:953` builds one URL as `endpoint + verb.suffix()` where `suffix()` returns `&'static str` (`:102`, asserted at `:1839`); `endpoint` is one `status.gitEndpoint` (`crd.rs:328`) naming one headless Service. There are **zero pkt-line parses in the whole 1,885-line module.** Its own module doc states the invariant sharding must break. Ref routing needs a new component that parses the receive-pack command list, tees a multi-GB pack to S upstreams, demuxes sideband-64k, and merges S report streams — and `forge/syncer/src/pktline.rs` is 72 lines of framing with no sideband.

Three residual client-visible defects even when it works:

- **Reads cannot be sharded at all.** One `upload-pack` must see the whole graph. A shard serving a fetch that lags the assembled advertisement returns `fatal: remote error: upload-pack: not our ref <oid>` — measured. With a headless Service resolving to S pod IPs, `GET /info/refs` and the follow-on POST can land on different pods; affinity becomes mandatory.
- **`report-status-v2` has exactly one `unpack <status>` line for the whole pack.** If shard A reports `unpack ok` and shard B `unpack error`, the merged report cannot express it — and A's refs are already CAS'd durable (`batch.rs:317-345`, `:354`) before any report is sent.
- **A cross-shard `--atomic` push must be refused.** Which is *strictly better than today* — see §3.2.

**Verdict on coherence:** coherent only as S full replicas with per-shard prefixes, per-shard claims-based GC, a union-advertisement splitter, and cross-shard atomicity refused. That is not the cheap thing the one-line proposal implies.

---

## 2. IS THE CONSTRAINT EVER BINDING

No. It is 18–48× away, and no git shop on earth is near it.

**Forge's own arithmetic.** 15.5 acked pushes/s ÷ 23–30 pushes per CAS = **0.52–0.67 conditional writes/s** on the hot key, against a ceiling of ~12–25 CAS/s. That is **2.1%–5.6% utilisation** of the exact resource sharding multiplies. The 545,613-byte whole-snapshot rewrite at 8,000 refs, at 0.67 CAS/s, is 366 KB/s — a rounding error. Batch duration 30 ÷ 15.5 = 1.94 s, consistent with the measured P2 median of 1.75 s.

**Splitting makes the per-push cost worse, and the code says so.** `forge/syncer/src/server.rs:520`: *"the batch pays one lease renewal, one snapshot CAS and one ref transaction however many pushes it carries, so the cost per push FALLS as the fleet gets busier."* Batching is per-process and self-tuning — `batch_window_ms: 0`, `batch_max: 64` (`lib.rs:282-283`), and `collect` (`server.rs:523-546`) drains exactly what queued while the previous batch ran. **The batch is the load.** At S=8 each shard sees ~1.9 pushes/s, so each batch carries 1–2 pushes instead of ~25, and the aggregate CAS rate rises to ~13/s — from 4% of the ceiling to nearly all of it, for the same delivered work. Sharding does not raise the ceiling relative to the load; it walks the load into it.

**What real high-rate git shops do** — surveyed, and the answer is unanimous:

| System | Shards one repo's refs across writers? | Unit of write ownership |
|---|---|---|
| Google Git-on-Borg | **Had** per-ref Bigtable rows — **abandoned them** (2017) | one reftable stack per repository |
| git reftable (upstream + JGit) | No — spec: *"single-threaded for writers"* | one `tables.list.lock` per repository |
| Gerrit multi-site / global-refdb | No — per-ref CAS is a split-brain **detector** | (project, ref) CAS row; no owner assigned |
| JGit Ketch (Raft multi-master) | No | one leader per repository; `refs/txn/accepted` |
| GitHub Spokes / DGit | No — concurrency **deliberately eliminated** | exclusive lock on a majority of replicas |
| GitLab Gitaly / Praefect | No — "shard" = node holding whole repos | primary per repository; per-repo WAL |
| Microsoft VFS for Git / Scalar | No — partitioned **objects + working tree** | n/a (client-side) |
| Azure Repos | No — "Limited Refs" shrinks what clients *see* | per repository |
| git namespaces | Partitions ref **names**, not the write path | each namespace is a separate repo to clients |

Two data points end the argument. Google ran per-ref write granularity (one Bigtable row per ref, `android:wifi:changes/1000`) at **1.8M branches** — 200× forge's 8,000 refs — and replaced it with a *single serialized stack*, because per-row writes cannot deliver atomic multi-ref batches. And the Windows monorepo (300 GB, 3.5M files, 4,000 engineers) does **8,421 pushes/day ≈ 0.097 pushes/s**; Piper, >1B files, does 45,000 commits/day = 0.52/s under one global ordering.

**Forge's single writer sustains 160× the largest git monorepo on earth**, and 1.34M pushes/day.

One more thing kills the fit specifically: the measured workload is 32 pushers to **distinct** branches, and its integration path is one ref by construction — every `refs/for/main` lands on the shard owning `main` (`batch.rs:419-420`, `:471`), where proposals are merged serially onto the moving tip (`batch.rs:487`). Sharding scales the half of the workload that is not the bottleneck, and cannot scale the half that is. A single ref admits one push per client fetch round-trip anyway: `batch.rs:171` inserts each accepted update into the running view `eff` and `batch.rs:429-437` judges the next command against it, so of 30 concurrent pushes to one ref, 1 wins and 29 get `stale info: fetch first` — a shipped falsifier (`tests.rs:315`).

---

## 3. THE RECOMMENDATION

### 3.1 Refuse it. Use more repositories.

A second `FlintRepo` is one CR: own `keyPrefix`, own lease, own snapshot, own Deployment, own idle rung scaling to zero (`reconcile.rs:184`). It multiplies the CAS ceiling **exactly**, with no shared prefix, no union advertisement, no splitter, no handoff, and **zero new code** — and it preserves `sweep.rs` verbatim.

Sharding costs the same S Deployments, S Services, S leases, S full `emptyDir` clones — operationally a wash — and adds the splitter, the union advertisement, the report merge, the atomic refusal, and a rewritten sweep. What it buys over more repositories is **one URL and one object graph**. That is a façade, and git already ships the façade: `gitnamespaces(7)` partitions the ref namespace over a shared object store, and its own caveat — no operation spans namespaces — is the same wall seen from the other side.

One consequence is unfixable and should be stated in any future proposal: **the shard map is immutable for the life of the repository.** `crd.rs:68-69` already makes `keyPrefix` immutable by CEL for exactly this class of reason. Moving `refs/heads/team-c/*` from shard 1 to shard 2 is an atomic ref move across two independently-leased snapshots — a cross-shard transaction, which *is* the multi-writer commit problem the 14-agent review refused. So S and the prefix map must be chosen at creation, before the branch layout is known, and `policy.rs:22-27` (many pods share one ServiceAccount) rules out deriving it per-principal. A new team either has no shard or forces a repository recreation.

### 3.2 Take these two defects out of the study — they are live and independent of sharding

**`--atomic` is accepted and not honoured.** `gitcmd.rs:179` sets `receive.procReceiveRefs = refs/` — *every* ref — which excludes proc-receive'd commands from git's own atomic ref transaction. `hook.rs:161-169` parses the client capability list for `push-options` only and echoes `version=1\0push-options`; `atomic` is dropped on the floor (`grep -i atomic forge/syncer/src/*.rs` finds nothing but `AtomicU64`). Measured: `git push --atomic` of two refs, hook returns `ok agent-work` / `ng locked`, and agent-work lands on disk — in forge it is also CAS'd into S3 (`batch.rs:317-345`) and update-ref'd (`batch.rs:354`) before any report. Per-command refusal is a deliberate choice (`batch.rs:157-169`); the bug is accepting a capability that contradicts it. **Fix: refuse a multi-command `--atomic` push, or honour it.**

**The test for it is vacuous.** `forge/e2e/gitqual/run-gitqual.sh:217-219` pushes `--atomic C3:refs/heads/atomic-a C1:refs/heads/main` with no `--force`. `main` is a non-fast-forward the client sees in the advertisement, so git aborts locally: measured `error: atomic push failed for ref refs/heads/main. status: 2`, hook never invoked. **The leg passes against a server with no atomicity whatsoever.** Its control at `:214-216` controls for the wrong thing.

---

## 4. IF IT WERE BUILT

The design, module by module. No discount for small S — the mechanism is identical at S=2.

**Shape:** per-shard key prefixes (`{prefix}/shards/{i}/…`), each shard a **full replica** of the object graph, per-shard lease + snapshot + log, one leased compactor owning fold/base-rebuild, and read traffic served by union followers (`follow.rs` is already read-only and takes no lease, `follow.rs:28-32`).

| Component | Change | Est. lines |
|---|---|---|
| **door `git.rs`** (1,885) | pkt-line command splitter; pack tee to S upstreams; sideband-64k demux; S→1 report-status-v2 merge; union `info/refs` with one `symref=HEAD`/`object-format`/capability line; per-verb pod affinity | **1,500–2,500 new** |
| **`sweep.rs`** (196) | the *rule* changes, not just code: rule 1's single atomic cut (`sweep.rs:71-85`) is meaningless across S snapshots — replace with a claims listing modelled on `undo::referenced` (`undo.rs:157-179`); re-scope `abort_orphaned_uploads` (`sweep.rs:43-56`) from key-prefix-plus-single-process to an age filter on `PendingUpload.initiated_unix` | **~200 rewritten** |
| **`fold.rs`** (1,207) | one leased compactor; each shard swaps its pack list inside its own CAS under its own coverage check (`fold.rs:773-811`, which tests roll-up membership and is already conservative enough to survive) | ~250 |
| **`lease.rs`** (425) | per-shard leases, plus a **partial-handoff protocol that does not exist**: the lease is whole-repository (`lib.rs:347`) and takeover rotates the whole snapshot | ~300 |
| **`lib.rs:310-347`** | every key function (`git_prefix`, `pack_prefix`, `snapshot_key`, `log_prefix`, `claim_key`) parameterised by shard; ~30 call sites | ~120 |
| **`snapshot.rs` / `log.rs` / `restore.rs` / `follow.rs`** | per-shard instances; the union follower | ~250 |
| **operator** (`crd.rs` 581, `render.rs` 1,160, `reconcile.rs` 625, `idle.rs` 313) | shard map in spec + status; S Deployments/Services/ConfigMaps against a module whose first line is "a ConfigMap, a headless Service, and a Deployment of one pod"; `replicas_for` returns 0 or 1 by construction; per-shard idle clock and wake | ~600 |
| **formal model** | `ForgeSync.tla` asserts the single-writer invariant everything rests on. Not optional. | new |

**Total ≈ 3,200–4,500 lines across four crates plus a new TLA+ model.**

**The first thing to build is not the splitter.** It is `sweep.rs` under S snapshots — the claims-listing reference set and the re-scoped MPU abort — with a falsifier that *deletes* if the rule is wrong. Every other piece is plumbing whose failure mode is a broken push; this one's failure mode is a repository that `restore.rs:115` refuses to serve forever. If the claims scheme cannot be shown sound with a decoy shard, a control arm, and a sweep past the event, stop there.

---

## 5. THE TRIGGER AND THE MEASUREMENT

Defer behind **all four** of the following, measured, on one repository:

1. **Sustained ≥ 8 acked pushes/s of *distinct-ref* traffic that the batcher cannot absorb** — i.e. the mean batch size has fallen below 8 while the queue is non-empty. Measure from the batch log: pushes per CAS and CAS/s over a 10-minute window. Today: 23–30 per CAS, 0.52–0.67 CAS/s. **Trigger at ≥ 8 CAS/s sustained — a 12–15× rise.**
2. **P2 median push latency > 5 s with the CAS on the critical path**, attributed by the latency rig's own breakdown (`forge/e2e/latency/`), not inferred. Today: 1.75 s.
3. **412 rate on the snapshot CAS > 5%** — direct evidence of contention on the mutable object, not of load. Today: effectively zero; the writer is alone.
4. **More repositories has been tried and rejected for a stated reason** that is not "one URL would be nicer" — e.g. a merge policy that genuinely must span the branches (`refs/for/*` targeting refs in two would-be shards), which sharding *also* cannot serve without full replication.

Two counter-triggers that should redirect the effort instead:

- **If the pressure is refs, not pushes** — the 545,613-byte whole-snapshot rewrite at 8,000 refs, or the ref-advertisement decay already measured at 63% of the rate loss — the answer is the one Google reached at 1.8M refs: **a better write format** (a delta stack, O(size_of_update)) and a shrunk advertisement (`uploadpack.allowFilter`, which gitqual found unset; Azure's "Limited Refs" above 10,000). Not more writers.
- **If the pressure is bytes** — 47.8 GB vs walgit's 16.5, the fold's whole-repo re-upload every ~24 pushes — sharding makes it S× worse. That is the `fold.rs` follow-up queue, not this.

**Record this refusal as: the partition does not partition the objects (thin packs, `for-each-ref`, `judge_merge`); `sweep.rs:43-56`'s prefix scope is inherited unmodified from the multi-writer refusal; the shard map is immutable for the life of the repository; and the constraint is at 2–5% utilisation while the largest git monorepo on earth runs at 0.6% of forge's measured rate.**
