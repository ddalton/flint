# flint-lean: packaging and release

**Goal (user, 2026-08-26):** run flint-lean on **one or more Kubernetes
clusters**.

Today it does not install. Not "installs with rough edges" — the chart
execs a binary that is not in the image it pulls, and the sidecar it
injects has no published image at all. This plan is what stands between
the code (which is drilled hard: battery 101/101, formal 61/61, kind
14/14, bucket 27/27) and a cluster that can actually run it.

---

## 1. Why the drills did not catch this

Every lean drill builds its images from `lean/e2e/Dockerfile.*`, and
those recipes are **correct**. The shipped path is a different set of
recipes, and nothing exercises it. The three findings below all live in
that gap, and all three are invisible to a kind rig by construction.

`scripts/release.sh` cannot catch them either: **`grep -c flint-lean
scripts/release.sh` = 0.** The release gate — written after 1.2.0 shipped
a chart referencing an unpublished image — has no knowledge of lean.

## 2. The three findings

**F1 — the chart execs a binary that is not in its image.**
`flint-lean-chart/templates/deployment.yaml:20` runs
`/usr/local/bin/flint-lean-operator` from image
`dilipdalton/flint-lite-operator` (`values.yaml:17`, *"the lean operator
ships in this image"*). `spdk-csi-driver/docker/Dockerfile.operator.prebuilt`
copies `flint-lite-operator` and `flint-hub-gateway` — **not**
`flint-lean-operator`. Install today ⇒ CrashLoopBackOff, "no such file
or directory". `templates/gateway.yaml:26` has the same problem for
`flint-lean-gateway`.

*The webhook needs no separate answer:* it is served by the operator
binary itself (`flint_lean_operator.rs:278`) and self-provisions its TLS
secret and `MutatingWebhookConfiguration` at runtime
(`webhook::ensure_cert_secret` / `ensure_webhook_config`), which is why
the chart ships neither. One binary, three roles — reconcile, webhook,
stale-MPU sweep. Fixing F1 covers the webhook.

**F2 — `flint-sync` has no production image recipe.** The webhook injects
`dilipdalton/flint-sync` (`values.yaml:27`) into every workspace pod. No
Dockerfile outside `lean/e2e/` mentions it. It also builds from a
**different crate** (`lean/sidecar`) than the operator
(`spdk-csi-driver`), so a lean release cross-compiles two crates into one
staging tree.

**F3 — the sidecar needs a CA bundle, and the obvious base has none.**
`crates/flint-store` uses the AWS SDK's `rustls` feature, which resolves
to **`rustls-native-certs`** (`lean/sidecar/Cargo.lock:1720`) — the
**system trust store**, not bundled roots. A busybox or scratch
`flint-sync` fails against every HTTPS S3 endpoint. Every drill missed
this because MinIO was plain HTTP; it appears on the first real bucket.
The image must carry `ca-certificates`, **and** a shell: the injected
`startupProbe` execs `test -f <marker>` inside it (`values.yaml:23`), so
distroless-static silently fails the gate.

## 3. Packaging decisions

### 3.1 Where the operator and gateway binaries go

| | add to `flint-lite-operator` | new `flint-lean-operator` image |
|---|---|---|
| new machinery | none — image already released by `release.sh` | a fourth image to publish, scan, keep in step |
| matches chart default | yes (`values.yaml:17` already says so) | needs a values change |
| release cadence | **a lean fix needs a lite-operator release** | independent |
| honesty | lean users pull an image named "lite" | name matches contents |

`Dockerfile.operator.prebuilt` already argues for multi-binary packaging
("one fewer image to publish, sign, scan and keep in step at release …
this is packaging and not coupling"), and both binaries come from the
same crate.

**Recommendation: add to `flint-lite-operator` now, revisit if lean's
cadence diverges.** It is the smallest change that makes the chart
installable, and it is what the chart already claims. The cost is real
and should be recorded: lean cannot ship a fix without a lite-operator
image release, because the operator binary lives in the hub crate
(`spdk-csi-driver/Cargo.toml:82`).

### 3.2 The `flint-sync` image

Its own image — it is injected into user pods, so size matters and the
toolless/read-only-rootfs posture of the operator image is wrong for it.
Requirements, all load-bearing: a **shell** (probe), **ca-certificates**
(F3), and small. `alpine` or `ubuntu:24.04`-slim both satisfy it;
`busybox` satisfies the shell and fails the certs.

### 3.3 `appVersion`

`flint-lean-chart` is `version: 0.1.0`, `appVersion: "1.37.0"`. If lean
versions independently, that appVersion is doing no work — it tracks a
CSI release lean does not ship with. Decide: either lean's chart tracks
its own binaries' version, or the field is dropped to avoid implying a
coupling that is not real.

## 4. One or more clusters

Two clusters against **different prefixes** are independent — nothing is
shared. Against the **same prefix**, the lease arbitrates and one wins,
which is the design (§2.2), and it is drilled: B12/B18 depose a holder
and confirm the loser is fenced.

What multi-cluster does change:

- **Operator identity collides across clusters.**
  `FLINT_LEAN_OP_IDENTITY` defaults to the pod name
  (`deployment.yaml:22-24`), and `flint-lean-<hash>` is not unique across
  clusters. It is an **audit tag** only — the lease holder id is
  `lean-{uuid}` per incarnation and is genuinely unique, so arbitration
  is not at risk. But the audit trail cannot answer "which cluster
  claimed this?", and v1.36.0 shipped six defects from exactly this shape
  (non-unique `co_ownerid`). Give it a cluster discriminator.
- **Credentials are per-cluster.** Each cluster needs bucket credentials
  scoped to its prefixes. Per-workspace bearers / SigV4 scoping is
  **open** (boundary-verbs plan §3), so today a cluster's credential is
  as broad as the bucket policy makes it.
- **The data-plane fence is still unbuilt.** P5-at-the-proxy is not
  implemented (verified 2026-08-26, §3 residual 4). Cross-cluster this
  matters more, not less: a partitioned cluster's sidecar keeps writing
  uncited versions until its lease is taken, and nothing refuses them at
  the wire.

## 5. Order of work

1. **F1** — add `flint-lean-operator` + `flint-lean-gateway` to
   `Dockerfile.operator.prebuilt`. Smallest unblock; covers the webhook.
2. **F2/F3** — a `Dockerfile.sync.prebuilt` with a shell and
   `ca-certificates`; stage `lean/sidecar` binaries into the same
   `docker/prebuilt/{amd64,arm64}/` tree.
3. **Release gate** — teach `scripts/release.sh` about
   `flint-lean-chart` the way it already knows `flint-lite-operator-chart`
   (which refuses to push when a referenced image is unpublished, and
   when the bundled CRD copy is stale). **This is the step that makes F1
   and F2 unrepeatable**, and it should land with them, not after.
4. **A real-cluster install drill** — `lean/cluster/` exists with 3 legs
   and has never been run. It needs provisioning; ASK before creating
   anything. Its first leg should be the one no kind rig can do: install
   the published chart, from published images, against a **TLS** S3
   endpoint — which is what F3 hides behind.
5. **Then** cut the release, lean-scoped: `flint-lean` 0.1.0 → 0.2.0,
   rebuild `flint-lite-operator` + `flint-sync`, and leave the CSI chart
   and the SPDK image at 1.37.0 (the CSI chart has **0 files changed**
   since v1.37.0 and its image content is unaffected by lean).

## 6. Open questions

1. **§3.1** — one image or two? Recommendation above is "one, for now".
2. **§3.3** — what does `appVersion` mean for a chart that versions
   independently?
3. **§4** — is same-prefix-from-two-clusters a supported topology, or
   should the operator refuse it? Today it is *safe* (the lease wins) but
   unadvertised, and the failure mode a user would hit is a workspace
   that silently stops publishing from one cluster.
