# mount-s3 1.24.0 against Apache Ozone 2.2.1's S3 gateway — 2026-09-30

The adoption question the passthrough review left open: does the pinned
Mountpoint mount an Ozone S3 gateway at all, and with which flags. Run
on the Linux box (8 cores, 30 GiB), Ozone 2.2.1's own `compose/ozone`
environment (om, scm, one datanode, s3g on `127.0.0.1:9878`, no
security: any key is accepted), mount-s3 1.24.0 from AWS's x86_64
tarball run on the HOST — the same binary and the same argument shapes
the passthrough worker execs (`--endpoint-url`, `--force-path-style`,
`--read-only` / `--allow-delete --allow-overwrite`, `--prefix`,
`--cache … --max-cache-size 768`), minus `--allow-other` (the box's
fuse.conf does not permit it for a user; the worker runs as the owner).
Scripts and logs beside this file.

| probe | result |
|---|---|
| read-only mount of `datasets/`, `ls` of a 10-key directory | 10 entries, content and size correct, mode 644 |
| a 300-key directory under one prefix (a paginated ListObjectsV2) | all 300 listed, a key in the middle reads |
| read-write mount, 48 MiB written by `head -c … >` (a 6-part multipart) with the DEFAULT upload checksums (CRC32C trailing) | landed whole: 50331648 bytes, ETag `…-6`, no multipart upload left pending, nothing in the mount-s3 log |
| the same with `--upload-checksums off` | landed whole, ETag `…-6` |
| a small single-PUT write, then `rm` | both worked (`delete rc=0`, the key is gone) |
| `--cache`: cold then warm read of a 128 MiB object, md5 against the seed | 2068 ms cold (Ozone's six JVMs on the box's cores), 221 ms warm from the cache, md5 equal |

**Flags needed: none beyond what the CR already implies.** `endpoint`
set ⇒ path-style; any `region` string. Ozone answers HEAD with no
`x-amz-checksum-*` and omits `KeyCount` from listings; mount-s3 relies
on neither. The 09-12 lean probe's finding that Ozone is checksum-blind
(it accepts a wrong CRC32) still stands and is invisible to a reader.

**A rig finding, not a product one:** MinIO's `mc` client cannot list a
folder on Ozone ("This part of feature is not implemented yet") and its
`pipe` did not land objects, so the s3csi rig's seed job — which uses
`mc` — cannot seed an Ozone store as it is; the AWS CLI seeded it fine.
The CSI path itself (a FlintPassthroughMount with `endpoint:
http://<ozone-s3g>:9878`, broker `static`) adds nothing to the mounter
that this host run did not exercise; running it end to end needs the
rig's seed and its `mc ls` precondition made client-agnostic first.

Three runs of the first script died on rig mistakes before any of this
measured anything, kept in `oz-arm-writes.log`'s history for the
record: a two-character bucket name (mount-s3 wants 3-255), the
Chainguard image's entrypoint already being `mc` (so `mc mc …` was a
silent no-op), `--allow-other` without `user_allow_other`, and a failed
mount leaving files inside the mount directory.
