# The hub never expired a departed client, so it never gave its descriptors back

**Found:** 2026-09-16, by the first SQLite *concurrency* drill
(`tests/lima/pnfs/sqlite-concurrency-drill.sh`) on real EC2, two
separate kernel NFS clients, two pure-spot instances in us-west-1a.

**Status: root-caused and fixed, verified by measurement.**

## The chain, and how each link was established

| link | how it was established |
|---|---|
| ~2 descriptors accumulate per sqlite transaction | measured: 18 → 120 over 50 txns = **2.04/txn**, single client |
| they are in `FdCache` | **plateau test**: with the cache capped at 64, 200 txns held 80 fds instead of the ~426 the rate predicts |
| they sit under OPEN stateids, not lock stateids | **key histogram**: `lock_stateids=0 other_stateids=48 entries_with_ino_0=0` |
| retiring the state DOES release them | the new `fd_release` hook fired **122** times |
| but state is never retired when a client leaves | fds held at 98 for **160s** after unmount; last lease line was a *renewal* |
| because expiry is TRAFFIC-DRIVEN | `courtesy_release_expired`'s only production caller was the top of every COMPOUND — its own comment: "it is the other cluster's traffic that releases this cluster's locks" |
| a reference server does not behave this way | **knfsd `/proc/fs/nfsd/filecache` over the same workload: acquisitions 660, releases 659, total inodes 0, lru entries 0** |

Three hypotheses were wrong before the right one, and the measurement
refused each in turn — the rate stayed at exactly 2.04 through two
"fixes":

1. *unlink strands descriptors* — refuted: 40 create+delete cycles
   leaked zero;
2. *cached under LOCK stateids* — refuted twice (rate unmoved; then the
   histogram showed `lock_stateids=0`);
3. *`seed_open_fd`'s residual, unreleased because nothing expires the
   client* — matches every measurement. Its own comment had named it:
   "fds of clients that die without CLOSE outlive the state entries
   (**lease sweep doesn't reach this cache yet**)."

## Why conformance never caught it

pynfs and pjdfstest test PROTOCOL correctness. No conformance suite
asserts "the server's descriptor count returns to baseline after CLOSE"
— that is a resource-lifecycle property and is invisible to them by
construction. The scale is also wrong: ~500 sustained transactions are
needed to reach 1024, and the only sqlite the tree ever ran was
`tier-drill.sh:245` — one process, TWO ROWS. Conformance proves you
speak the protocol; a drill proves you survive using it.

## What it cost before the fix

Every operation returned EMFILE once `RLIMIT_NOFILE` was reached — and
the F33 watchdog, seeing its own backing-store probe fail with EMFILE,
diagnosed a dead disk and **killed the server**:

```
ERROR [FENCE] backing store unresponsive past deadline — fencing (F33) stale_secs=99
ERROR [FENCE] all sockets shut down — clients fail over now ... exit_code=59
```

The export was a healthy local ext4 directory throughout.

## The fix, and what each part is for

1. **The laundromat** (`pnfs/mds/server.rs`) — traffic-independent lease
   expiry every 30s. THIS IS THE CURE. knfsd has exactly this; flint had
   every other link in the chain and not the timer.
2. **`fd_release` hook on `StateIdManager`**, called from
   `remove_master` — the common tail of both close paths, so a removal
   path added later inherits it rather than silently leaking.
   `delegation.rs` already had this hook for the delegation half of the
   same bug; this is the open-stateid half.
3. **`FdCache` bounded, LRU reap** — containment for any leak not yet
   found. Ganesha's model (percentage of rlimit); knfsd's memory
   shrinker is not available to a userspace server.
4. **`raise_fd_limit()`** — soft to hard at startup. The drill host's
   hard limit was 1,048,576 while the server sat at the inherited 1,024.
5. **EMFILE is no longer fatal**: `fence.rs` treats it as INCONCLUSIVE
   (it says we have no descriptor to probe WITH — not evidence about the
   disk), and `io_error_to_nfs4` answers `NFS4ERR_DELAY`, which clients
   retry silently. knfsd returns nfserr_jukebox; Ganesha denies above
   `FD_Limit_Percent`; neither self-terminates.

## Verified

| | fds |
|---|---|
| baseline, idle and mounted | 16 |
| after 60 transactions | 139 |
| +100s after unmount | 138 |
| **+120s after unmount** | **17** |

`laundromat: retired expired client state clients=1`. 120s is the design
point: a 90s lease plus at most one 30s sweep. Matches knfsd's `held 0`.

## The follow-up this does NOT do

knfsd does not notify a side cache at all: the file lives INSIDE the
state object (`nfs4_file->fi_fds[]`, refcounted by `fi_access[]`), so
dropping the state drops the reference automatically —
`expire_client` → `release_openowner` → `release_open_stateid` →
`release_all_access` → `nfsd_file_put`.

flint keys a SIDE CACHE by stateid, which stays correct only while every
removal path remembers to announce itself. That is how this bug happened,
and it has now happened twice (delegation, then open stateids). A design
needing a new hook per stateid flavour will need a third.

**Recommended: make the state OWN the descriptor** — hold the
`Arc<File>` in the state entry so `Drop` closes it when the state dies.
That retires both hooks and the whole bug class. Rust's RAII makes this
cleaner than the C refcount it imitates. Not done here; it is a refactor,
not a patch.
