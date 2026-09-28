# F71 — `rox-multi-pod` asserts nothing a broken ROX implementation would violate: the step called "verify-readonly" never attempts a write, and "multi-node" is a scheduling *preference*

Status: **FOUND 2026-09-22 by reading the test, NOT FIXED.** This is the
oracle defect that let [F70](f70-rox-export-is-not-enforced-server-side.md)
survive. F70 is a server that never enforces read-only; this is the reason
nothing noticed.

**TEST FIXED 2026-09-27 (uncommitted), NOT YET RUN ON A CLUSTER.** Defects 1
and 2 are fixed in `tests/system/tests-standard/rox-multi-pod/`:
- Step 06 now attempts a write through each readOnly mount and requires it
  refused.
- The anti-affinity is `required`, and step 06 compares the two
  `spec.nodeName`s.
- New steps 07 and 08 mount the ROX PVC from a pod WITHOUT `readOnly` and
  require its write refused. That is F70 reached through the ordinary API,
  with no privileged pod, and it FAILED while F70 was open. **F70 was fixed
  server-side 2026-09-28** (unit-tested; see the F70 doc). This cluster
  test has still not been run against the fixed server.
- The bare `kubectl` calls now name `$NAMESPACE`. kuttl runs each test in
  its own namespace, and the old step 06's bare calls looked in `default`.

Defect 3 (a ROX PVC created without a snapshot) is still uncovered.

**Run attempt 2026-09-27 on the build box's kind tier (`tests/regression/kind-up.sh`,
flint-driver 1.56.0): NOT RUN, and the kind tier is unsafe on that host.**
- Step 1 (the RWO writer) never started. MountDevice failed with "NVMe
  device did not appear after 20 seconds". The NVMe-oF connect succeeded
  on the host kernel (`nvme2`, `nvme3` over tcp), but a kind node's `/dev`
  is a tmpfs populated at container creation, so a device that arrives
  later never appears inside it. kind-up.sh's "spdk-rwo-basic works" does
  not hold on this host.
- The kind-mode driver's disk discovery called
  `bdev_uring_create /dev/nvme0n1` and `/dev/nvme1n1`: the HOST's real
  disks, both mounted ext4 and holding model-checker state. It only looked
  for an LVS, found none, and no filesystem error followed (dmesg clean;
  the runs on them continued). But in kind mode the driver opens host
  disks it has no business with, and a disk-init there would destroy
  them. The cluster was deleted, and the two NVMe-oF controllers were
  removed through sysfs.
- So this test needs a real multi-node cluster (AWS), or a kind mode that
  neither discovers host disks nor depends on a device appearing in the
  node's `/dev`.

`tests/system/tests-standard/rox-multi-pod/` is the only ROX system test in
the repo. Eight kuttl steps: RWO PVC → writer pod → snapshot → ROX PVC from
the snapshot → two reader pods → verify → cleanup. It passes today. It would
also pass if ROX were implemented as a no-op alias for RWX.

## Defect 1 — the read-only check performs no write

`06-verify-readonly.yaml` is named for the property it does not test. In
full, it is two `kubectl logs | grep` calls:

```yaml
if kubectl logs reader-pod-1 | grep -q "ROX_TEST_DATA"; then ... else exit 1; fi
if kubectl logs reader-pod-2 | grep -q "ROX_TEST_DATA"; then ... else exit 1; fi
```

That is a *readability* assertion — it proves the snapshot restored and both
pods started. It never execs into a pod, never attempts `touch /data/x`,
never asserts `EROFS` or `Read-only file system`. A grep of the entire
directory for `touch`, `EROFS` or `exec` returns nothing.

So the suite's own summary line — `README.md:54`, "✅ **Read-only mounts** -
Proper `-o ro` mount options" — is asserted by no file in the directory. The
`ro` option is real (`mount_opts.rs:179-182`, forced), but this test is not
what establishes it, and per F70 the option is the *only* thing standing
between a ROX volume and a write.

## Defect 2 — "multi-node" is `preferred`, and no assertion reads a node name

`README.md:46` claims "Multi-node readers | Pods on different nodes, both
Running" and `:56` claims "✅ **Multi-node attachment** - Same volume on
multiple nodes". The scheduling constraint backing both is soft, on both
pods (`05-reader-pods.yaml:12` and `:49`):

```yaml
podAntiAffinity:
  preferredDuringSchedulingIgnoredDuringExecution:   # not required
    - weight: 100
      podAffinityTerm:
        topologyKey: kubernetes.io/hostname
```

And `05-assert.yaml` checks three things — `rox-pvc` `Bound`, `reader-pod-1`
`Running`, `reader-pod-2` `Running`. It never reads `spec.nodeName` and never
compares the two pods'.

On a single-node rig — which is what lima/kind give you — both readers land
on the same node and every assertion still passes. The leg that is supposed
to exercise multi-node attachment is, on the rig it usually runs on, a
single-node test that says so nowhere.

This is the pattern from the lite drill ("24/41 legs would PASS IF BROKEN")
and the standing rule that **a control in the wrong configuration rules out
nothing.**

## Defect 3 (lesser) — the test does not exercise the path its name implies

Worth recording because it makes the coverage narrower than the file names
suggest. The ROX PVC is created **from a snapshot**, so `CreateVolume`
returns early from `create_volume_from_snapshot` (`main.rs:1761-1768`),
*before* the `is_rwx`/`is_rox`/`uses_nfs` block at `main.rs:1857-1875` ever
runs. The ROX PV therefore never gets `nfs.chert.us/enabled=true` in its
`volumeAttributes`. The NFS path is still taken, but only because
`ControllerPublishVolume` recomputes `is_rox` from the capability
(`main.rs:2544-2550`), and only because the snapshot path hand-stamps the one
other attribute the NFS branch needs:

```rust
// main.rs:968-973
// Add NFS replica-nodes attribute (needed for ROX volumes from snapshots)
volume_context.insert("nfs.chert.us/replica-nodes".to_string(), node_name.clone());
```

Without that line `rwx_nfs::parse_replica_nodes` would fail the publish
(`main.rs:2595-2598`). A leftover debug print at `main.rs:983` prints
`"🔴 MISSING - THIS IS THE BUG!"` when the key is absent, which is a fair
measure of how sharp the edge is. **A ROX volume created directly (not from a
snapshot) takes a materially different controller path and is covered by
nothing.**

## What the test has to assert to be worth running

1. **A write must be attempted and must fail.** `kubectl exec reader-pod-1 --
   sh -c 'touch /data/x'` expecting non-zero and `Read-only file system`.
   This is the assertion that fails against today's server once the client's
   `ro` is bypassed, and the one that makes an F70 fix provable.
2. **A write must be attempted from a mount the client did not mark `ro`** —
   otherwise it tests the mount option, not the export. This is the leg that
   distinguishes F70's "asked nicely" from an enforced export.
3. **`requiredDuringSchedulingIgnoredDuringExecution`**, and an assertion
   that reads both `spec.nodeName`s and asserts they differ — or, on a
   single-node rig, the leg should SKIP with a stated reason rather than pass.
4. **A ROX PVC created without a snapshot**, to cover the
   `main.rs:1857-1875` path the current test skips.
5. The README's claim table must be cut back to what is asserted. Three of
   its six rows currently overstate.

Fix this before F70. A fix to the server landed against the present test
would be unfalsifiable — per the standing rule, a positive control must run
through the load-bearing path, and this one runs through `kubectl logs`.
