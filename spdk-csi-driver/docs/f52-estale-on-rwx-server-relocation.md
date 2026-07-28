# F52 — relocating an RWX NFS server ESTALEs live clients: PostgreSQL PANICs and takes ~5 min to come back

**Status:** **FIX IMPLEMENTED** 2026-07-27 (see §5; 884 tests green; live
validation pending — 3.6f check (h) is the gate), root cause CONFIRMED —
see §4. Found live
2026-07-27 on runai (driver `1.21.0-rc2`) by the brand-new drill **3.6f**.
This is the `db FAIL` on that run — every F49/F47 assertion the drill exists
to test **passed**; the failure is downstream of them. One line: *the v4
kernel-filehandle resolver derives the file's path from
`readlink(/proc/self/fd)`, and on the relocated server's cold dcache the
kernel names every disconnected dentry `"/"` — so WRITEs open the export
container's root (EISDIR→EIO) and GETATTR serves the root directory's
attributes as if they were the file's (client-side ESTALE), with zero
server-side errors.*

**Severity: availability, not durability.** Zero acknowledged data loss (the
ledger check found **all 1208 acked writes present**), `pg_amcheck` **clean**,
writability probe fine, and recovery was fully automatic with no manual repair.
What it costs is **~5m15s of database outage** for what is supposed to be a
routine RWX operation.

> **Reproduced on flint's own code path — this is not a drill artifact.**
> Later the same day, drill **3.6e** (F43 acceptance: terminate a remote leg
> node) hit the identical signature *without anyone forcing a pod move*. Its
> **cutover** — the RWX replacement-admission path — bounces the NFS server by
> design, and the scheduler landed the new instance on a different node
> (aws-2 → aws-4):
>
> ```
> 23:05:38.485 PANIC: could not fdatasync file "0000000100000002000000A6": Input/output error
> 23:05:38.488 ERROR: could not seek to end of file "global/1262": Stale file handle
> ```
>
> Same WAL-fdatasync EIO, same `global/1262` ESTALE loop. **So this fires on
> the normal RWX self-heal path after any node loss, not only when a drill
> forces a relocation.** That is the real severity: every RWX replacement
> admission PANICs an attached database. 3.6e still passed its F43 assertions
> (in_sync at 422s, 2/2 restored) — the redundancy machinery works; it just
> takes the client's database down while doing it.

## 1. Timeline (T0 = 22:41:16Z, the drill deletes the NFS server pod)

| time | event |
|---|---|
| 22:41:16 | server pod deleted; all workers but the target leg node are cordoned |
| 22:41:34 | new server **up on aws-2**, listening, same filehandle Instance ID |
| 22:41:37.674 | client: `could not seek to end of file "global/1262": Stale file handle` |
| 22:41:37.688 | **`PANIC: could not fdatasync file "000000010000000100000069": Input/output error`** (a WAL segment) |
| 22:41:37 → 22:45:47 | **251 consecutive ESTALEs**, pid 91 on `global/1262`, once per second |
| 22:45:48.025 | `all server processes terminated; reinitializing` |
| 22:45:55.9 | `database system was not properly shut down; automatic recovery in progress` |
| 22:45:55.97 → 22:46:14.5 | redo (18.6 s) |
| 22:46:30.5 | `database system is ready to accept connections` |
| 22:46:40 | pg-0 pod Ready (T0+324 s) |

The PANIC is PostgreSQL behaving *correctly*: since the 2018 "fsyncgate"
change it will not retry a failed `fdatasync`, because the kernel may have
already dropped the dirty pages. It panics and crash-recovers instead. So the
EIO is the finding, not the PANIC.

**The ESTALE loop is what makes this a 5-minute outage rather than a 30-second
one.** The postmaster wanted to reinitialize at 22:41:37, but could not declare
`all server processes terminated` until 22:45:48 — 4m10s later — because pid 91
(the autovacuum launcher, polling `pg_database`) was stuck retrying a stale
filehandle once per second. Storage-side ESTALE dominated the recovery time.

## 1b. Second occurrence (drill 3.6e, cutover-triggered)

