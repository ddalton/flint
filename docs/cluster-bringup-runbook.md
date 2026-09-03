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
`pnfs.chert.us/*` keys are **rejected** at provision — a typo used to be
indistinguishable from success.

| parameter | meaning |
|---|---|
| `pnfs.chert.us/stripeSize` | stripe unit, power of two, 4 KiB..1 GiB. At the 8 MiB default a file smaller than 8 MiB lives on ONE data server and gets no parallelism. |
| `pnfs.chert.us/stripeWidth` | how many DSes a file is striped across. Omit = all of them, which maximises bandwidth **and** makes the failure domain the whole fleet. |
| `pnfs.chert.us/dirGid` | group owner of the volume root; also sets setgid so files inherit the group (this is what makes a pod `fsGroup` usable). |
| `pnfs.chert.us/dirMode` | octal mode for the volume root. Default 0777. A mode denying "other" without a `dirGid` is refused — no pod could write. |

Both geometry parameters are **fixed at create**: a file's placement is
pinned at its first layout grant and never re-striped.

**`mountOptions` on the class replace the driver's defaults — with one
known exception.** Fixed 2026-08-02 after runax measured a class carrying
`mountOptions: ["nconnect=16"]` propagating correctly to
`PV.spec.mountOptions` while the kernel still mounted the driver's own
`nconnect=4`. The driver used to emit *both* values and rely on the kernel
taking the last; it now emits the operator's value **instead of** its own,
so the string never contains the same option twice and precedence never
enters into it. The RWX/non-pNFS path was worse — it built its option
string as a compile-time literal and never read `mountOptions` at all —
and is fixed the same way.

The exception is **`nconnect`**: it is a property of the client's shared
`nfs_client`, and every pNFS PVC on a node mounts the same MDS ip:port, so
the second and later mounts on a node can inherit the first mount's
connection count regardless of the option string. To change it fleet-wide,
change the driver default rather than the class.

**Verify every time**, in the consuming pod:

```bash
kubectl exec <pod> -- grep nfs4 /proc/mounts
```

and cross-check what the driver actually asked for:

```bash
kubectl -n <ns> logs ds/flint-csi-node -c flint-csi-driver \
  | grep 'pNFS\] mount -t nfs4'
```

If those two disagree the loss is in the kernel, not the driver. The
driver also now names the empty cases (`request carried NO
volume_capability`, `Block access type`, `none supplied`) — before, a
silent `unwrap_or_default()` made "the operator set nothing" and "kubelet
never passed them" indistinguishable from the node.

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

### What the numbers mean, and which benchmark answers which question

Measured on runay 2026-08-02: 5 data servers on i3en.xlarge, one DS-free
12-vCPU client, preflight green.

| workload | read | vs 1 DS (352 MiB/s) |
|---|---|---|
| `fio --direct=1 --iodepth=32` | 1876 MiB/s | **5.33x** |
| checkpoint, 8 shard readers, `read()` | *see below* | |
| checkpoint, 8 shard readers, `mmap` | 1631 | 4.6x |
| checkpoint, 4 shard readers, `mmap` | 1283 | 3.6x |
| checkpoint, **1 reader**, `mmap` | 681-831 | ~2.2x |

**DS scaling is linear. Whether you SEE it depends entirely on how much
the client keeps in flight.** fio with 128 outstanding 1 MiB requests
engages every data server by construction; a single-threaded loader takes
about 40% of the same fleet. Any "pNFS does not scale" claim sourced from
one sequential reader is measuring the reader.

Two consequences worth acting on:

- **`mmap` costs about twice the CPU per byte of a plain `read()`**, and
  ~26% of the throughput: same file, same fleet, one reader — mmap 831
  MiB/s at 0.74 cores busy, `read()` 1123 MiB/s at 0.48. safetensors
  mmaps by default, so a checkpoint load pays this.
- **Readahead is a weak lever.** `read_ahead_kb` defaults to 15360 and is
  writable per-mount. Sweeping one stream: 4 MiB 459, 15 MiB 679, 64 MiB
  766, 128 MiB 828. Going *below* the 8 MiB stripe unit costs 32%; going
  8.5x above the default buys 22%.

The obvious-looking explanation — 15 MiB of readahead spans 2 of the 8 MiB
stripe units, so only 2 of 5 servers can be busy — predicts 2 x 375 = 750
against a measured 681 and is **wrong**. It was refuted by the sweep above:
if concurrency-over-stripe-units were binding, 128 MiB of readahead would
have roughly quadrupled throughput. Arithmetic that matches a measurement
is not a mechanism.

Also note **per-DS egress shares are not evidence of concurrency**. A
single reader shows a perfect 20.0% from each of five servers — that is
round-robin over the whole run, not five servers working at once. Only
throughput separates them.

Use `scripts/pnfs-fanout-diag.sh` for the fleet-parallel question and
`scripts/pnfs-model-bench.sh` for the checkpoint-load question; they do
not measure the same thing and neither substitutes for the other.

### The rig drifts. Put every point in ONE window.

The hardest lesson of 2026-08-02: **the same volume, same client and same
fleet measured 1849 MiB/s early in a session and 773 later**, with nothing
deliberately changed. A width sweep assembled from runs taken hours apart
produced a confident "stripeWidth 3 and 4 collapse ~3x" that a same-window
bidirectional sweep then failed to reproduce at all — the ascending and
descending passes disagreed by up to 1.6x on the same width.

**Check the NIC is not being metered before blaming flint.** `i3en.xlarge`
is "**up to** 25 Gbps" — burst over a much lower sustained baseline (only
`i3en.6xlarge+` sustains it). Hours of load exhausts the credit and every
number quietly halves. ENA exposes the counter:

```bash
./scripts/nodesh.sh <node> 'ethtool -S ens5 | grep allowance'
#   bw_out_allowance_exceeded: 0        <- still bursting
#   bw_out_allowance_exceeded: 918273   <- AWS is throttling you, not flint
```

Sample it beside every throughput number. A rising counter means the
measurement is metering, not storage.

So, for any comparison you intend to believe:

- **Every point in one window**, back to back, and **run the sweep in both
  directions**. Disagreeing passes mean the rig is not measurable right
  now — that is a result, not noise to average away.
- **Exactly ONE pNFS mount per client node.** Every pNFS PVC on a node
  mounts the same MDS ip:port and shares one `nfs_client`, so idle consumer
  pods left over from earlier runs are not free. Seven had accumulated by
  the time the drift was noticed; that is the leading suspect and it is
  cheap to avoid.
- **Re-measure the baseline at the END** of a sweep. If it no longer
  matches its own first reading, every ratio in between is void.

`scripts/nodesh-daemon.sh up` makes this affordable: it drops one sleeper
pod per node so `nodesh` execs (0.55s) instead of spawning a pod (20s),
which is the difference between a five-width bidirectional sweep taking ten
minutes and taking two hours. Run `down` at teardown — the sleepers are
privileged.

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
