# Chunked manifest entries — design of record

Status: **DESIGN — no code.** Written 2026-09-03. Step TWO. Step one is
`flint-lean-manifest-pointer-design.md` (immutable generations plus a
small mutable pointer), **built and committed at `1ace7bca`**; this
builds on it and is deliberately a separate change so each can be
attributed on its own.

## 1. What is still O(entries)

The pointer layout made the TAKEOVER cheap — a claim is now three small
requests instead of a multi-MB GET and PUT. It did nothing for the
publish. A citation still writes every entry of the project into one
generation object, and a barrier merge still parses every entry to
reconcile a handful of paths. At 1M entries that is ~264 MiB written to
change one file, and ~264 MiB parsed to discover that one file moved.

The goal here is the one the user stated: **a publish costs
O(changed), not O(entries).**

## 2. Shape

`Pointer.entries_key` becomes a chunk LIST. Entries are split over the
sorted key stream into immutable chunk objects; a publish rewrites only
the chunks it touched and then CASes the pointer, which is what makes a
multi-chunk publish atomic — readers never see a half-written set,
because the only thing that ever becomes visible is one small pointer.

```
.flint/lean/current                       { seq, chunks: [...], ... }
.flint/lean/chunks/<hash>                 immutable, content-addressed
```

Chunks are **content-addressed** rather than seq-named: two generations
that share a chunk share the object, so a publish touching one file in a
1M-entry project uploads one chunk and the other ~200 are simply
referenced again. That is where the asymptotic win actually comes from —
naming chunks by generation would rewrite every chunk on every publish
even if their contents were identical.

## 3. Boundaries: content-defined, and why not fixed-count

**Fixed-count chunking is disqualified**, and it is worth being precise
about why, because it looks reasonable. Split every 5000 entries in
sorted order and insert one file near the FRONT: every subsequent entry
shifts one slot, so every chunk boundary moves and every chunk is
rewritten. That restores O(entries) on precisely the operation being
optimised — a single-file publish — and it does so silently, since the
sizes look right and only the write volume betrays it.

**Content-defined boundaries** make the split a function of the KEY SET,
not of position. For each entry in sorted order, hash the path; cut
after the entry when `h % TARGET == 0`. Inserting a key can then only
(a) land inside a run and grow that one chunk, or (b) itself be a
boundary and split one chunk into two. Deleting is the mirror. Either
way the number of chunks rewritten is proportional to the number of
CHANGED KEYS, which is the property being bought. This is lakeFS's range
files and Iceberg's manifest lists; restic uses the same idea over file
content rather than key names.

Sizing, and the honest trade:

| knob | value | why |
|---|---|---|
| `TARGET` | ~4096 entries | ~1 MiB of compact JSON per chunk: big enough that per-object overhead is noise, small enough that one changed file rewrites ~1 MiB rather than ~264 MiB |
| `MIN` | `TARGET/4` | suppress degenerate runs; hash boundaries are geometrically distributed and without a floor a project accumulates many tiny objects |
| `MAX` | `4 × TARGET` | bound the worst case so one pathological run cannot recreate the single-object problem |

`MIN` and `MAX` are where the pure-function property leaks: a suppressed
or forced cut depends on where the PREVIOUS boundary fell, so a change
can cascade — but only until the next natural boundary that satisfies
`MIN`, which is one chunk in expectation. Stating it rather than hiding
it: the guarantee is "O(changed) in expectation", not "worst case".

The hash must be **stable across versions and platforms** — it is part
of the on-the-wire format, not an implementation detail. `crc64_nvme` is
already in the tree, already used for object integrity, and already
stable; reuse it rather than introducing xxhash.

