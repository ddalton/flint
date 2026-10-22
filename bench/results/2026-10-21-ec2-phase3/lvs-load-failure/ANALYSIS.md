# bench4 2026-10-21: lvstore fails to load after an spdk-tgt restart

## Symptom

16:07Z, bench4 (3 x i4i.2xlarge), Flint chart 1.60.0 as released
(spdk-tgt 1.7.0 = SPDK v26.05 + Flint patches, no parallel-alloc patch;
lvstores at 128 KiB clusters, md ratio 13). After a graceful spdk-tgt
restart, the lvstore on bench4-cp-1 and bench4-aws-1 failed to load:

    blobstore.c: 976:blob_parse: *ERROR*: Blobid (0x100000000) doesn't match what's in metadata (0x100000001)

bench4-aws-2 loaded fine. The disks had held one r3 100 GiB volume
(legs on all three nodes, raid on aws-2) through 4 epoch cuts under fio
load; the volume and its epochs were deleted before the restart.

## On disk (facts, from head16M-<node>.bin.gz in this directory)

- Super block identical on all three nodes, crc valid: clean=1,
  super_blob=0x100000000, cluster_size=131072, md_start=552 (byte
  0x228000), md_len=1,859,664; used_page mask at page 1 (57 pages),
  used_cluster mask at page 58 (437), used_blobid mask at page 495 (57).
- Masks identical: md-page and blobid masks have only bit 0 set; the
  used_cluster mask has exactly the md region set (every lvol and epoch
  had been deleted). md pages 1..3543 are zero.
- aws-2, md page 0: blob 0x100000000, the lvstore super blob (xattrs
  uuid, name=lvs_bench4-aws-2_0000-00-1f-0), empty extent table. Correct.
- cp-1 and aws-1, md page 0: an EXTENT PAGE of blob 0x100000001 (the
  replica lvol; its id survives snapshots), seq 0, next=0xffffffff, valid
  crc32c, one non-zero entry of 512:
  - cp-1: start cluster 392192, entry [21] = cluster 1819445
  - aws-1: start cluster 230400, entry [386] = cluster 2166135
- Not stale data from an earlier install: wipe-disks.sh zeroes the first
  and last 64 MiB; page 0 carries this lvstore's geometry, so the 1.7.0
  image wrote it between ~15:44 and 16:06.

## Mechanism (stock SPDK v26.05, verified in lib/blob/blobstore.c)

1. `bs_allocate_cluster` (I/O thread, ~L177) claims a new extent page
   only if the blob's extent-table slot is 0 at that moment ("Extent page
   shall never occupy md_page so start the search from 1"); otherwise the
   allocation's `ctx->extent_page` stays 0.
2. An epoch snapshot runs `bs_snapshot_swap_cluster_maps`: the live clone
   receives the snapshot's all-zero cluster and extent-page arrays.
   `blob_freeze_io` is a barrier for NEW I/O; it does not wait for
   copy-on-write cluster allocations already in flight. Neither the lvol
   layer nor Flint quiesces before a cut (epoch_scheduler.rs).
3. The in-flight allocation's insert, `blob_insert_cluster_msg` (md
   thread, L9092), now finds the slot 0 and writes the extent page to
   `ctx->extent_page` -- which is 0. The only guard is
   `assert(ctx->extent_page != 0)`, compiled out in release builds. The
   extent page lands on md page 0, the lvstore super blob.
4. Nothing notices while running. The next load parses md page 0 as the
   super blob, finds blob 0x100000001's extent page, and the whole
   lvstore (every replica on the node) is gone until page 0 is repaired.

The on-disk page is exactly what this predicts: the clone's id, ONE
entry (the map had just been emptied by the swap). Other writers ruled
out: the free path would show the cluster removed; snapshot-delete would
carry a snapshot's id; retention 6 was never reached.

## Why now

No new code path. F74 change 1 (128 KiB clusters) makes the window
common: with 1 MiB clusters a random-write working set is fully
allocated within seconds of a cut, so little is in flight at the next
cut; at 128 KiB (737k clusters for 90 GiB) every first write after a
cut is a COW allocation and many are in flight when the next cut lands.
The md ratio is irrelevant (page 0 is chosen because the index is 0).
F74 change 2 (parallel allocations, up to 32 in flight per channel)
would widen the window further. SPDK master still has this code
(per the forensics read; to confirm before proposing upstream).

bench3's F74 runs (2026-10-08, 128 KiB, f74c2, 300 s epochs) very likely
hit it unnoticed: disks were wiped between arms and spdk-tgt never
reloaded a used lvstore.

## The failed load did not make it worse

A clean load writes no md pages; its unload rewrote the masks it had
read and the super block (clean=1). Only md page 0 is wrong; it can be
rebuilt from aws-2's layout (own name/uuid, crc32c as SPDK computes it).
Not needed here: nothing is deployed (user, 2026-10-21), the bench data
is disposable.

## Fix direction

- `blob_insert_cluster_msg`: when the slot AND `ctx->extent_page` are 0,
  claim a fresh md page on the md thread (under used_lock, search from 1)
  before writing.
- `blob_write_extent_page` (and any md page writer): refuse page 0 with
  an error instead of writing it -- an error, not a legal value.
- Unit test (blob_ut, after `blob_insert_cluster_msg_test`): super blob
  A on md page 0; thin blob B with extent table, write cluster 0;
  `bs_allocate_cluster(B, 1, ...)` returns extent page 0 (slot occupied);
  snapshot B; `blob_insert_cluster_on_md_thread(B, 1, ..., 0, ...)`;
  reload; open A must succeed. Fails (assert / wrong page) before the
  fix, passes after.
