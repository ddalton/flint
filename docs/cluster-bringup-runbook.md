# Cluster bring-up runbook (trove → flint → pNFS)

End-to-end recipe for provisioning an AWS test cluster with trove,
bringing flint up on it, and tearing it down with zero residue.

**Validated end-to-end on cluster `runau`, 2026-08-01** (chart 1.23.0,
k8s 1.34.10, kernel 6.1.176, 1× on-demand CP + 3× spot i4i.xlarge). Every
command below was run in that order; the traps are ones that have
actually cost time, most of them more than once.

This exists because the recipe was previously spread across ~6 memory
files and a 2,600-line campaign doc, and reconstructing it cost 10-30
minutes at the start of every campaign.

---

## 0. Prerequisites — two traps that silently produce wrong results

**Build with `make serve-build`, never a plain `cargo build --release`.**
The featured build is `--features 'trove-providers/aws-live
trove-providers/wg-live'`. A plain release build silently produces a
MOCK-AWS binary (~20 MB vs ~40 MB): provisioning then "succeeds" with
instant node-Running events, no EC2 instances, and the project ends in
Failure. Cost a full provision attempt on runv.

**`FLINT_CHART_VERSION` is captured at backend STARTUP.** Bumping the pin
in `scripts/aws-live.env.fish` does nothing to an already-running
backend — restart it. There are TWO pins and the env one wins:

| where | note |
|---|---|
| `scripts/aws-live.env.fish` | the env override — **beats** the code default |
| `backend/crates/providers/src/components/flint_csi.rs` | the code default |

Update both. A stale 1.3.0 chart was deployed on 2026-07-04 from exactly
this drift. **Verify after provisioning with `helm list -A`** — that is
the only check that cannot lie.

AWS creds: `AWS_PROFILE=rolesanywhere` (us-west-1), also works for the
plain `aws` CLI.

---

## 1. Provision

```bash
cd ~/github/trove
fish scripts/aws-live-drive.fish create <name> <workers>
```

- CP instance type must be **storage-optimized** (`i4i.xlarge`) or trove
  installs flint in nfs-only mode instead of SPDK mode
  (`is_spdk_eligible`). Confirm with `"spdkEligible": true` in status.
- The drive script hardcodes `aws_on_demand` for the CP. **All-spot**
  (~$0.097/node-hr) needs `aws.controlPlaneNodeType: "aws_spot"` in the
  PROJECT body, which means a hand-rolled create. Weigh it against the
  risk: two runs (runab, runaq) were voided by a CP spot reclaim
  mid-campaign. For short runs, on-demand CP is the cheaper choice.
- Poll: `fish scripts/aws-live-drive.fish status <name>`. Nodes get
  renamed `<name>-cp-1` / `<name>-aws-N`. ~5 min to all-Ready.

```bash
curl -sk -X POST https://localhost:8080/api/v1/kubeconfig/download \
  -H "Authorization: Bearer trove-dummy-token" \
  -H "Content-Type: application/json" \
  -d '{"projectId":<id>}' -o /tmp/kubeconfig-<name>.yaml
```

**Then verify what actually deployed** — this is the step that catches a
stale backend:

```bash
helm list -A            # expect flint-csi-driver-chart-<version>
```

---

## 2. Disk init — THE STANDING GATE, do this before any SC or PVC

Workers come up with `blobstore_initialized=false` and `lvs=null`, so
they are **invisible to placement**. This is a known trove gap and it
applies to mid-campaign node additions exactly as at provision.

The node agent listens on **port 9081**, hostNetwork. VPC IPs
(`172.31.x.x`) are NOT routable from the Mac — the WG tunnel only carries
`10.42.x.x` — so use port-forward:

```bash
kubectl port-forward -n flint-system pod/<flint-csi-node-pod> 19081:9081 &
```

**`GET /api/disks` HANGS (curl exit 28). Use the POST RPC-style routes,
and POST needs a body** or it returns "EOF while parsing a value":

