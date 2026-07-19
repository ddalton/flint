# F26 — FileHandleManager cache re-architecture

Design analysis for the O(N)-scan-under-global-lock defect found in
`src/nfs/v4/filehandle.rs` (F26 in
`attach-detach-campaign-2026-07.md`). Status: **proposal, not
implemented.** No code changed by this document.

> **Revised after external research (2026-07-19).** §5 below is a
> sound *incremental* fix for the current path-based handle design and
> resolves F26. But the literature review in **§11** found that the
> current design is itself the root of the problem: production
> userspace NFS servers (Linux knfsd, NFS-Ganesha's FSAL_VFS) do **not**
> put paths in file handles at all — they encode the kernel's
> inode+generation handle via `name_to_handle_at(2)` /
> `open_by_handle_at(2)`, which is rename-stable *by construction* and
> makes the whole cache-maintenance problem (F26) — and retroactively
> F17 and F23 — disappear rather than merely run faster.
>
> **Decision (2026-07-19): go straight to the inode-handle
> architecture (§12); do not build the §5 path-based fix.** §5 is
> retained below as the F26 mechanism explanation and as the fallback
> shape if the §12 capability spike fails. There is no production fire
> forcing an interim patch, and §12 is a net-negative diff that retires
> the design smell behind the entire F17/F23/F24/F26 family. See §12
> "Recommended sequencing" for the capability-spike gate.

## 1. Problem recap

`note_fs_rename` and `note_fs_remove` — invoked on the RENAME
(`fileops.rs:3486`) and REMOVE (`fileops.rs:3304`) hot paths — each do
a full `.keys()` / `.iter()` scan of the filehandle caches while
holding **write** locks:

```rust
let dead: Vec<PathBuf> = p2h.keys()                     // O(N) over the WHOLE map
    .filter(|p| p.starts_with(old_path) || p.starts_with(new_path))
    .cloned().collect();                                // O(N) PathBuf heap allocs
```

The same locks are taken as **read** by `path_to_filehandle` and
`filehandle_to_path`, which run on essentially every op (GETATTR,
READ, WRITE, CLOSE, LOOKUP) on every connection. `path_to_handle`
grows unboundedly (one entry per distinct path ever handled; pruned
only per-subtree). Under a postgres workload — which renames
constantly (`pg_internal.init.<pid>` → `pg_internal.init`, WAL
recycling, temp files) — each rename holds a global write lock across
an O(N) scan and an O(N) allocation storm, stalling every concurrent
op on both connections. Measured signature: uniform ~50–200 ms/op
across all op types and both connections, 84 % system time, allocator
churn, worsening with runtime; a pod restart (which empties the
in-memory-only cache) restores 0.3 ms/op instantly.

## 2. Current structures — authoritative vs. cache

`FileHandleManager` holds five maps, each an `Arc<RwLock<HashMap>>`,
all process-global (one instance, shared by every connection):

| structure | contents | role | persisted? | reloaded on restart? | bounded? |
|---|---|---|---|---|---|
| `path_to_handle` | `PathBuf → Nfs4FileHandle` | **cache** | no | no | **no** |
| `handle_to_path` | `Vec<u8> → PathBuf` | **cache** | no | no | **no** |
| `path_to_id` / `id_to_path` | `PathBuf ↔ u64` (v2 long-path handles) | **authoritative** | yes | yes | no (but only long paths) |
| `rename_aliases` | `PathBuf → PathBuf` (F23) | **authoritative** | no | no | yes (`RENAME_ALIAS_CAP=8192`) |

### 2.1 Why `path_to_handle` and `handle_to_path` are *pure caches*

Handles are one of three self-describing formats:

- **v3** `[3][instance:8][hash:32][ino:8][len:2][path]` — the path is
  embedded; the identity hash is `SHA256(path ‖ instance_id ‖ ino)`.
- **v1** `[1][instance:8][hash:32][len:2][path]` — path embedded, no
  inode (only minted before the object exists; upgraded to v3 on the
  next lookup once it can be `lstat`'d — `filehandle.rs:277`).
- **v2** `[2][instance:8][id:8]` (17 bytes) — path **not** embedded;
  the id resolves through the authoritative `id_to_path` table. Only
  used when `FH_V3_MIN + path_len` exceeds `Nfs4FileHandle::MAX_SIZE`.

Three independent properties make the caches non-authoritative:

1. **Self-describing.** `parse_handle` recovers the full path from the
   handle bytes for v1/v3 with no map lookup (`filehandle.rs:622`).
   `handle_to_path` only saves the re-parse.
2. **Deterministic.** `generate_handle` is a pure function of
   `(path, instance_id, ino)`. Re-minting the same `(path, ino)`
   yields byte-identical handles, so **handle stability does not
   depend on `path_to_handle`.** `path_to_handle` only saves the
   SHA-256 + `lstat`.
3. **Self-verifying.** `filehandle_to_path` re-checks `current_ino`
   against the pinned inode on **every** resolve — cache hit or miss
   (`filehandle.rs:324`) — and either follows the rename alias (F23)
   or returns STALE. `path_to_filehandle` re-checks the inode on a
   cache hit and re-mints on generation change (`filehandle.rs:270`).

**Consequence:** a stale entry in either cache cannot cause a wrong
result — it is caught by the inode re-verification that runs anyway.
The proactive subtree eviction in `note_fs_rename`/`note_fs_remove`
over these two maps is therefore **defensive, not required for
correctness** (one caveat — v1 — in §5.2). The scans can be removed
outright; the maps can be bounded and evicted by any policy.

The only work that is *semantically* required on rename/remove is:

- **v2 table re-key** (`path_to_id`/`id_to_path`): a v2 handle's id
  must keep resolving to the file after it moves — this is a real
  mutation of authoritative state (and must be persisted).
- **rename alias insert** (`rename_aliases`): F23 — outstanding
  handles embedding the old path must follow the file.

Both touch small structures (v2 = long paths only; aliases capped),
so neither needs an O(N) scan of the *large* caches.

## 3. Access pattern (what postgres actually does)

- **Reads dominate.** Every op resolves a filehandle (reverse) and
  many mint one (forward). This is the hot path; it must be
  lock-light and never blocked by a writer.
- **Renames/removes are frequent but almost always leaf, short-path,
  v3.** `pg_internal.init.<pid> → pg_internal.init`, WAL segment
  recycling, temp-file cleanup. `starts_with(old_path)` on a leaf
  matches exactly one entry (the node itself). Directory renames
  (subtree re-key with k>0 descendants) are comparatively rare.
- **Distinct-path cardinality is large.** pgbench `-s 200` plus WAL
  and catalog churn touches thousands of paths, so N (cache size)
  climbs into the thousands — which is precisely what makes the
  current O(N) scans expensive.

The design must optimize the frequent case (leaf, short, v3) to O(1)
and keep the rare case (directory subtree) at worst O(log M + k).

## 4. Design goals & invariants to preserve

- **G1 — hot read path takes no global lock** for the v1/v3 case
  (≈100 % of postgres handles).
- **G2 — no O(N) work and no O(N) allocation on rename/remove.**
- **G3 — bounded memory** (no unbounded cache growth).
- **G4 — cross-connection isolation**: one connection's rename never
  stalls another connection's reads.
- **I1 — F17 preserved**: rename-over / remove+recreate returns STALE
  for the old generation, fresh handle for the new.
- **I2 — F23 preserved**: rename-away handles follow the file
  (ino-verified alias, chain-collapsed, capped).
- **I3 — v2 persistence preserved**: long-path handles survive
  restart and follow renames.
- **I4 — STALE vs. BADHANDLE semantics preserved** (`HandleError`):
  wrong-instance/absent ⇒ STALE (client re-walks); malformed ⇒
  BADHANDLE. (A regression here caused the 2026-06-12 errno-521 loop.)

## 5. Recommended design

Four independent changes, each shippable on its own, ordered by
value. Together they satisfy G1–G4 and preserve I1–I4.

### 5.1 Delete `handle_to_path` (reverse cache) — biggest win, lowest risk

`filehandle_to_path` becomes: v2 → `id_to_path` lookup; v1/v3 →
`parse_handle` (pure byte-slicing + one hash verify), then the
existing inode re-verification. The reverse resolve — the single
hottest operation — then takes **no global lock at all** for v1/v3.

- Cost added: a `parse_handle` per resolve (a few slices + one
  SHA-256 over the path). This is dwarfed by the `lstat` the inode
  check already performs, so net latency is unchanged.
- Removes one of the two maps the scans touched, and one global lock
  from the hot path. Satisfies **G1** for the reverse direction.

### 5.2 `path_to_handle` → bounded concurrent cache, no subtree eviction

Replace the unbounded `RwLock<HashMap>` with a **sharded, bounded**
cache (e.g. `quick_cache` or `moka::sync`, both lock-free readers with
internal sharding; either is a small, well-maintained dep — or a
hand-rolled N-way sharded LRU using the existing `dashmap`). Forward
mint becomes a shard-local get/insert: O(1), no global lock (**G1**
forward, **G4**). Bounding gives **G3**.

`note_fs_rename`/`note_fs_remove` stop scanning this map entirely.
Correctness holds via §2.1 (inode self-heal). **v1 caveat:** v1
handles carry no inode, so a stale v1 forward-cache entry for a
replaced object could hand out the old handle. Mitigations, in order:
(a) v1 is only minted pre-existence and upgraded to v3 on the next
lookup; (b) the bound caps the staleness window; (c) **belt-and-
suspenders**: keep an **O(1) point** eviction of the *exact*
`old_path` and `new_path` (not a subtree scan) in the two `note_fs_*`
functions. (c) preserves today's leaf-case semantics exactly at O(1)
and is what the design assumes.

### 5.3 v2 table → prefix-ordered structure for subtree re-key

The v2 re-key genuinely needs "every descendant of `old_path`." Back
`path_to_id` with a `BTreeMap<PathBuf, u64>` so a subtree is a
contiguous range:

```rust
// O(log M + k): k = long-path descendants (usually 0 for a leaf)
let victims: Vec<(PathBuf,u64)> = ids
    .range(old_path.to_path_buf()..)
    .take_while(|(p,_)| p.starts_with(old_path))
    .map(|(p,&id)| (p.clone(), id)).collect();
```

replacing the O(M) `.iter().filter()`. Keep `id_to_path` as a
`HashMap<u64,PathBuf>` for O(1) reverse. M is small (long paths only),
so even a single `RwLock` here is fine; the point is eliminating the
full-map scan (**G2**) and keeping the authoritative re-key correct
(**I3**).

### 5.4 `rename_aliases` → add a reverse index for O(1) chain-collapse

Chain-collapse currently scans all aliases
(`aliases.iter().filter(|(_,v)| v == old_path)`, `filehandle.rs:716`)
— O(A) per rename. Add `alias_rev: HashMap<PathBuf, HashSet<PathBuf>>`
(value → keys pointing at it) so collapse touches only the keys that
actually lead into `old_path`: O(deg). A is capped at 8192 so this is
a minor win, but it removes the last per-rename scan (**G2**).

### 5.5 Resulting complexity

| operation | current | proposed |
|---|---|---|
| resolve (reverse), v1/v3 | global read lock, O(1) map hit / O(len) parse; **blocked by any writer** | **no global lock**; O(len) parse + inode check |
| resolve (reverse), v2 | global read lock, O(1) | RwLock read, O(1) |
| mint (forward) | global read lock O(1) hit / global write lock O(1) miss | shard-local O(1) |
| `note_fs_rename` | **O(N)+O(M)+O(A) under write locks + O(N) allocs** | O(1) point-evict + O(log M + k) BTree range + O(deg) alias |
| `note_fs_remove` | **O(N)+O(M) under write locks** | O(1) point-evict + O(log M + k) BTree range |
| memory | O(N+M+A), **N unbounded** | O(cap + M + A), **bounded** |

The defining change: the hot read path no longer shares a lock with an
O(N) writer, and the writer is no longer O(N). Both the stall and its
growth-over-time disappear.

## 6. Correctness argument

- **I1 (F17)** — unchanged. STALE-on-replace comes from the inode
  re-verification in `filehandle_to_path`/`path_to_filehandle`, which
  is untouched. Removing the defensive cache eviction cannot weaken it
  (§2.1); the §5.2(c) point-evict preserves the v1 leaf case exactly.
- **I2 (F23)** — alias insert/follow logic unchanged; only chain-
  collapse is re-indexed (same result, fewer ops). The `MAX_HOPS` and
  `CAP` guards stay.
- **I3 (v2)** — the authoritative id↔path table and its persistence
  are preserved; only the container changes (`HashMap` → `BTreeMap`)
  and the re-key becomes a range instead of a scan. Same mappings,
  same `put_fh_mapping`/`delete_fh_mapping` calls.
- **I4 (STALE/BADHANDLE)** — `parse_handle`/`validate_handle` error
  mapping is unchanged; deleting `handle_to_path` only removes a
  fast-path shortcut before the identical parse.

## 7. Rollout plan (incremental, each independently revertible)

1. **§5.1 delete `handle_to_path`.** Smallest diff, removes a lock
   from the hot path and one scanned map. Ship + validate first.
2. **§5.2 bound `path_to_handle` + O(1) point-evict.** Removes the
   remaining O(N) scan and caps memory. This is the change that
   actually fixes F26.
3. **§5.3 BTreeMap v2 re-key** and **§5.4 alias reverse index.**
   Robustness for the directory-rename and alias-heavy tails; lower
   urgency (M and A are small/capped).

Steps 1–2 alone resolve the measured degradation; 3 hardens the tails.

## 8. Testing

- **Unit (correctness, preserve existing):** all current
  `filehandle.rs` tests must pass unchanged — `rename_over_stales…`,
  `stale_mint_cache…`, `removed_file_stales…`,
  `v3_handle_survives_restart…`, `rename_away_handle_follows_the_file`,
  `parse_path_lenient_understands_v3`. Add: resolve-after-`handle_to_path`
  -removal returns the same path; bounded cache eviction does not
  change resolve results; v1 point-evict stales a replaced v1 object;
  BTree subtree re-key moves exactly the descendants.
- **Perf regression (would have caught F26):** a test that inserts
  N=50 000 forward-cache entries, then times 1 000 `note_fs_rename`
  leaf calls under a wall-clock budget (e.g. assert p99 < 1 ms). On
  today's code this is O(N) per call and blows the budget; on the
  proposed design it is O(1). This is the mechanized guard analogous
  to the `no_iter_guards_in_scrutinees` lint from F24 — it turns "a
  reviewer must notice the scan" into "CI fails."
- **Concurrency:** spawn readers (resolve) on one thread and a
  rename/remove storm on another against a large cache; assert reader
  p99 latency is unaffected by writer activity (the cross-connection
  isolation goal, G4).

## 9. Alternatives considered & rejected

- **Keep the architecture, just make the scan O(1) for leaves.**
  (Point-evict + fall back to scan for directories.) Simplest patch,
  but leaves the global lock and unbounded growth (fails G1, G3, G4).
  Acceptable as an emergency hotfix, not the target design.
- **Make `path_to_handle` a `DashMap`.** Shards writes, but subtree
  eviction still needs iteration, and iterating a `DashMap` while
  mutating it is exactly the F24 shard-guard hazard. Sharding without
  removing the scan trades one deadlock class for another. Rejected.
- **Drop `path_to_handle` entirely** (always mint). Viable — minting
  is a sub-µs hash + an `lstat` the resolve already does — and it
  would be the simplest possible design. Rejected only because a
  bounded cache cheaply absorbs repeated LOOKUP of the same hot path
  (postgres re-opens the same relation files constantly); revisit if
  the bounded cache proves to add complexity for little hit-rate.

## 10. Open questions

- Cache dep choice: `quick_cache` vs `moka` vs a hand-rolled sharded
  LRU over the existing `dashmap`. Prefer the fewest new deps; a
  hand-rolled N-way `dashmap`-of-LRU may suffice and keeps the F24
  guard discipline in-tree.
- Is v1 reachable at all under the current mint path in production, or
  only in the pre-creation window? If the latter, §5.2(c) can be
  dropped and v1 relies solely on the v3 upgrade — simpler, one fewer
  branch on the rename path. Needs a quick audit of v1 mint sites
  before committing to that.

## 11. Literature review — is §5 the best approach?

External research (papers + production NFS server implementations)
says: **§5 is the right incremental fix, but not the best
architecture.** Three findings, in decreasing order of impact.

### 11.1 Production NFS servers don't put paths in handles at all

Both mainstream Linux NFS servers encode an **inode number +
generation number**, not a path:

- **Linux knfsd**: "NFS filehandles don't contain paths; they normally
  only contain roughly the inode number… identified by an inode number
  and a generation number." Handle→object resolution is the kernel's
  `exportfs`/reconnect path.
- **NFS-Ganesha FSAL_VFS**: "uses the `name_to_handle_at` and
  `open_by_handle_at` system calls" to translate name↔handle↔inode,
  wrapping the opaque kernel handle in a ~5-byte header (export id +
  fsid). Works on "any local filesystem" on Linux ≥ 2.6.39.

The Linux syscalls `name_to_handle_at(2)` / `open_by_handle_at(2)` are
**explicitly designed for userspace NFS servers** (per the man page).
Properties directly relevant to flint's findings:

- **Rename-stable by construction** — the handle is the inode, and
  rename doesn't change the inode. This is F23 *for free*: no alias
  table, no `note_fs_rename`, no chain-collapse.
- **Generation number stales replacements** — a file deleted and
  recreated at the same inode returns `ESTALE` from
  `open_by_handle_at`. This is F17 *for free*: no SHA-256 identity
  hash, no `embedded_ino` re-verification, no per-resolve `lstat`.
- **Fixed, small handle** — the opaque handle is a few dozen bytes
  (ext4 ~12–20); no v2 long-path id↔path table is ever needed. Fits
  flint's `Nfs4FileHandle::MAX_SIZE = 128` with room to spare.

Because the handle no longer contains or maps to a path, **there are
no `path_to_handle` / `handle_to_path` caches to maintain, so
`note_fs_rename`/`note_fs_remove` cease to exist and F26 cannot
occur.** This is the "something better": it removes the problem class,
where §5 only makes its cost O(1). See §12 for the flint-specific
design.

### 11.2 If forced to stay path-based: generation counters, not eviction

If the inode-handle route is blocked (see §12 caveats), the
state-of-the-art for a *path-keyed* cache under concurrent
rename is **per-directory generation counters with lock-free reads**,
not the point-eviction of §5.2:

- Linux's own dcache (since 2.6.38, Nick Piggin's RCU rewrite) does
  path lookups "without acquiring any lock… a seqlock on each dentry
  detects concurrent modifications and triggers a fallback." Renames
  bump a counter; readers validate and retry rather than lock.
- Bhat/Porter, *"How to Get More Value From Your File System Directory
  Cache"* (SOSP '15), generalizes this: a prefix cache invalidated at
  the **subtree** granularity via directory generation counters, so a
  rename invalidates a branch by bumping one counter instead of
  scanning entries, and lock-free readers detect staleness by counter
  comparison.

This is strictly better than §5.2's point-eviction for the
read-vs-rename race (no writer ever blocks a reader), but more complex
to implement correctly. It's the right answer *only* if §12 is
infeasible; given flint's backing store is a real local fs, §12 is
preferred.

### 11.3 If a bounded path cache is kept: W-TinyLFU over plain LRU

Relevant only to §5.2 (a forward cache that survives Tier 1 but is
deleted by Tier 2). Flint's workload mixes hot reuse (relation files
re-opened constantly) with one-shot scans (WAL segments streamed
once). Plain LRU is polluted by the scan; **W-TinyLFU** (Caffeine's
policy; available in Rust via `moka`/`quick_cache`) adds a frequency
admission filter that resists scan pollution, at ~8 bytes/entry
(a 4-bit CountMinSketch) — "near-optimal hit rate, competitive with
ARC and LIRS" (Einziger & Friedman, *TinyLFU*, and the Caffeine
efficiency data). Net: if we keep a bounded path cache, prefer
W-TinyLFU; but under §12 the cache is gone and this is moot.

