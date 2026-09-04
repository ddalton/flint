# The lean manifest: an immutable generation plus a pointer — design of record

Status: **BUILT 2026-09-03** (`lean/sidecar/src/manifest.rs`), 126/0 in the
lean suite. §7 records what the build changed about §3.
Written 2026-09-03 after the user asked for metadata optimisation and
proposed three options; this is the fourth, arrived at by checking the
first three against the code.

## 1. What the manifest costs today

`<prefix>/.flint/lean/manifest` is ONE object holding every entry
(`lib.rs:269-271`, `manifest.rs:49-72`). It is read whole
(`manifest::load`, `:99-116`) and rewritten whole under
`PutCondition::IfMatch(etag)` (`cas_write_stamped`, `:137-180`). The
cost of every operation therefore scales with the size of the PROJECT
rather than with what changed — the exact property chunked immutable
metadata exists to avoid.

The code already records the symptoms, measured on the 0b rig:

- at 1M entries the document is ~264 MiB, and an IDLE barrier tick cost
  **27 s / 1.3 GiB** before the no-change early exit was added
  (`barrier.rs:452-470`);
- that early exit is a HEAD against the persisted baseline ETag, and it
  gets `remote.seq` for free from the object's `flint-gen` stamp;
- `rotate_for_takeover` (`manifest.rs:248-269`) clones the standing
  manifest, bumps `seq`, and CASes it back: **a multi-MB GET + PUT per
  claim**, it double-bumps `seq` for one logical change, and — because
  it moves the ETag without changing a single entry — it **defeats the
  early exit for every other syncer**, which then GETs and parses the
  whole document to discover nothing moved.

## 2. What rotation actually protects, checked rather than assumed

The obvious reading is that `seq` is the fence. It is not. The manifest
CAS is `IfMatch` on the OBJECT's ETag; nothing on the write path
compares epochs — `cas_write_stamped` only *stamps* `epoch` into
`GenerationStamps` (`:160-176`). So rotation's protection is entirely
"rewrite the object, and every outstanding ETag handle goes stale". The
`seq += 1` is there because a PUT of identical bytes reproduces the same
MD5 ETag — the same reason `EpochBody` carries a `salt`
(`flint-store/src/s3.rs:213-224`).

That matters, because it kills the tempting version of the fix. Moving
`seq` into the epoch cell and leaving the manifest untouched would leave
a straggler's manifest ETag **still valid**: the protection would be
silently removed, not made cheap.

It also narrows what rotation is worth. Every barrier-triggering arm
renews the lease FIRST (D12: `sentinel.rs:759-767`, `:1064-1072`,
`:1083-1090`), and a renew against a superseded cell is a 412 that
fences the syncer — measured on kind, a SIGSTOPped holder woken after a
takeover fenced itself and exited within 15 s (`fenced: deposed at
renew: epoch_put: 412 PreconditionFailed`, S14). The gateway path says
so outright: *"per-request epoch validation on the manifest path — the
straggler's CAS dies HERE even if rotation were absent"*
(`gateway.rs:752-756`).

**So rotation covers exactly one window: between a syncer's successful
renew and its manifest CAS.** Real, and worth keeping. Currently paid
for with a multi-MB GET+PUT on every claim.

## 3. The design

Two objects where there was one.

```
<prefix>/.flint/lean/current                 # tiny, mutable, CAS'd
<prefix>/.flint/lean/manifests/<seq:020>     # immutable, write-once
```

`current` is the only mutable metadata object:

```json
{ "seq": 42,
  "entries_key": ".flint/lean/manifests/00000000000000000041",
  "entries_etag": "…",
  "pinned_reads": false,
  "boundary_source": "sentinel",
  "epoch": 7 }
```

- **Publish**: PUT the new generation object (a fresh key, so
  `IfNoneMatchAny`), then CAS `current` from the pointer ETag the writer
  loaded. The CAS that decides the race moves from a multi-MB object to
  a few hundred bytes.
- **Rotation**: CAS `current` with `seq + 1` and the **same**
  `entries_key`. O(1), and the straggler's pointer ETag is stale exactly
  as it is today.
- **Read**: GET `current`, then GET `entries_key`. One extra round trip,
  cacheable by ETag.
- **The no-change early exit gets BETTER**: HEAD `current`; if its ETag
  is unchanged, nothing moved (as today). If it changed but
  `entries_key` did not, only a rotation happened — so the entries are
  known unchanged and the whole-document GET is skipped. The
  "rotation defeats the early exit" line disappears rather than shrinking.
- `pinned_reads` and `boundary_source` move onto the pointer, so the
  gateway's HEAD-instead-of-GET trick (`gateway.rs:548-556`) keeps
  working and gets cheaper: it HEADs a small object rather than a
  possibly-hundreds-of-MiB one.

### Why this composes with chunking rather than competing

