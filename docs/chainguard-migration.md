# Migrating the flint images to Chainguard

Status: planning / recommendations. Written from a read of the current
`spdk-csi-driver/docker/Dockerfile.*` and the chart image references. No image
changes have been made yet.

## Goal

The chart images are published to Docker Hub under `dilipdalton/*`
(`flint-lean-operator`, `flint-s3-csi`, `flint-s3-worker`,
`flint-s3-worker-lean`, `flint-sync`, `flint-passthrough-mounter`,
`flint-forge-operator`). We want to base them on
[Chainguard](https://images.chainguard.dev/) images (Wolfi/apko, minimal,
CVE-managed, nonroot) to shrink the attack surface and drop the CVE backlog that
comes with `ubuntu:24.04`.

## Why the current Dockerfiles are already well-positioned

- **They are `*.prebuilt`:** each Dockerfile just `COPY`s a pre-built binary
  into a runtime base (from `docker/prebuilt/${TARGETARCH}/`). There is **no
  in-image compilation to port** — the runtime base can be swapped freely.
- **The Rust binaries are built musl-static** (`Makefile`: `cargo zigbuild
  --target <arch>-unknown-linux-musl`). A static binary is exactly what
  `cgr.dev/chainguard/static` wants — no libc, no shell, distroless.
- **Entrypoints are exec-form** (`ENTRYPOINT ["/usr/local/bin/…"]`) — no shell
  dependency, so distroless bases work.
- **The operators already run nonroot** (`USER 65532:65532`), which matches
  Chainguard's nonroot uid.

So this is mostly a **base-image swap + a few build hygiene changes**, not a
rewrite.

## The one variable that decides the target base: static vs dynamic

Before choosing a base, confirm how each binary is linked:

```sh
file docker/prebuilt/amd64/flint-lean-operator     # "statically linked" ⇒ chainguard/static
ldd  docker/prebuilt/amd64/flint-lean-operator     # "not a dynamic executable" ⇒ static
```

- **musl-static** → `cgr.dev/chainguard/static:latest` (smallest, distroless, nonroot, CA certs baked in).
- **glibc-dynamic** → `cgr.dev/chainguard/glibc-dynamic:latest`.

Standardising every controller binary on musl-static (the build already does
this for some) is the single biggest enabler — it lets every pure-controller
image use `chainguard/static`.

## Per-image plan (grouped by what the runtime actually needs)

| Image(s) | Current Dockerfile | Runtime needs | Chainguard target | Effort |
|---|---|---|---|---|
| `flint-lean-operator`, `flint-forge-operator` (operator + gateway binaries) | `Dockerfile.operator.prebuilt`, `Dockerfile.forge-operator.prebuilt` — `FROM ubuntu:24.04`, `apt ca-certificates`, `USER 65532` | just CA certs | **`cgr.dev/chainguard/static:latest`** (certs baked in — drop the apt line) | **easy — do first** |
| `flint-s3-csi` (carries `flint-s3-csi-node` **and** `flint-s3-broker`) | `Dockerfile.s3csi.prebuilt` — `FROM ubuntu:24.04` + `util-linux mount e2fsprogs ca-certificates`, runs **root** (CSI node needs it) | mount tooling | **`cgr.dev/chainguard/wolfi-base`** + `apk add --no-cache util-linux e2fsprogs mount ca-certificates-bundle` | medium |
| `flint-sync` | `Dockerfile.sync.prebuilt` — `FROM alpine:3.20` (musl), `apk add ca-certificates` | CA certs (+ musl already) | `chainguard/static` if it needs no shell, else `chainguard/wolfi-base` | easy |
| `flint-passthrough-mounter` (base of `flint-s3-worker`) and the worker images | `Dockerfile.s3worker` / `-lean` — `FROM ${MOUNTER_IMAGE}` / `${SYNC_IMAGE}`, expect `/usr/bin/mount-s3` | **`mount-s3` (mountpoint-s3) + libfuse** | `chainguard/wolfi-base` + `apk add fuse` (or `fuse3`), then `COPY` the pinned `mount-s3` binary in | **hardest — do the mounter base first; the two worker images inherit it** |

Notes:
- The `flint-s3-csi` image can't be fully distroless because the **CSI node needs
  `mount`/`util-linux`/`e2fsprogs`**. Use `wolfi-base` (has `apk`), not `static`.
  It stays root — that's correct for a CSI node plugin.