```bash
curl -s -X POST http://localhost:19081/api/disks \
  -H 'Content-Type: application/json' -d '{}'
```

Pick the disk with **`is_system_disk == false`** and initialize it:

```bash
curl -s -X POST http://localhost:19081/api/disks/initialize_blobstore \
  -H 'Content-Type: application/json' -d '{"pci_address":"0000:00:1f.0"}'
# -> {"lvs_name":"lvs_<node>_0000-00-1f-0","status":"success"}
```

On i4i.xlarge: the data NVMe is `0000:00:1f.0` (937 GB) and the **8 GB
system root is `0000:00:04.0` — never initialize it** (F3).

**Node Ready ≠ agent listening.** Initing straight after the Node goes
Ready fails with `curl: (7) ... port 9081` because spdk-tgt/agent is
still starting. Poll `POST /api/disks` until it answers, then init.

Verify before proceeding: `blobstore_initialized=true`, an `lvs_name`,
and ~933 GB free.

---

## 3. StorageClasses

Trove creates `flint-spdk` (primary, `WaitForFirstConsumer`) and
`flint-nfs`. It does **not** create:

- `flint` — the kuttl standard suite provisions with `storageClassName:
  flint` in 9 places; without it every PVC stays Pending and the tests
  burn 300 s timeouts. Apply a clone of `flint-spdk`.
- any **pNFS** class — add `layout: pnfs` to an otherwise identical
  parameter set (see §4).

Clone parameters from `flint-spdk`: `numReplicas`, `autoRebuild`,
`thinProvision`, `nfsEmptyDir`.

---

## 4. pNFS

Not enabled by default (`pnfs.enabled` and `pnfs.server.enabled` are both
false). **`helm upgrade` straight from the OCI URL fails** — pull the tgz
first, then upgrade from the local file:

```bash
helm pull oci://registry-1.docker.io/dilipdalton/flint-csi-driver-chart \
  --version <v> -d /tmp/chartup
helm upgrade flint-csi /tmp/chartup/flint-csi-driver-chart-<v>.tgz \
  -n flint-system --reuse-values \
  --set pnfs.enabled=true --set pnfs.server.enabled=true --wait
```

Gives `flint-pnfs-mds` + `flint-pnfs-ds-{0,1}` (MDS count 1, DS count 2 by
default). The chart can now render the StorageClass too — it could not
before, and `--set storageClass.parameters.layout=pnfs` renders *nothing*
because `storageclass.yaml` hardcodes the four SPDK keys:

```bash
--set pnfs.storageClass.create=true --set pnfs.storageClass.name=flint-pnfs
```

### pNFS StorageClass parameters

All are optional; omitting one means the MDS default. Unknown
`pnfs.flint.io/*` keys are **rejected** at provision — a typo used to be
indistinguishable from success.

| parameter | meaning |
|---|---|
| `pnfs.flint.io/stripeSize` | stripe unit, power of two, 4 KiB..1 GiB. At the 8 MiB default a file smaller than 8 MiB lives on ONE data server and gets no parallelism. |
| `pnfs.flint.io/stripeWidth` | how many DSes a file is striped across. Omit = all of them, which maximises bandwidth **and** makes the failure domain the whole fleet. |
| `pnfs.flint.io/dirGid` | group owner of the volume root; also sets setgid so files inherit the group (this is what makes a pod `fsGroup` usable). |
| `pnfs.flint.io/dirMode` | octal mode for the volume root. Default 0777. A mode denying "other" without a `dirGid` is refused — no pod could write. |

Both geometry parameters are **fixed at create**: a file's placement is
pinned at its first layout grant and never re-striped.

