# A mounted client that keeps working keeps accumulating descriptors

**2026-09-16 · c6i.2xlarge spot, us-west-1c · Ubuntu 24.04, kernel NFSv4.1 client · hub at HEAD (`f482369f`)**

## What was being proved

The descriptor fix (`0cbf45a1` laundromat, `f482369f` state-owns-the-descriptor)
was verified against a 60-transaction drill: 16 → 139 → 17. That was taken as
"the leak is fixed". This run asked whether 60 transactions was long enough to
see the thing it claimed to bound.

It was not.

## Result

**On a live, continuously mounted client, descriptors grow at exactly +2 per
SQLite transaction and are never released — with the fix in.**

| arm | build | txns | fds start → end | delta |
|---|---|---|---|---|
| FIX | HEAD | 4000 | 62 → 8064 | **+8002** |
| NOFIX | HEAD, fix removed | 4000 | 17 → 8021 | **+8004** |

Dead linear, no plateau, and the control with the fix surgically removed
(`mknofix.sh`) is **statistically identical**. On this path the fix does
nothing.

All 4000 rows landed in both arms, zero insert failures. This is a resource
leak, not a correctness bug.

## It is the server, not the client

The obvious alternative was that the NFSv4 client legitimately holds opens
across transactions, and any server would show this. It does not. Same client
kernel, same workload, 2000 transactions each, run interleaved
(`paired.sh`, `paired.txt`):

```
txn   flint_fds_on_db   knfsd_total_inodes
250        501                  2
1000      2001                  2
2000      4001                  2
```

knfsd finished with `acquisitions 7038 / releases 6970 / total inodes 2` —
balanced and flat. flint held 4003 descriptors **on the same single file**.
The identical workload from the identical client needs 2.

## What the fix actually fixed

The fix is real, but it covers the *departure* path only. Unmounting the
client:

```
before umount: 4021
  +60s: 4020
  +90s: 17        <- the 90s lease expires
laundromat retirements: 1
FD LEASE (RAII):  3982
```

The laundromat fires once at lease expiry and the RAII lease returns 3982
descriptors. That is exactly what the original 60-transaction drill measured,
and it works. What nothing covers is the client that *stays*: the lease is
continuously renewed, so nothing is ever expired, so the laundromat has
nothing to retire. Agents keep their mounts, so this is the path that matters
for a fleet.

## The bound holds, and eviction is safe

The LRU cap that shipped in the same wave is what stands between this and
EMFILE. Forced with `FLINT_FD_CACHE_MAX=512` (`soak-cap.txt`):

- 28 high-water reap events; fds tracked the cap (…523 → 407)
- **2000/2000 rows, 0 insert failures, 0 EMFILE / Delay / jukebox**

So when the LRU evicts a descriptor a live stateid still needs, the workload
still gets correct answers. At the shipped default the ceiling is
`hiwat=16384`, reached in ~8200 transactions, after which the hub sits pinned
at 16384 open descriptors for the life of the mount.

## It is not a general leak — it is repeated opens of one file

git, over the same mount, does not leak at all (`gitwork2.sh`):

```
fds before git: 4021 … at 40 commits: 4021 … delta=0, git fsck: CLEAN
```

git opens many distinct files once each and those are reaped. SQLite reopens
*the same* file under byte-range locks, and each OPEN mints a new stateid, a
new cache entry and a new descriptor on the same inode; CLOSE reaps its own
entry but opens outrun closes by 2 per transaction. Over one transaction the
server logged 17 OPEN against 12 CLOSE — the asymmetry noted and left
unexplained in the earlier drill was this defect's fingerprint.

## Status

Open. Not fixed by this wave. Contained by the capacity bound, which is
demonstrated safe but is doing all of the work.

The claim that should be made about the shipped code is: *a client that goes
away returns its descriptors; a client that stays accumulates them until the
LRU cap reclaims them, and correctness holds throughout.* Not "the leak is
fixed".

## Reproducing

`soak2.sh <binary> <tag>` (N, STEP env), `paired.sh` for the knfsd control,
`gitwork2.sh` for breadth, `mknofix.sh` to build the positive control.

---

# FIXED 2026-09-16 — Option A (adopt an fd this server already holds open)

`ioops.rs::adopt_open_fd`. READ and WRITE looked the cache up by the
PRESENTING STATEID alone and opened a fresh descriptor on a miss; they now
consult the inode index first and share the `Arc<File>` the entry already
carries. The `Arc` is the refcount — knfsd's `nf_ref` for free — so the
descriptor closes exactly when the last stateid referencing it goes away.
`seed_open_fd` had always done this at OPEN time; the two hottest paths
never did.

Measured on one c6i.2xlarge, both arms on the same box, 4000 transactions:

| arm | fds start → end | delta |
|---|---|---|
| **Option A** | 17 → 19 | **+2** |
| control (same tree, only the two wirings removed) | 17 → 8021 | +8004 |

Against knfsd, same client, interleaved, 1500 txns each: **flint 1 held
descriptor, knfsd 2.** 1500/1500 rows on both, zero failures.

Other arms, all on the fixed build: soak 4000 → `delta=0`, 4000/4000 rows;
git 40 commits → `delta=0`, `fsck CLEAN`; peer-close hazard drill → PASS
(a peer's CLOSE does not take a shared descriptor).

## The bug this nearly shipped with

The first cut adopted by `find_by_path`. The paired drill deleted
`paired.db` and recreated it at the same path; the stale entry was adopted,
every WRITE landed in the DELETED inode, and all 1500 transactions returned
"no such table" with the file on disk empty. **Silent data loss, out of a
cache hit.** A path is not an identity.

Adoption is now keyed on the inode the path names *right now*
(`find_by_ino(cur_ino)`), which is knfsd's key and cannot alias. Pinned by
`adoption_refuses_a_descriptor_for_a_file_that_was_replaced`, verified as a
positive control: it FAILS against the path-keyed version and passes with
the inode guard.

Suite: 2535 passed (the new test included). The one intermittent failure is
the pre-existing tier flake below, unrelated.

## Still open

- **The tier test flake.** Pre-existing (3 failures in 5 runs on an
  unpatched tree), needs parallelism (single-threaded 3/3 green,
  `--test-threads=2` 4/4 green), is NOT CPU contention (tier tests under a
  saturated CPU: 6/6 green), and needs the `nfs::` tests present (suite
  without them: 8/8 green). Victim moves between tier tests, always a
  marker/eviction assertion. Three real defects were fixed while chasing it
  — `_excl` declared before `orch` in `hydrate.rs` and `evict.rs` (lock
  released before the orchestrator tore down, contradicting their own
  comments), and 8 tests enabling process-global capture without
  `test_exclusive()` — but none resolved it. Mechanism not yet identified.
- **The credential gap.** `FdCache` has no cred/uid/client dimension;
  knfsd matches `nfsd_match_cred`. Pre-existing (`seed_open_fd` shares the
  same way) but adoption now applies it on the hot paths. Unresolved.