## 12. Target architecture — kernel inode handles (Tier 2)

Adopt the knfsd/Ganesha model: flint's NFS-side file handle becomes a
small framed wrapper around the kernel's opaque handle.

**Mint** (replaces `generate_handle` + `v2_handle_for`): on the object
path, call `name_to_handle_at(dirfd, name, &handle, &mnt_id, 0)`; wire
handle = `[version:1][fsid/export:…][kernel_handle:…]`. No hash, no
embedded path, no id table.

**Resolve** (replaces `parse_handle` + `filehandle_to_path` + the
alias follow): `open_by_handle_at(mount_fd, &handle, O_PATH|…)` →
`ESTALE` maps to `NFS4ERR_STALE`; success yields an fd for the object,
which flows straight into the existing `FdCache` for I/O. No path
lookup, no lock, no inode re-verification (the kernel did it).

**Deletions this enables:** `path_to_handle`, `handle_to_path`,
`path_to_id`/`id_to_path` (v2), `rename_aliases` (F23),
`note_fs_rename`, `note_fs_remove`, the SHA-256 identity scheme, and
`follow_rename_alias`. Net **negative** diff — it removes far more than
it adds, and retires F17/F23/F26 as a category.

**What stays / must be designed:**

- **Pseudo-fs / PUTROOTFH** (`pseudo_fs`) is unchanged — only
  real-object handles move to kernel handles.