**Two hashes, not one (settled at implementation, 2026-09-04).** The
BOUNDARY rule above uses `crc64_nvme`. The chunk's content ADDRESS does
not, and the distinction is not fussiness — the two carry different
consequences. A boundary collision merely puts a cut somewhere
unremarkable and costs nothing. An address collision means two
different chunk bodies share an object key and one silently shadows the
other: a data-loss class, and a silent one, discovered as entries that
vanished from a manifest nobody edited. 64 bits is thin margin to take
that on, so the address is SHA-256 truncated to 128 bits (`sha2` is
already a `flint-store` dependency, so no new supply-chain surface).
Boundaries stay on crc64 because they are hot, cheap, and harmless when
they collide.

## 4. When the pointer itself becomes the bottleneck

The pointer is rewritten on every publish and now carries the chunk
list, so its size is `O(entries / TARGET)`.

| entries | chunks | pointer body | verdict |
|---|---|---|---|
| 100k | ~25 | ~2 KB | noise |
| 1M | ~250 | ~20 KB | fine — smaller than one of today's HTTP responses |
| 10M | ~2.4k | ~200 KB | the knee: rewritten every publish, and a publish can be seconds apart |
| 100M | ~24k | ~2 MB | a second single-object problem, one level up |

**So: single level to ~10M entries, and a second level beyond it** — the
pointer names a small number of chunk-LIST objects, each naming chunks,
which is exactly Iceberg's manifest-list-of-manifests. The format should
therefore make `chunks` a tagged union from day one (`{"chunks": [...]}`
vs `{"chunk_lists": [...]}`) so growing a level later is a reader
addition and not a third migration. Building the second level now would
be speculative; leaving no room for it would be the mistake.

## 5. Reads become proportional too

- **Barrier merge.** Today it parses the whole document to reconcile a
  handful of paths. With chunks it resolves each changed path to its
  chunk by boundary search, fetches only those chunks, merges within
  them, and rewrites only them. The three-way merge itself is unchanged
  — it is per-path already; what changes is how much of the manifest is
  in memory to run it.
- **Partial checkout** (a prefix, a resumed fetch) touches the chunks
  covering that key range instead of the whole document.
- **A full checkout** still reads every chunk, and should: it wants
  every entry. The win there is streaming — chunks parse and materialise
  one at a time instead of holding 264 MiB of `BTreeMap` at once, which
  is the `fetchInflightMb` pressure from a different direction.

## 6. What must not regress

- **The barrier's no-change early exit.** It is a read of the pointer
  and a comparison against a persisted baseline, and it supplies
  `remote.seq` for free. A chunk list does not disturb that: the pointer
  is still one small object and its ETag still answers "did anything
  move". The `entries_seq` refinement the pointer design deferred lands
  naturally here as a chunk-list diff — a follower learns exactly which
  chunks changed and can skip the rest.
- **`pinned_reads` and `boundary_source`** already live on the pointer,
  not in the entries, so they are untouched.
- **The gated lane's version-id backfill** (`gated.rs:1124`) rewrites
  entries in place across the whole document. Chunked, it must rewrite
  only the chunks holding backfilled paths — otherwise the one path that
  legitimately rewrites everything stays O(entries) and quietly
  dominates the gated mode.
- **The gateway's HITL CAS** (`gateway.rs:748-775`) accepts a whole
  `LeanManifest` over the wire and CASes it. Chunked, that is a
  full-document write from a remote caller. It must either chunk it
  server-side on receipt (simple, keeps the wire type) or move to a
  chunk-aware verb (faster, a protocol change). **Chunk server-side**;
  the HITL path is rare and correctness beats throughput there.

## 7. Migration, fail-closed — the same hazard, a third road

`manifest::load` maps a missing object to `Ok(None)`, `None` means first
write, and a barrier answers that with `If-None-Match: *`. Every layout
change must therefore make an old reader REFUSE rather than conclude the
project is empty.

The pointer layout already carries this: an old binary reading a
migrated workspace finds a legacy key it cannot parse. For chunking the
same rule applies one level in — a pointer carrying `chunks` must be
unreadable to a pointer-era binary that expects `entries_key`. Since
`entries_key` is a REQUIRED field on `Pointer` with no serde default,
a chunked pointer that simply omits it already fails to parse, which is
the behaviour wanted. **That is load-bearing and must not be "fixed" by
adding a default.** A test asserts it.

