# Flint v1.0.0 — AWS multi-node validation prompt

This file is the self-contained prompt for running pre-publish
validation of a Flint release candidate on a multi-node AWS Kubernetes
cluster. The validation machine doesn't need any context from the
release-prep session — everything load-bearing is in the prompt below
or pointed at via repo paths.

**Workflow.** On the validation machine: clone the repo, read this
file, copy the section between the `---` markers below into a fresh
Claude Code session as your first message, run. Report back to the
release-prep session with the TSV + markdown table + log excerpts.

---

## Prompt to paste into Claude Code on the validation machine

I'm running pre-publish validation for Flint CSI **v1.0.0**, a Kubernetes CSI driver providing high-performance local block storage (SPDK) plus parallel-server NFS (pNFS). The release candidate is committed to `main` of `https://github.com/ddalton/flint`. The release tag `v1.0.0` has **not** been created yet — the publish is gated on this validation passing. **Do not run `git tag` or `gh release create`. Do not push images to the `:1.0.0` tag (only `:1.0.0-rc<N>` is OK). Do not push to `origin/main`. Your job is the bench run and a written report.**

### Infrastructure I have

A Kubernetes cluster on AWS with **5 nodes** (1 control + 4 workers, all `i3en.xlarge`):
- 4 vCPU, 32 GB RAM each
- 1× 2.5 TB local NVMe per node (the device that SPDK / Flint will format)
- 25 Gbps inter-node network
- AMI with kernel 5.16+ and `ublk_drv` available

`KUBECONFIG` is set in my shell. `kubectl get nodes` lists 5 nodes, ready.

### What you need to do

1. **Confirm the cluster meets the bench prerequisites** documented in `tests/k8s/pnfs-bench/README.md`. Run `kubectl get nodes`, verify there are ≥4 worker nodes, and confirm `ublk_drv` is loaded on each worker (`kubectl debug node/<name> -it --image=busybox -- chroot /host lsmod | grep ublk`, or equivalent). If `ublk_drv` is missing, log it and stop — don't try to install kernel modules from this session; the user has to fix that on their AMI bootstrap.

2. **Build the v1.0.0 release-candidate container image** from the current `main` checkout. Tag it `dilipdalton/flint-csi-driver:1.0.0-rc1` (a release candidate — not the final `:1.0.0` tag, since this validation has to pass first). Push to Docker Hub. (If the user's `docker login` for `dilipdalton` is set up, this works directly. If not, ask the user to `docker login` first.) For this validation, build `linux/amd64` only.

3. **Run the cross-host pNFS bench**:
   ```bash
   KUBECONFIG=$KUBECONFIG \
     PNFS_IMAGE=dilipdalton/flint-csi-driver:1.0.0-rc1 \
     MDS_NODE=<worker-1-name> \
     DS_NODES="<worker-2-name> <worker-3-name>" \
     CLIENT_NODE=<worker-4-name> \
     make test-pnfs-cross-host
   ```
   The harness creates a Namespace, MDS+DS Deployments with `nodeName` pins, and a client Deployment. It runs `bs={4K, 1M} × {read, write}` fio sweeps and dumps a TSV + a markdown table. Capture both.

4. **Report back with**:
   - The full TSV that `make test-pnfs-cross-host` produces.
   - The markdown table summary.
   - `kubectl logs` from each MDS pod and each DS pod (last 100 lines).
   - Whether the run hit the **pass criterion** documented in `tests/k8s/pnfs-bench/README.md` (the README is authoritative — read it; the criterion was deliberately written before the bench had real numbers, so it's the published bar to hit).
   - Any `OOMKilled`, `CrashLoopBackOff`, or `Error` pod statuses during the run.
   - Total wall-clock time of the bench run.

5. **If the bench fails to complete or pass criterion is not hit**: do **not** delete the test namespace; leave the pods in their failed state so we can examine them later. Capture full logs, kubectl describe outputs, and report the failure mode.

### What success looks like

- `make test-pnfs-cross-host` exits 0.
- The TSV contains numbers across all four `bs × {read, write}` rows (no empty cells, no errors).
- Per-DS allocation is approximately balanced (each DS holds roughly half the written data; tolerance per the README).
- Pass criterion in `tests/k8s/pnfs-bench/README.md` is met.

### What to leave alone

- Don't run `git tag`, `git push --tags`, or `gh release create`. Tagging is the release-prep session's job once you report back green.
- Don't bump versions in `Cargo.toml`, `Chart.yaml`, or `CHANGELOG.md`.
- Don't push to a `:1.0.0` image tag (the immutable release tag); only `:1.0.0-rc<N>` is OK at this stage. Once validation passes, the main session will re-tag/re-push at `:1.0.0`.
- Don't modify `tests/k8s/pnfs-bench/*` to make the harness pass. If the harness has a real bug, file it as a separate finding in your report.
- Don't push to `origin/main`. If you find a bug that needs fixing in source, surface it as a finding for the user to decide on, don't fix-and-merge silently.

### Reference docs in the repo

- `tests/k8s/pnfs-bench/README.md` — authoritative bench spec, topology, pass criterion.
- `tests/lima/STATUS.md` — full project state. The "Picking up next session" section at the top has the v1.0 publish gate.
- `CHANGELOG.md` — what v1.0.0 ships and known limitations.
- `docs/plans/pnfs-production-readiness.md` — Phase A/B done, Phase C deferred.
- `docs/decisions/0001-3` — ADRs (one driver / perf baseline / write-perf deep dive).

If anything in those docs is ambiguous about the bench setup, surface it in your report — better to ask than to guess.

---

## Notes on this prompt's design

(Not part of the prompt above. For anyone editing this file later.)

- The prompt assumes the validation machine has a fresh Claude Code session with no prior context. Everything load-bearing is in the prompt itself or pointed at via repo paths.
- The "don't tag, don't release" guard is explicit because validation sessions can drift into "completing" the release if they're not told not to.
- The `:1.0.0-rc<N>` image tag avoids polluting the immutable `:1.0.0` tag that the final release should own. If validation passes, the release-prep session re-tags and re-pushes at `:1.0.0`.
- The pass criterion deliberately points at `tests/k8s/pnfs-bench/README.md` rather than restating it here, because that file is the single source of truth and could change.
- The "don't fix-and-merge silently" line is important: if the validation session finds a real bug (e.g., a controller-startup race that only fires under multi-node networking), the failure should come back to the release-prep session as the next problem to fix on `main`, not be patched-and-pushed in isolation.

## Outcome paths

When the validation machine session reports back:
- **Green:** the release-prep session tags `v1.0.0`, pushes, creates the GitHub Release, and proceeds to the final image build/push at `:1.0.0`.
- **Red:** the failure becomes the next session's load-bearing input — fix on `main`, build a `:1.0.0-rc2` image, repeat the validation.