- **Restart & instance_id semantics.** Kernel handles are naturally
  stable across a server restart on the same filesystem — which is
  what RWX persistence wants — but that inverts today's
  `instance_id`-stamped "STALE everything on restart" behavior. The
  grace-period / reclaim interaction (RECLAIM_COMPLETE, courtesy
  release) must be re-checked so a surviving handle plays correctly
  with the state that *is* rebuilt. This is the main design work.
- **`mount_fd` lifetime.** `open_by_handle_at` needs an fd for the
  filesystem (an O_PATH fd on the export root); hold one for the
  server's life.

**Caveats / dependencies (why this is Tier 2, not Tier 1):**

- **Capability.** `open_by_handle_at` requires `CAP_DAC_READ_SEARCH`.
  Ganesha explicitly flags this as "tricky inside a container." flint's
  NFS pod already runs privileged (it stages block devices and mounts),
  so the cap is almost certainly present — **but this must be verified**
  and pinned in the pod securityContext before committing.
- **Backing fs support.** Requires a local fs that implements
  `export_operations`. flint's export is **ext4** on the ublk block
  device (confirmed: `rwx_nfs.rs` references the "ext4 journal on the
  backing raid"), which fully supports `name_to_handle_at`. A move to
  a fs without it (e.g. plain tmpfs) would break this route.
- **Scope.** Larger blast radius than §5 (touches mint/resolve and the
  restart/reclaim path), so it wants its own drill cycle. Hence:
  **ship §5 now to stop the bleeding; schedule §12 as the durable
  fix.**

**Recommended sequencing (decided 2026-07-19): go straight to §12; do
not build §5.** The §5 path-based fix is throwaway by construction —
every structure it adds, §12 deletes — and there is no production fire
to justify an interim patch (the cluster is torn down; nothing is
paging). §12 is a *net-negative* diff that retires the design smell
behind the whole F17/F23/F24/F26 family, and hands F17+F23 to the
kernel's battle-tested generation/inode semantics. The plan:

1. **Capability spike first (~1 h, gating).** In the real flint-nfs
   pod securityContext, `name_to_handle_at` on `/mnt/volume` and
   `open_by_handle_at` the result. This de-risks the only hard
   dependency (Ganesha flags `open_by_handle_at` as "tricky inside a
   container").
   - **Green (expected):** proceed to step 2; §5 is never written.
   - **Red (cap blocked by seccomp/SELinux/runtime and ungrantable):**
     fall back to the path-based **generation-counter** design of
     §11.2 (dcache-RCU / SOSP '15), *not* §5's point-eviction.
2. **Implement §12 mint/resolve** against the ext4 export, wiring
   `open_by_handle_at` fds into the existing `FdCache`.
3. **Re-validate the restart/reclaim path** (the real design work —
   kernel handles surviving restart inverts today's instance_id
   "STALE-on-restart" behavior) with pynfs + a full phase-3 drill
   re-run. This gate is already owed after the F17–F24 handle changes.

§5 remains documented above only as the fallback shape and as the
explanation of the F26 mechanism; it is not the plan of record.

## 13. Sources

- Linux `name_to_handle_at(2)` / `open_by_handle_at(2)` man pages
  (man7.org) — userspace NFS server handle API, `CAP_DAC_READ_SEARCH`,
  ESTALE/generation semantics.
- Chris Siebenmann, "The Linux kernel NFS server and reconnecting
  client NFS filehandles" — knfsd handles = inode + generation, not
  path.
- NFS-Ganesha wiki, *VFS* and *Fsalsupport* — FSAL_VFS uses
  `name_to_handle_at`/`open_by_handle_at`; ~5-byte header + fsid;
  container-privilege caveat.
- McKenney et al., "Scaling dcache with RCU" (Linux Journal) and the
  Linux 2.6.38 RCU-walk dcache — lock-free path lookup, seqlock
  fallback on rename.
- Bhat & Porter, "How to Get More Value From Your File System Directory
  Cache," SOSP '15 — subtree-granularity invalidation via directory
  generation counters with lock-free reads.
- Einziger & Friedman, "TinyLFU: A Highly Efficient Cache Admission
  Policy" (arXiv:1512.00727); Caffeine *Efficiency* wiki
  (ben-manes/caffeine) — W-TinyLFU hit-rate vs LRU/ARC/LIRS, 8 B/entry.
- Einziger et al., "Limited Associativity Makes Concurrent Software
  Caches a Breeze" (arXiv:2109.03021) — set-associative concurrent
  caches as a simpler-to-parallelize alternative to fully-associative
  LRU.
- RFC 7530 §4 (NFSv4) — persistent vs volatile filehandles,
  `FH4_VOL_RENAME`.
