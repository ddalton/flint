# Working in a flint-lean workspace

You are running inside a pod whose working directory (usually
`/workspace`) is a **flint-lean workspace**: an ordinary local directory
that a syncer process publishes to an S3 bucket at *boundaries*. Read and
write it with normal tools. There is no special API to call, no library
to import, and no network access required. Everything you need is a
file under this directory. This document is written by the syncer itself
(`.flint/AGENTS.md`), so it always describes the syncer you are talking
to; sentinel protocol 1.

## The model, in five lines

1. **The tree is a local copy.** Your writes land on the pod's disk at
   once and reach the bucket only at a boundary. Until then they exist
   only here; if the pod is replaced before a boundary, they are gone and
   the new pod starts from the last boundary.
2. **A boundary is a coherent point.** Every file as it stood at that
   point is in the bucket together, under one manifest. Readers elsewhere
   see the last boundary, never a half-written tree.
3. **Boundaries happen on a cadence** (every `floorSecs` seconds, default
   60) **and on demand**, when you ask for one with the publish verb.
   Ask when you finish a unit of work; do not rely on the cadence for
   anything a reader must see.
4. **One writer.** This workspace has exactly one syncer, and every
   process in this pod shares the tree. Nobody else writes to these
   files live. Contributions from outside (another party through the
   gateway, or a previous incarnation of this workspace) arrive only at a
   boundary, and only onto files you have not modified.
5. **Nothing is silent.** Every file the syncer did not take, and every
   foreign change it declined to apply over yours, is named in a record
   you can read.

## The control namespace: `.flint/`

`.flint/` at the workspace root is reserved. It is never published, and
the syncer owns it. You write exactly two names there, `publish` and
`sync`; you read the rest. Do not store data under `.flint/`.

| file | who writes | what it is |
|---|---|---|
| `.flint/capabilities.json` | syncer | the marker. **Read it before touching a sentinel.** |
| `.flint/publish` | you | ask for a boundary now |
| `.flint/publish.ack` | syncer | the answer to `publish` |
| `.flint/sync` | you | ask to integrate news from the bucket now |
| `.flint/sync.ack` | syncer | the answer to `sync` |
| `.flint/remote.seq` | syncer | the news ticker: is there anything to integrate? |
| `.flint/AGENTS.md` | syncer | this document |

The syncer's own state lives in `.flint-sync/` at the workspace root.
Never write there. Two files in it are for you to read:
`.flint-sync/gauges.json` (health: `rpo_secs`, `withheld_reason`,
`last_boundary`) and `.flint-sync/conflicts.jsonl` (one
JSON record per line per conflict: `path`, `kind`, `foreign_etag`,
`preserved_key`, `at_unix`; the file rotates at 1 MiB into
`conflicts.jsonl.1` and older records are gone — read it as you go).

### `capabilities.json`: check first, every time

```json
{ "protocol": 1, "verbs": ["publish", "sync", "remote-seq"],
  "state": "live",
  "sentinel_min_interval_secs": 5, "sentinel_hourly_budget": 60,
  "syncer_version": "…", "boot": { "holder_id": "…", "boot_unix": 0 } }
```

- **File absent** ⇒ an old syncer that does not know the verbs. Do not
  create `.flint/publish` or `.flint/sync`: an old syncer would upload
  them to the bucket as data. Work normally and rely on the cadence.
- **`verbs` empty** ⇒ do not touch sentinels; `reason` says why they are
  off (the pre-flight found `.flint/` already in use by an application,
  or the operator turned sentinels off). Keep working on the files; the
  cadence still publishes. `state` is always `"live"`.

## `publish`: declare a coherent point

Write `.flint/publish`. Either an empty file, or a JSON body:

```json
{ "nonce": "task-42", "note": "tests green, ready for review" }
```

`nonce` is any string you choose (at most 128 bytes) so you can find your
answer — use a fresh one for every touch, since a reused nonce matches
the previous ack; `note` is free text (at most 4 KiB). **Write the body atomically**:
write to a temporary name inside `.flint/` and rename it onto
`.flint/publish`. The syncer consumes the file by renaming it away, and a
plain write racing that rename can leave a torn body (which is then
honoured as a bare touch, with a warning record).

The meaning of the touch: *everything on disk when the syncer picks the
file up is part of the boundary.* The barrier scans strictly after the
consume, so every completed write is included; a file still being
written may publish with later bytes, never earlier ones. A file you
deleted is deleted in the bucket at that boundary.

