# repack amplification — bytes to S3 per byte pushed

`maybe_repack` runs `git repack -a -d -b`, which collapses the
repository into ONE pack, and then uploads every pack the snapshot does
not already name. That pack is the whole repository. So every time the
pack count passes `repack_threshold` (24), a repository re-uploads all
of itself, with the serving loop inside that upload and pushes queueing
behind it. The scale drill measured such an upload at **262 s for
10 GiB on real S3**.

Nobody had measured what that costs per push. This does, end to end,
against the shipped binary.

    ./run-repack.sh          # ~6 min: two repository shapes + the control

## What it measures

Amplification is `bytes of pack uploaded / bytes of content pushed`,
over the steady state. The seed push is excluded: an import uploads the
repository once whatever the repack policy is.

Bytes uploaded are counted as **keys that have ever appeared** under
the pack prefix, not the bucket's current contents. Packs are
content-named and immutable, so a key showing up is an upload that
happened, and the sweep deleting it later does not un-spend it.
Counting current contents would measure the opposite of the thing: a
repack makes the bucket *smaller* while spending the most.

## The numbers (2026-09-05, MinIO, 30 pushes, threshold 24)

| arm | content pushed | uploaded | amplification | the repack push |
|---|---|---|---|---|
| source-shaped (12k small files) | 0.1 MiB | 3.8 MiB | **67.2x** | 3.6 MiB for 2 KiB of content |
| blob-shaped (96 MiB of binaries) | 60.0 MiB | 204.2 MiB | **3.4x** | 146.1 MiB for 2 MiB of content |
| control (source, repack out of reach) | 0.1 MiB | 0.3 MiB | 5.1x | none |

**The ratio is the least useful number here, and it misleads in both
directions.** The source arm's 67x is enormous but the absolute cost is
3.6 MiB; the blob arm's 3.4x is modest but the absolute cost is
146 MiB. The honest statement is the one both share: **every 24 pushes,
a repository re-uploads all of itself.** Scale that by the repository,
not by the ratio.

The control's 5.1x floor is git's, not the repack's: every push
rewrites the tree of the directory it touched, so a 2 KiB edit
legitimately ships ~9 KiB of pack. Per-push volumes in the control and
the source arm agree to within a few bytes until the repack fires. A
first draft of the analyzer failed the control for not being 1.0x,
which would have had me "fix" a rig that was right.

## What `--geometric` would cost instead

Pure git, no syncer change: take each arm's repository and compare what
`repack -a -d -b` rewrites against `repack --geometric=2 -d
--write-midx`. What a repack rewrites is what forge then uploads.

| shape | full repack rewrites | geometric rewrites |
|---|---|---|
| source | 3.1 MiB (all of it) | **0.0 MiB** |
| blob | 156.1 MiB (all of it) | **12.0 MiB** |

Geometric rolls up only the small packs each push leaves and leaves the
big one alone, so what it rewrites is the increments rather than the
repository.

**Two traps this probe fell into first, both now closed.**

`git repack --geometric` REFUSES outright on a repository with
`pack.writeBitmaps` on — *"Incremental repacks are incompatible with
bitmap indexes"* — and forge has it on for the clone path. The first
version of this probe sent that fatal to `/dev/null` and reported
"geometric rewrites 0.0 MiB", which read as a total win and was in fact
the command never running. It now fails the leg instead.

And the progression is over **object counts, not bytes**. A synthetic
repository of a few large blobs has a low object count, so the
progression does not hold and geometric rolls up everything exactly as
the full repack does — which is what an early hand-test showed, on a
shape forge never actually has. Both shapes are measured here because
one of them would have answered the wrong question.

## What geometric would cost the design

It is not a drop-in. `--write-midx` leaves:

    multi-pack-index                              <- FIXED NAME, MUTABLE
    multi-pack-index-<hash>.bitmap
    pack-<hash>.{pack,idx,rev}                    x N

The `multi-pack-index` is a mutable file at a fixed key, and forge's
whole pack-directory model is "immutable, content-named, unconditional
PUT" — the sweep's four rules rest on it (`packio`, design §10). A
mutable object there would be a second mutable object beside the
snapshot, and one that must stay consistent with the pack set the
snapshot names or a restore gets an index naming packs the bucket does
not hold.

The cheap resolution is not to upload it at all: the MIDX and its
bitmap are derived, and a restore can rebuild them locally with `git
multi-pack-index write --bitmap` once the packs are down. That trades
restore CPU for keeping the bucket model intact. Not built; recorded.