| time | event |
|---|---|
| 22:58:37 | T0 — aws-1 (remote leg node) terminated |
| 23:02:07 | identity swapped to aws-4 (T0+210s) |
| 23:03:10 | replacement at warm standby (T0+273s) |
| 23:05:08 | `CutoverStarted` — **cutover bounces the NFS server**, new instance scheduled on aws-4 (T0+391s) |
| 23:05:38.485 | `PANIC: could not fdatasync … Input/output error` on the live client |
| 23:05:38 → … | `global/1262` ESTALE loop, once per second, same as §1 |
| 23:05:59 | replacement leg `in_sync` — 2/2 restored (T0+422s) |

The client here was pg-0 on aws-4, which had been deliberately moved off the
kill target beforehand so the drill measured pure storage loss. It was never
touched by the drill — the only thing that happened to it was that its NFS
server moved out from under it.

**3.6e nevertheless recorded `db=PASS`, and that PASS is hollow.** Kubelet
evicted pg-0 for DiskPressure at ~23:10 and it rescheduled at 23:11:12Z;
`verify-db.sh` greps only the *current* pod instance's log, so by the time it
ran, the 23:05:38 PANIC had been erased with the old container. The PANIC is
real — it is quoted above from a live read taken before the eviction. Any
`db=PASS` on a drill whose pod restarted must be treated as **unproven, not
clean** until the harness reads `--previous` too (tracked separately).

## 2. What it is NOT

- **Not a filehandle-namespace change.** The `FileHandleManager` Instance ID is
  **identical** before and after the move (`15422587554401453657` on both aws-1
  and aws-2), and both servers log `v4 kernel filehandles ACTIVE (probe
  passed)`. My first hypothesis — that relocation re-mints the filehandle
  namespace — is disproved by that pair of log lines.
- **Not a soft-mount artifact.** The client mount is `hard` (verified in
  `/proc/mounts`: `vers=4.2,hard,proto=tcp,timeo=600,retrans=2`). `hard`
  protects against *timeouts*, and would have blocked. It does not protect
  against an explicit `NFS4ERR_STALE`, which is a fatal answer, not a retryable
  one.
- **Not a build regression.** Control: drill **3.11 on the same cluster, same
  rc2 image, same harness, minutes earlier** — which does everything except
  move the server — recorded `db=PASS` with zero corruption-pattern lines. The
  trigger is the relocation, not the build.

## 3. Is it a regression from the F47/F48/F49 wave?

No, and the reason is structural: **this vector was unreachable before the F49
fix.** Pre-fix, an NFS server landing on a node that hosts one of its own legs
could not assemble the raid at all — it looped on `bdev_raid_create …
Operation not permitted` and never became Ready (that *is* F49). A client could
therefore never reach a relocated, serving instance to be ESTALEd by it.

The honest limit on that claim: I cannot A/B 3.6f against rc1, because rc1
fails earlier for exactly that reason, so no pre-wave baseline for this vector
exists or can exist. The claim is "newly **reachable**, not newly broken" —
the same pattern as F44 → F45 → F46 and F48 → F50.

## 4. Root cause — CONFIRMED 2026-07-27

**`KernelFh::resolve` trusts the kernel's name for a *disconnected* dentry.**
The v4 kernel-filehandle scheme (F26 §12, `fh_kernel.rs`) resolves a handle by
`open_by_handle_at(mount_fd, kh)` and then **derives the path from
`readlink("/proc/self/fd/<fd>")`**. On a freshly-mounted filesystem the dcache
is cold: `open_by_handle_at` (which passes *no* acceptability callback —
`fs/fhandle.c` accepts any alias) finds the inode fine but materializes it as a
**disconnected dentry** via `d_obtain_alias()`, and the kernel's name for such
a dentry is literally **`"/"`**. So `resolve()` returns `Ok("/")` — *success,
with garbage* — for every file the new server process has never looked up by
path.

Empirically proven in `repro/` (privileged container, loopback ext4): mint a
handle, resolve **warm** → the true path; umount + remount the same image and
resolve **cold** → `"/"`, while `fstat` on the very same fd shows the correct
inode and a `/proc/self/fd` reopen reads the correct content. One path lookup
of the file reconnects the dentry and resolution is correct again.