Then wait for `.flint/publish.ack`. **Do not wait on the file's mere
existence**: the ack of a previous boundary stays in place until the
next one overwrites it. Match your own answer by one of two rules:

- your `nonce` is in `nonces` (the list is bounded at 32; under a storm
  of touches the oldest are dropped), or
- `sentinel_mtime_unix_ns` is at least the time of your touch, where
  "the time of your touch" is the mtime `stat` reports for the file you
  renamed into place (or for the temp file just before the rename) — not
  a wall-clock reading taken before writing, which a filesystem's mtime
  clock can lag by a tick. A later boundary strictly contains an earlier
  one, so an ack for a touch after yours covers yours.

```sh
printf '{"nonce":"task-42"}' > .flint/.publish.tmp && mv .flint/.publish.tmp .flint/publish
until grep -qs '"task-42"' .flint/publish.ack; do sleep 1; done
cat .flint/publish.ack
```

(The nonce rule is enough unless more than 32 touches coalesce; then fall
back to the mtime rule with any JSON reader.)

The ack:

```json
{ "status": "ok", "nonces": ["task-42"], "sentinel_mtime_unix_ns": 0,
  "seq": 17, "manifest_etag": "\"…\"", "boundary": "sentinel",
  "completed_unix": 0,
  "report": { "uploaded": 3, "deleted": 1, "parked": 0, "consumed": 0,
              "no_change": false, "conflicts": [] } }
```

Empty lists (`conflicts`, `dropped`, `applied`) are omitted from the
JSON, not written as `[]`.

- `status: "ok"` — **the boundary is in the bucket**, under manifest
  `seq`. Not queued, not scheduled. `uploaded: 0` with `no_change: true`
  is still an honest ok: nothing had changed since the last boundary.
- `status: "partial"` — the boundary installed, but the paths in
  `report.dropped` are not in it: each met a newer foreign version the
  syncer could not preserve, or the copy in the bucket that it meant to
  cite — one it found there, or its own upload — was replaced or removed
  by another writer just before this boundary committed (an
  `adopt-withheld` or `upload-withheld` record); `report.parked`
  counts them. Treat it as a failure for those paths and touch again.
- `boundary: "sentinel-deferred"` — your touch was honoured by the
  cadence tick rather than at once: it arrived inside
  `sentinel_min_interval_secs` of the previous boundary, or the hourly
  budget was spent. Honoured in full either way. `boundary: "drain"` —
  the syncer was stopping (the pod is shutting down) and honoured your
  touch as part of its final boundary.
- `report.parked` — paths whose upload met a newer foreign version the
  syncer could not preserve; they are not in this boundary (`status` is
  `partial` and `report.dropped` names them) and are retried at the
  next. In the normal case the foreign version IS preserved and your
  version is published over it, with an `upload-412-preserved` record
  naming the preserved copy. `report.consumed` — foreign writes
  integrated into your tree at this boundary (see "foreign changes").
  `report.conflicts` — the conflict records this boundary wrote.

Rate limits, enforced by the syncer, never by you: at most one
sentinel boundary per `sentinel_min_interval_secs` (touches inside the
wait coalesce into one boundary whose ack lists every nonce), and
`sentinel_hourly_budget` units per hour where a unit is 64 MiB of
published bytes (a small diff costs 1). Touch once per unit of work,
not in a loop.

## `sync`: integrate news at a point you choose

The syncer never overwrites a file you modified. News from the bucket
reaches you two ways: writes queued from outside (the inbox) are applied
at every boundary onto files you have not modified; everything else (a
manifest that moved ahead, through another party or a previous
incarnation of this workspace) waits until you ask for it with `sync`.

`.flint/remote.seq` tells you whether there is news, at zero cost:

```json
{ "observed_seq": 21, "integrated_seq": 17, "updated_unix": 0,
  "sync_requested_unix": 0, "sync_requested_by": "reviewer@example" }
```

`observed_seq > integrated_seq` ⇒ there is news you have not integrated.
`sync_requested_by` ⇒ someone asked you to pull (advisory: it is a
request, not an action). `updated_unix` older than three times the floor
⇒ the syncer or its bucket path is unhealthy; look at `gauges.json`.

