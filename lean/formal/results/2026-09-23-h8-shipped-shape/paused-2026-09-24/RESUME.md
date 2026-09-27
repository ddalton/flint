# Paused 2026-09-24 ~05:21 UTC — box `ddalton@10.0.0.249` shut down for the night

Two runs were stopped deliberately, each on a completed checkpoint, with no
violation in either. The logs here are the box's logs at the pause (each run's
wrapper overwrites its log on a resume, so these are the record).

| run | world | model (md5) | stopped at | checkpoint | resume with |
|---|---|---|---|---|---|
| r34 | `LeanImmutableRenameHolds` (3 paths) on `LeanSubtree.tla` | `d2dd061a…`, cfg `b50442b3…` | 1,465,616,138 distinct, depth 30, queue 96,154,940, 0 violations | `/mnt/nvme2/tlc-r27/26-09-22-18-50-35`, 05:20:35 (stopped 6 s after it: nothing lost) | `~/r52-resume-r34.sh` |
| r51 | `LeanCoreHolds` (3 paths) + `Prop_NoSilentRevert`, `-continue` | `LeanCore.tla` `83c17abf…`, cfg `682816ca…` | 308,012,187 distinct, depth 21, queue 94,432,295, 0 violations | `/mnt/nvme/tlc-r51/LeanCoreHolds/26-09-23-23-36-55`, 05:07:08 (about 9 min of work lost) | `~/r53-resume-r51.sh` |

**Soundness fingerprint.** Before trusting a resumed run, check that its
`Recovery completed` line matches the checkpoint. For r34/r52 that is about
1,465.6M states examined and about 96.15M on the queue. For r51/r53 it is about
299.7M examined and about 93.6M on the queue (Progress(21) at 05:07:08:
299,727,934 distinct, queue 93,601,035). If it doesn't match, stop the run.

**Do not run `~/r51.sh`.** It does `rm -rf` on the r51 metadir. On the box it now
has no execute permission and carries a DO NOT RUN header.

**Resource plan when resuming.**
- r52 (8 workers, 12 GB heap, full priority) and r53 (2 workers, 5 GB heap,
  nice 19) share the 8-core, 30 GB box, as before the pause.
- Launch each one detached: `nohup ~/r5X-….sh >/dev/null 2>&1 </dev/null &`.
- Check each one's `Recovery completed` line against the fingerprint above.

**Disk at the pause:** r34's metadir is 194 GB on `/mnt/nvme2` (594 GB free);
r51's is 50 GB on `/mnt/nvme` (698 GB free).

**Rates before the pause:** r34 about 0.8M distinct/min at depth 30, queue
shrinking slowly. r51 about 0.9–1.2M distinct/min at depth 21, queue still
growing: this world has never finished (the last earlier attempt reached
120M at depth 19).

**Waiting on r34:** the two model patches in `lean/formal/pending/`
(narrow-over-handles, sync-queue L-123). Don't edit `LeanSubtree.tla` in the box
gate tree while r34/r52 exists.