From that single lie, both observed symptoms follow — and neither involves the
server ever *answering* STALE (`nfs-server-new-aws2.log` has **zero** logged
resolve failures and zero STALE answers; the server believed it was healthy
throughout):

- **The PANIC (EIO path):** the client reconnects, CREATE_SESSION at
  22:41:37.669, and replays its dirty writeback with its old stateids — which
  **validate**, because open/lock state is persisted in the on-volume sqlite.
  `handle_write` resolves the fh to `"/"`, the fd cache of the fresh process
  is empty, so it runs `open("/", O_RDWR|O_CREATE)` → **EISDIR** → returns
  `NFS4ERR_IO`. **5,501** `WRITE: Failed to open file "/": Is a directory`
  lines in ~1 s (22:41:37.67→22:41:38). The client latches the writeback error
  and `fdatasync` on the WAL segment returns EIO → postgres PANICs (correct
  fsyncgate behavior).
- **The 251 s ESTALE loop (client-minted, server-silent):** `handle_getattr`
  resolves the fh to `"/"`, happily `stat`s the **container root** and returns
  *directory* attributes with a foreign fileid and `NFS4_OK`. The Linux client
  holds a cached *regular-file* inode for that fh; a type/fileid flip marks
  the inode stale **client-side**, so `lseek(SEEK_END)` (which forces a size
  revalidation) fails ESTALE. The autovacuum launcher retried once per second
  for 4m10s, each retry getting the same well-formed wrong answer — which is
  why the server log is silent for the whole window and why the loop ran far
  past the 90 s grace. Recovery after the PANIC is total because crash
  recovery re-opens everything **by name**: LOOKUPs connect the dentries and
  the pathology evaporates.

**Why the startup probe passes on every boot, including broken ones:**
`KernelFh::try_new` mints and immediately resolves `.flint-nfs/fh.key` — the
mint's own path lookup has just connected that dentry, so the probe never
exercises a cold-cache resolve. It has a structural blind spot for exactly
this failure.

**Why five weeks of drills never caught it:** kernel filehandles shipped
2026-07-19 (`3aef632`, v1.18.0). Drill 3.2 restarts the server pod **on the
same node** (`EXPECT_RESCHEDULE=none`) — kubelet's *staging* mount of the
backing PVC survives a same-node recreate, the ext4 superblock and its dcache
stay warm, and handles keep resolving (runz 07-21: "zero ESTALE" — genuinely).
Only a **cross-node** relocation stages a fresh mount, which is always cold;
no drill forced one until 3.6f, and the F49 EPERM squatter made the
forced-onto-leg-node variant unreachable besides. Both runai hits crossed
nodes (aws-1→aws-2 forced by cordons; aws-2→aws-4 by the scheduler after node
loss). Caveat on the prior record: a same-node cutover bounce shows nothing,
and runag 3.6e's `db=PASS` had its own pg-0 eviction — with the
`verify-db.sh --previous` gap, a cross-node bounce there could have been
masked; unverifiable now, the cluster is gone.

Disposition of §4's original hypotheses: (1) was the right syscall
neighborhood but the wrong failure mode — `open_by_handle_at` *succeeds*; it
is the path derivation that lies. (2) is moot — grace cannot help, because
from the server's view nothing ever failed. (3) is real but downstream — the
client-side inode staleness from the type flip is what pins cached fds until
process death.

Note this is exactly the trap knfsd avoids by construction: nfsd demands
connected dentries (`reconnect_path`) and serves I/O on the resolved
inode/fd, never on a re-derived path string.

## 5. The fix (implemented 2026-07-27, entirely inside `fh_kernel.rs`)

Chosen shape: **trust gate + inode-identity recovery + startup prewarm**, not
the full fd-based-serving refactor — same correctness for every reachable
case, without rewriting 4k lines of path-centric operation handlers. (An
untrusted resolution names a live file *somewhere* under the export or it
doesn't; identity recovery finds it if it does, STALE if it doesn't — the
only cases fd-serving would additionally rescue are unlinked-but-open files,
which the F17b/c open-fd fallbacks already own.)