To integrate, write `.flint/sync` the same way you write `publish`:
empty, or `{"nonce": "…", "scope": ["inputs/", "shared/config.json"]}`.
Without `scope` the whole tree is reconciled to the bucket's latest
manifest; with `scope` (up to 64 prefixes or exact paths, matched on
whole path components) only those paths are, and everything else keeps
flowing through the ordinary boundary path. An empty or invalid scope
(no entry that is a relative path without `.` or `..`, or more than 64)
is answered with `status: "refused-scope"` and a `reason`: nothing was
done, fix the scope and touch again. A touch without `scope` that
coalesces with a scoped one is honoured as the whole tree. The rules
are the same either way:

- remote adds, changes and deletes are applied **only to paths you have
  not modified since the last boundary**;
- **your modified version always wins** — nothing of yours is
  overwritten; every foreign change declined for that reason is a
  `sync-dirty` record in `sync.ack`'s `report.conflicts` (and in
  `conflicts.jsonl`), and the foreign bytes remain in the bucket under
  their own version;
- the ack lists `report.applied` (paths written) and, for a scoped sync,
  `out_of_scope_foreign` (changes seen but left for the boundary path).

Ask for a sync at a safe point: between tasks, never mid-edit of the
files in scope.

## Foreign changes at a boundary, and other writers

At every boundary the syncer first integrates the bucket's inbox (writes
made from outside) into your tree, onto paths you have not modified. If
you have modified a path someone else also wrote, **your version wins
and is published**; the foreign bytes are preserved in the bucket and
a `consume-dirty` record names the path and where the foreign copy went
(`preserved_key`). When you collaborate, read `report.conflicts` on your
acks and `conflicts.jsonl` before assuming a path is the latest.

**Other agents may share this workspace**, each with its own syncer and
its own tree, and every one of them publishes every floor: a boundary
never waits for another writer's lifetime, only for the seconds its
commit takes. What that means for you:

- a path you have not modified may change under you at a boundary, or
  disappear if another writer deleted it — the ack's `report.consumed`
  counts both, and `remote.seq` says there is news before that. A path
  you HAVE modified is never removed that way: your version stays and
  publishes, and a `consume-foreign-delete-vs-dirty` record names it;
- if two writers edit ONE file, the later boundary's version is current
  and the earlier is preserved in the bucket with an
  `upload-412-preserved` record on the later writer and a
  `consume-dirty` record on the earlier one when its copy is replaced.
  Nothing is lost, but only one version is current: edit disjoint
  files where you can, and `sync` before you start on a path that
  `remote.seq` says has news;
- a boundary that could not take its turn (`publish.ack` absent past
  two floors) is retried by the cadence; touch again rather than assume
  it failed.

## What gets published, and how change is detected

- **Regular files only**, with their mode bits (an executable stays
  executable). Symlinks are skipped and never published. Empty
  directories are not tracked: a fresh checkout will not have them.
  Sockets, FIFOs and devices are ignored.
- **A file has changed if its size or its mtime differs** from the last
  boundary (mtime to the nanosecond). So never preserve or restore
  timestamps after editing (`cp -p`, `touch -d`, `rsync -t`): a rewrite
  to the same size that keeps the old mtime is invisible until the file
  changes again. Plain editors and `cp` are fine.
- **A delete is published** once the path is absent from two consecutive
  scans, and a declared boundary (`publish`) takes the second look at
  once: `rm` then `publish` deletes it in the bucket.
- **A rename or move is a delete plus a fresh upload** of every byte
  under the new path: the object key is the path. Moving a large
  directory re-uploads it. Edit in place where you can.
- Files under `.flint/` and `.flint-sync/` are never published.
- The whole tree lives on the pod's disk; keep it within the workspace's
  size limit and the file-count budget the operator set.

## Do and do not

Do:

- read `.flint/capabilities.json` before your first sentinel;
- touch `.flint/publish` when a unit of work is complete, with a nonce,
  and wait for *your* ack by nonce or by mtime;
- check `.flint/remote.seq` between tasks and `sync` at a safe point when
  it shows news;
- read `report.conflicts` and `conflicts.jsonl` when working with others.

Do not:

- write anything under `.flint/` other than `publish` and `sync`, or
  anything at all under `.flint-sync/`;
- wait on `publish.ack` merely existing, or reuse a nonce;
- touch sentinels in a loop, or expect a boundary faster than
  `sentinel_min_interval_secs`;
- preserve timestamps when copying edited files into the tree;
- rely on symlinks or empty directories surviving a checkout;
- treat an ack as proof for anyone but yourself: acks are files any
  process in the pod could write. The authoritative record is the
  manifest in the bucket, which outside parties read through the
  gateway.
