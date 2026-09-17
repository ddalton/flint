# Chainguard base images for the flint images

Status: **Dockerfiles converted, built and smoke-tested locally on linux/arm64
and linux/amd64 (2026-09-16). Not yet published and not yet run on a cluster.**
The canonical in-container-compile twins (`Dockerfile.csi`, `Dockerfile.pnfs`)
carry the same runtime stage but were not built here; they run cargo inside the
build.

## Goal and what did not change

Cut the CVE surface of every image flint ships by moving its runtime base from
`ubuntu:24.04` / `debian:bookworm-slim` / `alpine:3.20` to a
[Chainguard](https://images.chainguard.dev/) base (Wolfi, CVE-managed).

The binaries are **unchanged**: no build, linkage or crate change. Every
`*.prebuilt` Dockerfile only `COPY`s host-cross-compiled binaries, and every one
of them is already musl-static (`file docker/prebuilt/{amd64,arm64}/*` →
"statically linked" for all 15), so the static-vs-dynamic question the first
draft of this doc raised is settled: a base provides only certs, tools and a
filesystem. Entrypoints were already exec-form.

## Per-image result

Two bases, both pinned by multi-arch **index** digest as
`ARG BASE_IMAGE=cgr.dev/chainguard/<name>:latest@sha256:…` before `FROM
${BASE_IMAGE}` (override with `--build-arg BASE_IMAGE=<mirror>`):

- `cgr.dev/chainguard/static` — distroless: no shell, no apk, CA bundle, nonroot
  passwd entry.
- `cgr.dev/chainguard/wolfi-base` — glibc, busybox, apk, CA bundle.

| Image | Recipe | Base | Packages added | Why not `static` |
|---|---|---|---|---|
| `flint-lite-operator` (alias `flint-lean-operator`) | `Dockerfile.operator.prebuilt` | static | — | — |
| `flint-forge-operator` | `Dockerfile.forge-operator.prebuilt` | static | — | — |
| `flint-sync` | `Dockerfile.sync.prebuilt` | wolfi-base | `ca-certificates-bundle` | It is the base of `flint-s3-worker-lean`, and the s3csi rigs exec `sh -c`, `ps \| grep`, `cat`, `sed`, `tr`, `test -f` inside lean workers |
| `flint-s3-csi` (node plugin **and** broker, one image) | `Dockerfile.s3csi.prebuilt` | wolfi-base | `mount umount e2fsprogs ca-certificates-bundle` + ext4 format pin | The node execs util-linux `mount -o loop` and `mkfs.ext4` (`s3csi/quota.rs`). The broker shares the image, so it cannot move to `static` on its own |
| `flint-passthrough-mounter` | `Dockerfile.passthrough` | wolfi-base | `fuse2 ca-certificates-bundle`; mount-s3 from AWS's tarball (sha256-pinned) | mount-s3 is a glibc binary linking `libfuse.so.2`; rigs exec `sh`/`grep`/`tr` in passthrough workers |
| `flint-s3-worker` | `Dockerfile.s3worker` | FROM `flint-passthrough-mounter` | — (unchanged recipe) | inherits |
| `flint-s3-worker-lean` | `Dockerfile.s3worker-lean` | FROM `flint-sync` (**not** the mounter) | — (unchanged recipe) | inherits |
| `flint-forge-git` | `Dockerfile.forge-git` | wolfi-base | `git git-lfs git-daemon ca-certificates-bundle` | git + http-backend are packages; agents run a shell here |
| `flint-forge-syncer` | `Dockerfile.forge-syncer.prebuilt` | wolfi-base | `ca-certificates-bundle git` | every object-database operation is a `git` subprocess |
| `flint-driver` | `Dockerfile.csi.prebuilt` (+ twin `Dockerfile.csi`) | wolfi-base | `bash coreutils nvme-cli util-linux util-linux-misc mount umount blkid blockdev findmnt lsblk losetup wipefs e2fsprogs e2fsprogs-extra xfsprogs xfsprogs-core xfsprogs-extra nfs-utils rpcbind pciutils curl ca-certificates-bundle libcap-utils` + ext4/XFS format pins; `setcap` unchanged | the block driver execs ~20 tools |
| `flint-pnfs` | `Dockerfile.pnfs.prebuilt` (+ twin `Dockerfile.pnfs`) | wolfi-base | `bash coreutils util-linux util-linux-misc mount umount blkid blockdev findmnt lsblk losetup wipefs nfs-utils rpcbind e2fsprogs e2fsprogs-extra xfsprogs xfsprogs-core xfsprogs-extra ca-certificates-bundle curl krb5 tcpdump libcap-utils` + format pins; `setcap` unchanged | NFS/Kerberos/tcpdump tooling; pnfs-bench rigs `kubectl exec … bash -lc` |

Wolfi splits packages more finely than Debian: `util-linux` alone installs
neither `mount` nor `umount`, and `resize2fs`/`dumpe2fs`, `xfs_growfs` and
`mountpoint`/`nsenter` live in `e2fsprogs-extra`, `xfsprogs-extra` and
`util-linux-misc`. The driver and pnfs builds therefore end with a `command -v`
loop over every tool the code execs (census: `grep -rhoE 'Command::new\("[^"]+"\)'
spdk-csi-driver/src`) plus what ubuntu gave a human or a rig, and fail on any
miss. `coreutils` is GNU on purpose: `mount_util.rs` reads `timeout`'s exit
124 as TimedOut.

Not converted, on purpose: `Dockerfile.csi-prebuilt` (it is `FROM` a previously
published `flint-driver` tag and inherits whatever base that tag has), the SPDK
Dockerfiles, test-rig Dockerfiles (`Dockerfile.c6gates`, `Dockerfile.stub`) and
the **builder** stages of `Dockerfile.csi`/`Dockerfile.pnfs` (`rust:1.92-alpine`,
`rust:1.90-slim-bookworm`), which are not shipped. Replacing them would change
the build, which was out of scope.

## Behaviour differences from the previous images

- **Default user.** Images that were root stay root (`Config.User` is now `"0"`
  where it was `""`; same uid). Both operator images still set `65532:65532`.
  Worker pods set `runAsUser` explicitly (`s3csi/worker.rs`), so neither value
  reaches a worker.
- **No shell in the operator images** (`static`). Nothing execs one there; the
  one rig exec into the gateway runs the binary directly
  (`tests/regression/agent-fleet-doc-drill.sh`).
- **git 2.45.4 → 2.55.0, git-lfs 3.5.1 → 3.8.0** in `flint-forge-git` and
  `flint-forge-syncer`. Above the 2.43 floor, but a real upgrade that forge's
  e2e must cover before release.
- **busybox userland** replaces Debian's coreutils in the passthrough mounter
  (alpine's sync/forge images were busybox already). The rig commands were run
  in the old and the new images with identical results.
- **mount-s3** now comes from AWS's release tarball instead of the `.deb` (apk
  cannot install a `.deb`). For 1.24.0, `bin/mount-s3` is byte-identical on
  both arches (sha256 `77fcd881…` arm64, `5b63656f…` amd64), the layout
  (`/opt/aws/mountpoint-s3` + `/usr/bin/mount-s3`) is kept, and a real FUSE
  mount of a public bucket behaves the same in the old and new mounter.
- **Sizes** (docker-reported, arm64). Shrank: lite-operator 179 → 78 MB,
  driver 249 → 148 MB, pnfs 299 → 187 MB, s3-csi 137 → 56 MB, mounter
  185 → 88 MB. Grew: forge-git 55 → 97 MB and forge-syncer 59 → 92 MB
  (Wolfi's git is glibc with fuller dependencies), flint-sync 28 → 34 MB.

## Filesystem format pins (driver, pnfs, s3-csi)

A filesystem these images make is mounted by the **node's** kernel. Wolfi's
e2fsprogs 1.47.4 and xfsprogs 7.1.1 default to features that ubuntu's 1.47.0 and
6.6.0 did not enable, and that older kernels refuse:

| Tool | New default feature | Kernel needed |
|---|---|---|
| mkfs.ext4 | `orphan_file`, `metadata_csum_seed` | 5.15 / 4.4 |
| mkfs.xfs | `nrext64`, `exchange`, `parent` | 5.19 / 6.10 / 6.12 |

A fresh volume or quota image made with those defaults would fail to mount on
an older node (EKS AL2 runs 5.10). The images pin the previous feature sets:

- ext4: the `features =` line in `/etc/mke2fs.conf` is rewritten to ubuntu
  1.47.0's.
- XFS: mkfs.xfs has no default config file, only `-c options=`, so
  `/usr/local/bin/mkfs.xfs` (first on `PATH`) execs `/usr/bin/mkfs.xfs -c
  options=/etc/flint/mkfs.xfs.conf "$@"`. That profile has ubuntu 6.6.0's
  feature set (rmapbt/reflink/inobtcount/bigtime on; nrext64/exchange/parent off).
  A caller passing its own `-c`, or `mkfs -t xfs` (which searches `/sbin`
  first), bypasses it. No such caller exists today.
- Each build formats a scratch image with both tools and fails if the features
  drift. Positive controls: unwrapped `/usr/bin/mkfs.xfs` and Wolfi's original
  ext4 line both make the gate refuse.
- Verified: s3-csi quota images made with the exact `quota.rs` arguments show a
  `dumpe2fs` feature list identical to the old image's. XFS's `xfs_db version`
  matches except for the legacy `ATTR2` superblock bit, which xfsprogs ≥ 7 no
  longer sets and no option restores (V5 implies attr2). Mounting on a
  pre-5.18 kernel was **not** tested: the local VM runs 7.0.

## Keeping the pins current (per release)

A digest pin is reproducible and it also freezes the base, and a frozen base
ships last month's CVEs. **Each release refreshes the pins, then rebuilds and
retests:**

```sh
scripts/refresh-chainguard-bases.sh --check   # 0 current, 1 behind/unpinned, 2 registry error
scripts/refresh-chainguard-bases.sh           # rewrite stale digests in place
```

It resolves each `ARG BASE_IMAGE=cgr.dev/chainguard/<name>:<tag>` line's current
index digest with `docker buildx imagetools inspect` (a registry read, no pull),
requires linux/amd64 and linux/arm64 in the index, and rewrites only the digest.
A resolution failure exits 2 and is never reported as "current". It is not
wired into `scripts/release.sh`.

The free `cgr.dev/chainguard/*` tier serves `:latest` (and `-dev`) tags. There is
**no `:nonroot` tag** (`static:nonroot` is not found; that is gcr.io
distroless's naming). `static:latest` already runs as 65532.

**Bumping mount-s3** means `MOUNT_S3_VERSION` plus both
`MOUNT_S3_SHA256_{AMD64,ARM64}` (sha256 of
`mount-s3-<ver>-<x86_64|arm64>.tar.gz`) in `Dockerfile.passthrough`. A version
bumped without its checksums fails the build.

## Constraints on editing these recipes

`scripts/release.sh` greps recipe content, and each check still passes against
the converted files:

- `Dockerfile.operator.prebuilt` / `Dockerfile.forge-operator.prebuilt` must name
  `/usr/local/bin/<bin>` for each binary the charts exec.
- `Dockerfile.sync.prebuilt` must mention `ca-certificates`. It does so because it
  genuinely installs `ca-certificates-bundle`. That check's comment still gives
  the old reason for needing a shell (an injected `test -f` startupProbe that
  left with the webhook in v1.45.0); the need is still real, for the rig
  reason above.
- `Dockerfile.passthrough` must contain `mount-s3` and a line matching
  `^ARG MOUNT_S3_VERSION=[0-9]`.
- `Dockerfile.s3worker` / `Dockerfile.s3worker-lean` must name
  `flint-passthrough-mounter:<app>` / `flint-sync:<app>` in their `ARG
  MOUNTER_IMAGE=` / `ARG SYNC_IMAGE=` defaults. `publish-images.sh` passes the
  release's own tag as a build-arg, and the version bump edits those lines by hand.

`publish-images.sh` builds each arch with `docker build --platform linux/<arch>`.
The mounter's download stage runs on `$BUILDPLATFORM`, so an amd64 build on an
arm64 host fetches natively.

## Verification done (local, Docker Desktop, arm64 host)

Every converted recipe built for linux/arm64 and linux/amd64. Per image:
entrypoint binary starts on both arches; `Config.User`/entrypoint/env compared
with the published `dilipdalton/*:1.55.0`; CA bundle present (119 certs in
`static`). Specific checks:

- flint-sync over HTTPS to real S3 through rustls-native-certs → `403
  InvalidAccessKeyId`, the same as the old image; with the bundle deleted →
  `dispatch failure` (so the bundle is load-bearing).
- s3-csi: privileged `mkfs.ext4` + `mount -o loop,noatime` + write + `umount`.
- mounter: `mount-s3` linkage (control: libfuse removed → exit 127), FUSE mount
  of `noaa-ghcn-pds`, `fusermount -u`.
- workers: both start and listen on the comm socket.
- forge-git: clone through `flint-forge-gitcgi` → `git http-backend`, HTTPS
  `ls-remote`, `git propose`, hooks.
- driver/pnfs: `getcap` identical, file-cap binaries exec as 65532, GNU
  `timeout` → 124, XFS/ext4 loop mounts, nvme/lspci, kinit/klist/tcpdump/bash.

**Not done:** a cluster run. Before publishing, run at least the s3csi e2e
(`s3csi/e2e/run-s3csi.sh`, quota and lean-worker legs), forge e2e (git 2.55), a
pNFS drill, and a block-driver format+mount on the oldest node kernel supported.

## CVE effect

`docker scout quickview --platform linux/amd64`, published
`dilipdalton/<image>:1.55.0-amd64` vs the local Chainguard build of the same
recipe and binaries (2026-09-16):

| Image | Before (C / H / M / L) | After | Packages indexed |
|---|---|---|---|
| flint-passthrough-mounter | 3 / 13 / 10 / 49 | 0 / 0 / 0 / 0 | 157 → 33 |
| flint-s3-worker | 3 / 13 / 10 / 49 | 0 / 0 / 0 / 0 | 157 → 33 |
| flint-s3-csi | 0 / 0 / 13 / 2 | 0 / 0 / 0 / 0 | 132 → 49 |

Scout attributed the mounter's findings to its Debian 12 base (base alone: 2 /
10 / 8 / 13, plus 4 unspecified) and s3-csi's to `ubuntu:24.04` (0 / 0 / 13 / 2).
These are **base-package** counts: the flint binaries and `mount-s3` are the
same bytes on both sides, and Scout does not scan their Rust dependencies, so a
Rust-crate CVE would not show in either column. Other images were not measured
for amd64; an earlier arm64 pass, stopped part-way, gave lite-operator and
forge-operator 0/0/13/2 → 0/0/0/0, flint-sync 0/0/1/0 → 0/0/0/0, and
s3-csi 0/0/13/2 → 0/0/0/0.

## Registry note

All seven charts take `global.imageRegistry`, so republished images in any
registry are one value away. No Dockerfile or chart edit is needed.