## 8. Falsifiers

1. A publish touching 3 files in a 200k-entry fixture writes bytes
   proportional to those files: one or two chunks, not the project.
   Measured with the memory store's op counter in a unit test, and on
   the kind rig with MinIO's request log.
2. The same publish READS one or two chunks, not every chunk.
3. Inserting a file at the FRONT of the key order rewrites the same
   small number of chunks as inserting one in the middle — the
   fixed-count failure mode, asserted absent.
4. A checkout of a chunked workspace yields byte-identical files to a
   checkout of the same workspace unchunked.
5. A pointer-era binary reading a chunked workspace REFUSES and writes
   nothing.
6. Chunk GC: a chunk still referenced by any retained generation is
   never collected. (This is why chunks are content-addressed and
   reference-counted across retained pointers, rather than swept by age
   like generations.)

## 8.1 Orphans from a crash between the chunk PUTs and the pointer CAS

The pointer layout's version of this is already solved and the shape
carries over: `cas_write_stamped` writes the generation, then CASes the
pointer, and a crash in between leaves an object no pointer names, which
`sweep_generations` collects by AGE above the live pointer (it is a
window below it and a grace above — asymmetric on purpose, because
"newer than the live pointer" and "abandoned" look identical by name).

Chunked, the same crash leaves N orphan chunks instead of one orphan
generation, and one property makes it strictly better rather than worse:
**chunks are content-addressed, so a retry of the same publish
regenerates the same names and ADOPTS the orphans** instead of
duplicating them. The uuid in a generation key cannot do that — a retry
there always writes a second object. The wasted bytes from a crash are
therefore bounded by what the retry does not re-reference, not by the
number of retries.

What must not be got wrong is the sweep's ORDER, and it is the opposite
of the intuitive one:

1. **List candidate chunks first.**
2. **Then** union the chunk sets of every retained pointer.
3. Delete `candidates − referenced`, and only those older than
   `ORPHAN_GRACE_SECS`.

Reading the pointers first and listing second is the bug: a chunk PUT
and referenced by a pointer installed in that gap is absent from the
reference set and present in the listing, so it is collected while
live — destroying a chunk the current manifest cites. Listing first
makes any newly-referenced chunk either absent from the candidate list
(written after it) or present in the reference set (read after it).

Note that `sweep_generations` does the OPPOSITE — it loads the pointer
and then lists — and is correct to. A generation is named by exactly one
pointer and its key embeds the seq, so a generation newer than the live
one is protected by POSITION plus the age grace, never by membership in
a reference set; the gap that would be fatal for chunks is harmless
there. Do not "harmonize" the two sweeps into one order: they are
protecting against different things, and the chunk sweep is the one
whose safety depends on the listing happening first.

The age grace is still required on top of the ordering, and for a
different reason: a chunk a concurrent publisher wrote thirty seconds
ago and has not yet named is byte-for-byte indistinguishable from one a
crashed publisher abandoned. Only elapsed time separates them, so the
grace must exceed the longest plausible publish rather than the longest
plausible sweep. This is one place where the generation reaper's
constant can be reused as-is.

Falsifier: kill a publisher between its chunk PUTs and its pointer CAS,
run the sweep immediately, and assert the orphans SURVIVE (the grace
holds); then re-run the same publish and assert it writes zero new chunk
objects (adoption), and that the resulting manifest is byte-identical to
one produced without the crash.

## 9. The part that needs care beyond this doc

GC is genuinely harder than it was for generations. A generation object
was named by exactly one pointer; a CHUNK is shared by every generation
that did not change it, so "delete what the live pointer does not name"
would destroy the history the reaper's window exists to preserve. The
sweep must union the chunk sets of every RETAINED pointer and delete
only what none of them names — which means retained pointers have to be
enumerable, i.e. the generation objects the pointer reaper already keeps
must become pointer snapshots. Design that before writing the reaper,
not after.