- The **workers** are only as Chainguard-clean as the **mounter/sync base**. The
  FUSE mounter is the real puzzle: `mount-s3` is a dynamically-linked binary that
  needs libfuse at runtime. Convert `flint-passthrough-mounter` (wolfi-base +
  `fuse` + the `mount-s3` binary) first; `flint-s3-worker` then just `COPY`s the
  Rust worker on top as today.

## Dockerfile changes that make the migration mechanical

1. **Parameterise the base image.** Replace the hardcoded `FROM ubuntu:24.04` /
   `FROM alpine:3.20` with:
   ```dockerfile
   ARG BASE_IMAGE=cgr.dev/chainguard/static:latest
   FROM ${BASE_IMAGE}
   ```
   Then the base is a build-arg, so a rollback or an air-gapped mirror is a `--build-arg`, not an edit.

2. **Drop packages the Chainguard base already ships.** `chainguard/static` and
   `glibc-dynamic` include CA certificates — remove the `apt-get install
   ca-certificates` / `apk add ca-certificates` steps for the controller images.
   Keep only genuinely-needed packages (mount tooling on the CSI node; fuse on the mounter) via `apk` on `wolfi-base`.

3. **Standardise on musl-static for controller binaries** so they land on
   `chainguard/static`. If a binary must stay glibc-dynamic, target
   `chainguard/glibc-dynamic` instead — don't force it onto `static`.

4. **Keep exec-form `ENTRYPOINT`** (already done) — required for distroless (no shell).

5. **Set `USER 65532:65532`** on every non-privileged image (operators already
   do; add it to `flint-sync`). It matches Chainguard's `nonroot` user. Leave the
   CSI node as root.

6. **Pin by digest** for reproducibility once the tag is chosen
   (`cgr.dev/chainguard/static:latest@sha256:…`); Chainguard tags move frequently.

7. **Multi-arch** carries over unchanged — the Dockerfiles already key on
   `TARGETARCH`, and Chainguard images are multi-arch.

## Suggested order

1. **Operators/gateways/broker → `chainguard/static:nonroot`.** Trivial once the
   binaries are confirmed musl-static; drop the apt CA-cert step; already nonroot.
2. **`flint-s3-csi` node image → `chainguard/wolfi-base`** + `apk add util-linux
   e2fsprogs mount`. Stays root.
3. **`flint-passthrough-mounter` (FUSE/mount-s3) → `wolfi-base` + `fuse`**, copy
   `mount-s3` in. Then `flint-s3-worker` / `-lean` inherit it with no further change.

## Example: the easy class (operator image)

```dockerfile
# Dockerfile.operator.prebuilt  (Chainguard variant)
ARG BASE_IMAGE=cgr.dev/chainguard/static:latest
FROM ${BASE_IMAGE}
ARG BIN_DIR=docker/prebuilt
ARG TARGETARCH
# CA certs are already in the base; no package manager step.
COPY ${BIN_DIR}/${TARGETARCH}/flint-lite-operator  /usr/local/bin/flint-lite-operator
COPY ${BIN_DIR}/${TARGETARCH}/flint-hub-gateway     /usr/local/bin/flint-hub-gateway
COPY ${BIN_DIR}/${TARGETARCH}/flint-lean-operator   /usr/local/bin/flint-lean-operator
COPY ${BIN_DIR}/${TARGETARCH}/flint-lean-gateway    /usr/local/bin/flint-lean-gateway
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/flint-lite-operator"]
```
(Requires the four binaries built musl-static; otherwise use
`cgr.dev/chainguard/glibc-dynamic`.)

## Example: the CSI node image (needs a shell/apk)

```dockerfile
# Dockerfile.s3csi.prebuilt  (Chainguard variant)
ARG BASE_IMAGE=cgr.dev/chainguard/wolfi-base:latest
FROM ${BASE_IMAGE}
ARG BIN_DIR=docker/prebuilt
ARG TARGETARCH
RUN apk add --no-cache util-linux mount e2fsprogs ca-certificates-bundle
COPY ${BIN_DIR}/${TARGETARCH}/flint-s3-csi-node /usr/local/bin/flint-s3-csi-node
COPY ${BIN_DIR}/${TARGETARCH}/flint-s3-broker   /usr/local/bin/flint-s3-broker
ENTRYPOINT ["/usr/local/bin/flint-s3-csi-node"]
```

## Registry note

The charts now take a `global.imageRegistry` override (see
`flint-*-chart/values.yaml`) instead of hardcoding a host, so once these images
are republished (Chainguard-based, to whatever registry), consumers point at them
by setting that one value — no Dockerfile or chart edit required.