**`mountOptions` on the class do NOT reliably reach the kernel mount** —
this was previously documented here as working and it does not. Measured
2026-08-02 on runax: a class carrying `mountOptions: ["nconnect=16"]`
propagated correctly to `PV.spec.mountOptions: ['nconnect=16']`, and the
kernel still mounted with the driver's own **`nconnect=4`**. So the
class-level escape hatch cannot currently override a default the driver
already emits, and an operator has no supported way to retune one. Treat
any option the driver sets in `build_pnfs_mount_opts` as effectively
fixed until this is fixed, and VERIFY with `grep /proc/mounts` rather
than trusting the StorageClass.

The mount is **NFSv4.2** (`minorversion=2`) and
requests **`sec=sys`**; earlier releases pinned 4.1 and sent no `sec=`
at all, which negotiated AUTH_NULL so no uid reached the server and
every created file landed owned by root — measured, and the reason
ownership-sensitive workloads (postgres) would not start on a pNFS PVC.

**Pods must use `nodeSelector`, not `nodeName`** — an explicit `nodeName`
bypasses the scheduler and a `WaitForFirstConsumer` PVC then never binds.

### Before ANY throughput or scaling measurement

```bash
./scripts/pnfs-bench-preflight.sh 5     # expected DS count
```

Non-zero exit means **do not benchmark** — fix the rig first.

This is not ceremony. On 2026-08-01 a DS-scaling sweep ran three times and
every result was void: all five data servers' **backing volumes** had been
provisioned onto one node, so the sweep measured a single device at every
stripe width and reported "pNFS does not scale" (1.11x). The DS *pods* were
spread across five nodes — which is what was checked, and precisely why it
looked right. With the rig corrected the same sweep measured **3.50x**.

The trigger was ordering: `helm` ran before disk-init, so when the
StatefulSet's PVCs bound only one node had a blobstore and everything landed
there. **Disk-init makes a node visible to placement; a node initialised
later never gets used.** Hence the rule already stated in §2 — disk-init must
complete before any StorageClass or PVC exists — and hence this gate, which
enforces it after the fact.

Two more traps the preflight catches, both of which produced confident wrong
answers the same day:

- **Multi-client tests that share one volume** cannot distinguish a
  per-volume cap from a fleet-wide one. Give each client its own volume.
- **A client co-located with a data server** both serves and consumes: part
  of its traffic is node-local and it contends for the same CPUs. On
  4-vCPU nodes a client alone needs ~3.8 cores at 1 GB/s, so a co-located
  pair is starved by construction.

### Verifying pNFS actually works

```bash
kubectl exec <pod> -- mount | grep /data          # nfs4
kubectl exec <pod> -- sh -c 'dd if=/dev/urandom of=/data/t.bin bs=1M count=64; sync; md5sum /data/t.bin'
kubectl exec flint-pnfs-ds-0 -n flint-system -- find / -name '*.stripe*'
kubectl exec deploy/flint-pnfs-mds -n flint-system -- stat -c '%n size=%s blocks=%b' <stub>
```

Expect stripe files on **both** DSes and an MDS stub with **`blocks=0`** —
that is correct: the MDS is out of the data path. The client must still
report real allocation (`du` = the file size); a client-visible `0` is the
pre-1.23.0 bug that made `tar --sparse` archive nothing.

---

## 5. Conformance

- `make test-nfs-protocol` — pynfs 4.1 suite (171/171 PASS expected).
- `make test-nfs-42` — ALLOC1-3 + COPY5, 4/4. **Runs the server inside
  the VM on purpose**: SEEK/ALLOCATE/DEALLOCATE bodies are
  `#[cfg(target_os = "linux")]`, so a darwin-hosted server fails ALLOC1-3
  no matter what the code does.
- pNFS subset: `tests/lima/pnfs/pynfs.sh`, or run the `pnfs` flag set
  directly. **Baseline is 1 PASS / 3 FAIL** — the three are not flint
  defects: LAYOUTCOMMIT1 and GETLAYOUT1 demand *block* layout (flint is
  *files* layout, RFC 8881 §13) and GETDLIST1 hits a Python 3 bytes/str
  bug inside pynfs itself. Treat any deviation from 1P/3F as a
  regression.