The user's option 1 — split by key range so a publish rewrites only the
chunks it touched — is the change that fixes the asymptotics, and it
wants immutable chunk objects and a list that names them. That list is
this pointer, grown a field. Doing the pointer first makes chunking an
edit to `entries_key` → `chunks: [...]`, not a second protocol change.

## 4. What it costs, stated plainly

- **Two round trips per publish** instead of one. Publishes are already
  O(entries) in bytes; one extra small PUT is noise beside that.
- **Garbage.** Superseded generation objects accumulate and need a
  reaper — keep the last N, or an age, alongside the existing
  `noncurrentRetentionDays` posture. A crash between the body PUT and
  the pointer CAS leaves an orphan generation that nobody reads; the
  same sweep collects it.
- **A migration**, below, which is the only genuinely delicate part.

## 5. Migration, fail-closed

An existing bucket has `.flint/lean/manifest` and no `current`. The
hazard is not the new reader — it can fall back — but the OLD one:

> `manifest::load` returns `Ok(None)` for a missing object, and `None`
> means *first write*, which a barrier answers with
> `IfNoneMatchAny`. An old syncer pointed at a migrated workspace whose
> legacy key had simply been deleted would therefore conclude the
> project is empty and **re-seed over it**.

So the legacy key is never deleted; it is **overwritten with a document
that cannot parse as a manifest**:

```json
{ "moved": ".flint/lean/current", "note": "pointer layout; upgrade flint-sync" }
```

`LeanManifest` has no `serde(default)` for `seq` or `entries`, so
`parse` fails, `load` returns `LeanError::State`, and an old syncer
REFUSES rather than re-seeds. Fail-closed, and the error names the fix.

New readers: `current` absent ⇒ read the legacy key as generation 0 and
install the pointer on the first write. That path is what every existing
workspace takes exactly once.

## 6. Falsifiers

1. A takeover on a 100k-entry workspace performs **no GET or PUT of a
   generation object** — only the pointer moves. (Bucket request count,
   from MinIO's access log.)
2. After that takeover, an unrelated syncer's idle tick still takes its
   early exit: one HEAD, no GET. Today it takes a full GET + parse.
3. A straggler frozen between renew and CAS, then woken, still fails its
   publish — the window rotation exists for is still closed.
4. An OLD `flint-sync` pointed at a migrated workspace refuses with a
   parse error and writes NOTHING; the manifest and the project survive.
5. A crash between the generation PUT and the pointer CAS leaves the
   workspace readable at the previous generation, and the orphan is
   swept.

## 7. What the build changed about this design

Three things, each found by writing the code or the test rather than by
re-reading the plan.

**The generation key is unique per WRITE, not per generation.** §3 named
it `manifests/<seq:020>`. The lean suite refused immediately: a
write-once key keyed on `seq` breaks every path that legitimately
rewrites without bumping — the gated lane's version-id backfill among
them — and turns an ordinary retry into a hard error. The key is now
`manifests/<seq:020>-<flush_uuid>`: still lexically chronological, which
is what the reaper and `mc ls` want, but unique per writer. Two writers
that reach the same seq now write two different objects and race only at
the pointer, which is the one place a race should be decided; and a
writer retrying its own interrupted publish re-puts a byte-identical
object under its own uuid and falls through to the CAS.

**The reaper is not symmetric about the live generation**, and the first
cut of it was. Below the live pointer everything is superseded by
definition, and the only question is how much slack to leave a reader
that has resolved the pointer and not yet fetched the object it names —
that is `KEEP_GENERATIONS = 5`. Above it, an object may be a publish
still IN FLIGHT, its entries written and its pointer CAS not yet landed.
A keep-window is exactly the wrong tool there: an orphan at a high seq
sorts to the front, so the window protects it forever AND costs a real
generation its slot. The test caught this on the first run. Age decides
that case instead (`ORPHAN_GRACE_SECS = 3600`), and an object the store
cannot date is left alone — a leak beats deleting a live publish.

**The `entries_seq` refinement is present in the format but not yet
exploited.** §3 promised that a follower seeing a moved pointer with an
unchanged `entries_key` could skip the entries GET. The field is
written and asserted, but nothing reads it yet, because in the
single-writer model the only syncers that observe a rotation are the
successor (which wants the entries anyway) and the deposed one (which
fences within seconds). The readers it actually helps are CROSS-CLUSTER
followers, which poll a prefix they do not hold. That is a real case and
worth building — it belongs with chunking, where a follower will want to
diff chunk lists rather than documents.

## 8. Measured

`a_rotation_reads_and_writes_no_generation_object` (lean suite) asserts
the falsifier directly, with the memory store's op counter: a takeover
over a 200-entry workspace is **at most three requests total**, the
entries object's ETag is unchanged across it, and the generation count
does not grow. Under the single-object layout the same claim was a GET
and a PUT of the entire document.

What is NOT yet measured is the end-to-end shape on a real bucket, which
is the drill leg this design still owes.