1. **Trust gate** (`trusted_resolution`): `resolve()` now `fstat`s the
   handle's fd — `(st_dev, st_ino)` is ground truth — and only returns the
   readlink path if it lies under the export root **and** lstats back to
   that same identity. `"/"` (the disconnected-dentry name), foreign paths,
   `"(deleted)"` suffixes, and since-renamed paths all fail the gate.
2. **Identity recovery** (`IdentityResolver`): an untrusted resolution is
   re-located by inode identity via a bounded walk of the export tree, which
   as a side effect *reconnects dentries as it goes* — one walk heals the
   whole replay storm. A cached ino→path index (cap `FLINT_FH_IDENT_MAX`,
   default 200k entries; 0 = targeted early-exit walks only) serves
   concurrent cold resolves from a single walk; a stale index **hit** (file
   renamed since the walk) forces an immediate re-walk — postgres's
   write-temp-then-rename pattern must never STALE — while a **miss**
   against a fresh complete index is authoritative (1 s cooldown, so unlink
   storms don't walk per probe).
3. **Startup prewarm**: `FileHandleManager` builds the index right after the
   kernel-handle probe passes, *before the listener accepts* — so relocation
   replay traffic finds every dentry already connected and the cold path is
   never even entered. Logged (`🔥 fh identity index prewarmed`), and drill
   3.6f asserts the marker.
4. **Belt**: if recovery finds nothing, answer `Stale` — visible, WARN-logged
   (the original incident was invisible server-side precisely because this
   path never spoke), and the F17b/c open-fd fallbacks still get their turn.
   A foreign path is never returned to a caller under any outcome.

Perf cost: one `fstat` + one `lstat` per resolve on the warm path (≈µs
against 3 pre-existing syscalls); walks only on cold/untrusted resolves.

Tests: 7 new unit tests (all-platform — the identity machinery is pure
`std::fs`) covering the trust gate against the exact `"/"` shape, index
build/cap, rename-invalidation-during-cooldown, unlink→None, cap=0 targeted
mode, and env parsing; the lima Linux e2e gains `resolve_as_if_disconnected`
assertions (forced recovery finds the true path, follows renames, STALEs on
unlink). Full lib suite: **884 green**. The privileged-container repro
(`repro/`, phases 4–6) proves the exact algorithm against a *genuinely* cold
dcache: `readlink "/" UNTRUSTED → recovered by identity walk → true path`;
warm path untouched; unlinked → STALE.

Residual risk, documented: an export larger than the index cap falls back to
targeted walks per cold resolve (budget 2M entries) — raise
`FLINT_FH_IDENT_MAX` for huge exports; and a dentry evicted under memory
pressure *after* prewarm re-enters the recovery path (correctly, just
slower). Neither affects the flint RWX shape (one backing PVC per volume,
typically a database datadir).

## 6. Drill gate — DONE

3.6f check (h) (added 2026-07-27): **zero** `Stale file handle` and **zero**
`PANIC` lines in the client's postgres log across the relocation — reading
BOTH container instances (`--previous` too; an eviction mid-drill erased the
PANIC on runai 3.6e and minted a hollow `db=PASS`) — plus the new server's
`fh identity index prewarmed` marker. Either signature count non-zero is a
hard drill FAIL. The counts ride the drill NOTES (`f52_estale=`,
`f52_panic=`) so the relocation's client-visible outage is a recorded
number, not an anecdote.

Evidence: `tests/chaos/artifacts/f52-estale-server-relocation-runai/` —
postgres log across the whole event, both NFS server logs (old aws-1 and new
aws-2, showing the identical Instance ID), the client's `/proc/mounts` options,
the db verdict, and the drill log. `repro/` in the same directory holds the
root-cause reproducer (`fhtest.c` + `run.sh` + output): mint → remount →
resolve answers `"/"` on a cold dcache while the fd stays correct.

Related: [F49](f49-local-leg-export-squatter.md) (the fix that made this
vector reachable), [F50](f50-hotrejoin-window-concurrency.md),
[F51](f51-deletevolume-lvol-leak.md).
