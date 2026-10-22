# Flint's SPDK patch series

The spdk-tgt image (`docker/Dockerfile.spdk`) is upstream SPDK at the tag in
[`BASE`](BASE) plus these patches, applied in order with `git apply`. Each
patch is a `git format-patch` commit: its message says what it changes, why,
and where it stands upstream.

Never edit a patch file by hand. Work on the series in a scratch clone of
upstream SPDK (no fork) with [`scripts/spdk-patches.sh`](../../scripts/spdk-patches.sh):

```sh
scripts/spdk-patches.sh tree /path/spdk         # SPDK at BASE + the series, one commit per patch
#   ... amend a commit, or commit a new patch on top ...
scripts/spdk-patches.sh test /path/spdk         # unit tests: blob, lvol, vbdev_lvol, raid, raid1
scripts/spdk-patches.sh export /path/spdk       # rewrite this directory (and BASE)
scripts/spdk-patches.sh check                   # applies to BASE exactly as the image build does
```

`test` needs SPDK's build dependencies (its `scripts/pkgdep.sh`; the
image's builder installs them). On a host without root, a missing Python
module can come from the distribution's package unpacked into a scratch
directory: `apt-get download python3-tabulate && dpkg-deb -x python3-tabulate_*.deb DIR`,
then `PYTHONPATH=DIR/usr/lib/python3/dist-packages`.

Upgrading SPDK is `tree /path/spdk v26.09`. `git am -3` merges a patch
whose surrounding code moved and stops on a real conflict (resolve it, then
`git -C /path/spdk am --continue`). A patch that upstream has merged comes
out empty (`git -C /path/spdk am --skip`) and drops out of the series. Then
`test`, `export`, review the diff of this directory, and run the release
gates (the restart gate and the EC2 drills) on the new image.

Any change here changes the spdk-tgt image: `scripts/release.sh` refuses a
release whose chart still pins an spdk-tgt tag older than the last change
to this directory or the Dockerfile.

## The patches

| # | Patch | Kind | Upstream |
|---|---|---|---|
| 0001 | nvmf: name the listener address when a host is refused | diagnostics | Flint only |
| 0002 | lvol: FLUSH persists the blob's metadata (F5) | fix | candidate, not filed |
| 0003 | ublk: log whether the bdev takes FLUSH | diagnostics | Flint only |
| 0004 | blob: batched metadata reads in bs_recover (F5) | performance | candidate, not filed |
| 0005 | blob: log blobstore unload start and completion | diagnostics | Flint only |
| 0006 | bdev/raid: skip_rebuild on add, leased quiesce RPCs | feature (hot rejoin) | Flint only; 26.09 has no equivalent |
| 0007 | blob: up to 32 cluster allocations in flight per channel (F74) | performance | candidate, not filed |
| 0008 | blob: never write an extent page over md page 0 | fix | [Gerrit 29555](https://review.spdk.io/c/spdk/spdk/+/29555), spdk/spdk#3113 |
| 0009 | blob: ops that reach a cluster during its allocation wait their turn | fix | [Gerrit 29556](https://review.spdk.io/c/spdk/spdk/+/29556), spdk/spdk#2144 (the unmap part is Flint's) |
| 0010 | blob: zero a cluster before mapping it unwritten | fix (data exposure) | candidate, not filed |
| 0011 | bdev/uring: WRITE_ZEROES through the kernel's zeroout | performance | candidate, not filed; see Gerrit 28716 |
| 0012 | blob: a write that covers a new cluster is the cluster's only write | performance (0010's cost) | candidate with 0010, not filed |

The order is the order the patches were written in. Several touch
`lib/blob/blobstore.c` and depend on the ones before them, so the numbers
are not grouped by kind; this table is. Status of what we filed:
[`docs/spdk-upstream-tracking.md`](../docs/spdk-upstream-tracking.md).

Until 2026-10-22 these were loose files in `spdk-csi-driver/`
(`nvmf-hostlog.patch` ... `blob-alloc-window-order.patch`), each applied by
its own `patch -p1` line in the Dockerfile. Each commit message names its
old file. The conversion was checked byte for byte: v26.05 plus `git apply`
of this series gives the same tree as v26.05 plus `patch -p1` of the old
files.

`test/unit/lib/nvmf/ctrlr.c/ctrlr_ut` does not link with 0001 (it needs a
`spdk_nvme_transport_id_trtype_str` stub). The image is built without unit
tests, and `spdk-patches.sh test` does not run that suite.
