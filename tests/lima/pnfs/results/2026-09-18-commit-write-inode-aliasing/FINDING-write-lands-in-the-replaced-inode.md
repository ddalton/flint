# A WRITE lands in the REPLACED inode, and the client is told it succeeded

**2026-09-18 · 10.0.0.249, Ubuntu kernel 6.12.0 · standalone `flint-pnfs-mds`
at `0d5808c7` · real kernel NFSv4.1 mount, traced with bpftrace**

## Result

A client replaces a file at a path it still holds open. Every subsequent write
to the NEW file lands in the REPLACED inode. `write()` and `fsync()` both return
success. The file at that path stays empty.

```
ino_A (replaced) = 36175893      ino_B (current) = 36175894
client's view of data.db         = 36175894        <- the client is NOT stale
client: write 4096 B-bytes + fsync -> SUCCESS

data.db  (ino_B)  size=0                    <- the file at the path is EMPTY
oldlink  (ino_A)  size=4096  first=BBBB     <- the bytes are in the replaced inode
```

**No crash is required.** This is not a durability window; the bytes never reach
the file the client addressed.

## It is the server, not the client

The obvious alternative was a stale filehandle in the client's attribute cache,
which would make this correct server behaviour. It is not: `stat` through the
mount reports `36175894`, the new inode. The client addressed the right file.
The server wrote to the wrong one (`drill3.sh` asserts this before writing).

## Mechanism, every link confirmed in the server log

1. The client holds `data.db` (ino_A) open. Cache entry: `{path: data.db,
   ino: A, fd -> A}`.
2. The client replaces the file. Because ino_A is still open, the kernel client
   **silly-renames**:
   ```
   RENAME: data.db -> .nfs000000000228001500000004
   RENAME: new.tmp -> data.db
   ```
3. **Nothing evicts a cache entry on rename** — `evict_by_path`/`evict_by_ino`
   have zero production callers. The entry still claims `path = data.db` while
   ino_A now lives under the `.nfs*` name.
4. The client OPENs the new `data.db` (CLAIM_FH). A **fresh** stateid is minted,
   and `seed_open_fd` (`ioops.rs:1733`) calls `find_by_path("data.db")`, gets the
   STALE entry, and seeds the new stateid with it:
   ```rust
   if let Some(existing) = self.fd_cache.find_by_path(path, false) {
       self.fd_cache.insert(stateid.other, CachedFile {
           file: existing.file,        // shares the OLD descriptor
           path: path.clone(),         // records the NEW path
           ino:  existing.ino,         // keeps the OLD inode
   ```
   The resulting entry is **internally inconsistent**: its path names ino_B, its
   ino and descriptor are ino_A.
5. `WRITE` hits `get(stateid).filter(|e| e.path == path)`. The path matches —
   step 4 wrote the new path onto a stale descriptor — so the guard passes and
   4096 bytes land in ino_A.
6. `COMMIT` calls `find_by_path` (`ioops.rs:3001`), which never re-checks the
   inode, and fsyncs ino_A. Traced with bpftrace on `vfs_fsync_range`: the
   fsync lands on ino_A and **ino_B is never fsynced at all**.

## Root cause

`seed_open_fd` shares a descriptor **by path with no inode check** — the exact
defect `adopt_open_fd` was rewritten to close on 2026-09-16. That wave hardened
the copies and left the original. Its own comment says so, as reassurance:

> `seed_open_fd` has always done this at OPEN time; the two hottest paths
> simply never did.

A path is not an identity. See `feedback_a_path_is_not_an_identity`.

## What knfsd does, and we do not

Three independent mechanisms in knfsd each stop this on its own:

| knfsd | where | flint |
|---|---|---|
| filecache hashed on `nf_inode` from the filehandle | `filecache.c:100` | keyed on stateid, path used as the check |
| `nfs4_check_fh()` — `fh_match` or `nfserr_bad_stateid` | `nfs4state.c:6304` | no stateid-to-filehandle binding |
| fsnotify `FS_ATTRIB\|FS_DELETE_SELF` -> `nfsd_file_close_inode` | `filecache.c:188` | `evict_by_*` exist, never called |

(Read from torvalds/linux v6.8.)

## Trigger, stated precisely

A file replaced at a path some client still holds open — log rotation, config
reload, deploy-over-in-place, a sqlite reader during a checkpoint. It is the
holding-open that produces the silly-rename and leaves the stale entry behind.
Without an open holder the client sends CLOSE and the entry is removed.

## Reproducing

- `commit-drill.sh` — bpftrace on the server's fsync path; shows COMMIT fsyncing
  the replaced inode and never the current one. Carries a no-replace CONTROL leg.
- `drill2.sh` — keeps the replaced inode reachable via a hardlink and reads the
  bytes back: they are in ino_A.
- `drill3.sh` — proves the CLIENT's view is correct before writing, so the defect
  cannot be attributed to client caching. Dumps the server debug log.

All three assert `stat -f -c %T` is really `nfs4` before measuring, and declare
INCONCLUSIVE if ext4 happens to reuse the inode number.

`proof-fd_cache.patch` / `proof-ioops.patch` are the in-tree unit proofs,
including the positive control that the same lookup is correct with no stale
entry present.
