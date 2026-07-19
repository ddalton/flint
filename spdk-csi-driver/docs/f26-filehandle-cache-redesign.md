# F26 — FileHandleManager cache re-architecture

Design analysis for the O(N)-scan-under-global-lock defect found in
`src/nfs/v4/filehandle.rs` (F26 in
`attach-detach-campaign-2026-07.md`). Status: **proposal, not
implemented.** No code changed by this document.

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