### The MDS scores worse than the standalone server on the general suite

Measured for the first time 2026-08-01. Only the 8-test `pnfs` subset had
ever been run against the MDS; nobody had pointed the general 4.1 suite
at it.

| server role | result |
|---|---|
| standalone (RWX path) | **171 / 171 PASS** |
| pNFS MDS | **160 PASS / 11 FAIL** |

The 11 are stable across a dirty and a clean-restart run, so they are
real behaviour, not accumulated session state. Two clusters:

- **SEQ9a-f (6)** — every one returns `NFS4ERR_RETRY_UNCACHED_REP` where
  a result was expected. Session reply-cache behaviour.
- **PUTFH1a, RNM1a/2a/3a, LKPP1a (5)** — `OP_LOOKUP` →
  `NFS4ERR_RESOURCE` and `OP_CREATE` → `NFS4ERR_IO`.

**Not yet diagnosed, and not obviously benign.** In the shipped
configuration the CSI driver mounts the MDS for pNFS volumes and real
clients do issue LOOKUP/CREATE/RENAME against it. Ordinary paths work —
a live cluster run wrote, read, checksummed and striped a 64 MiB file
without trouble — so these are likely edge cases, but "likely" is not
"measured". Treat 160/171 as the current MDS baseline and investigate
before relying on the MDS for general NFS traffic.

**Set `FLINT_NFS_GRACE_SECS=900` when running the suite against the
MDS.** Without it RECC3 fails spuriously: the suite outlasts the
RFC-default 90 s grace window and the test assumes the server is still in
grace. `make test-nfs-protocol` sets this for the standalone run; an
ad-hoc MDS run must set it too. That was 1 of the 12 failures in the
first run and it is a harness artifact, not a defect.

---

## 6. Teardown

```bash
curl -sk -X POST https://localhost:8080/api/v1/projects/delete \
  -H "Authorization: Bearer trove-dummy-token" \
  -H "Content-Type: application/json" -d '{"projectId":<id>}'
```

`DELETE /projects/<id>` returns **405**. Termination takes ~5 min to
`runningNodeCount: 0`.

Verify residue with the **canonical tag `trove:cluster=<name>`** — a
`tag:Project` filter silently matches nothing:

```bash
aws ec2 describe-instances --region us-west-1 \
  --filters "Name=tag:trove:cluster,Values=<name>" \
  --query 'Reservations[].Instances[?State.Name!=`terminated`].[InstanceId,State.Name]'
aws ec2 describe-volumes --region us-west-1 \
  --filters "Name=tag:trove:cluster,Values=<name>" --query 'Volumes[].VolumeId'
```

Then `GET /aws/orphans` (+ `POST /aws/orphans/terminate {instanceId}`)
after any messy delete. Filtering security groups on `trove*` matches OLD
clusters — filter on the cluster name, and sweep stale `trove-*` SGs
separately.

---

## Trap index

| Trap | Symptom |
|---|---|
| plain `cargo build` | mock-AWS binary, no instances, project Failure |
| backend not restarted after a pin bump | deploys the previous chart |
| `GET /api/disks` | hangs (curl 28) — use POST |
| `POST /api/disks` with no body | "EOF while parsing a value" |
| init before agent is up | `curl: (7)` on port 9081 |
| initializing `0000:00:04.0` | that is the 8 GB system root (F3) |
| skipping disk init | workers invisible to placement; PVCs Pending |
| `nodeName` on a consumer pod | `WaitForFirstConsumer` PVC never binds |
| `helm upgrade` from an OCI URL | "failed to download" — pull the tgz first |
| two nodes added in the same second | identical timestamp names; deleting the dup record **evicts the live node**. Add workers ONE AT A TIME |
| `DELETE /projects/<id>` | 405 — use `POST /projects/delete` |
| `tag:Project` residue filter | matches nothing; use `trove:cluster` |
| non-storage-optimized CP | flint installs nfs-only, not SPDK |
