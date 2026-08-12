# Lessons learnt

The durable, cross-cutting lessons from flint's campaign record — the ones that
change *how we decide to test, measure, and operate*, distilled so nobody has to
re-pay for them. Point-lessons stay embedded where they bite (the
[cluster bringup runbook](cluster-bringup-runbook.md), the design docs'
RIG-PROVEN boxes, the rig scripts' header comments); this doc is the layer above:
which tool answers which question, and the rules each tool taught us.

Evidence tags like `(runbb)` or `(F69)` refer to the cluster campaign or finding
that paid for the lesson.

---

## 1. The validation ladder

Four rungs. Each has a domain where it is the *cheapest tool that can tell the
truth* — and a blind spot where it cannot tell the truth at any price.

| Rung | What it settles | What it cannot see |
|---|---|---|
| **TLA+ model** | Protocol/state-machine safety: races, recovery interleavings, invariant design | Anything about the real kernel, wire, or performance |
| **Unit tests** (macOS *and* Linux cross-build) | Transaction logic, encoders, replayed TLC traces | End-to-end behavior; everything `#[cfg(linux)]` if you only run macOS |
| **lima rig** | Real-kernel protocol semantics, fencing, recovery, session lifecycle — one box, real spdk-tgt, real MDS, production CSI paths | Performance numbers, multi-node topology, fleet kernels |
| **Cluster** | Performance truth, multi-node failure topology, k8s integration, economics | Nothing — but it is 100–1000× the cost per finding, so arrive with everything else already settled |

**The ladder is ordered by cost-per-finding.** The block-layout tier reached
shipped-and-proven (allocator, wire, export, fence/unfence, PTPL, lease sweep,
sessions, zombie drills) with **zero cluster spend**; the files-layout era found
equivalent-class bugs on rented hardware at real cost. The difference wasn't
discipline in hindsight — the block architecture *fits in one VM* (no DS fleet)
and its risk lives in the least-exercised kernel driver, exactly the rig's
domain. Ask "which rung does my question live on?" before reaching for a
credit card.

**Corollary:** when a cluster campaign discovers a bug, ask whether a rig could
have caught it — F69 (the 5s cold-open stall) was found on a cluster and then
reproduced, fixed, and verified on lima. About half the files-era findings were
riggable in hindsight. Make the rig catch the next one first.

---

## 2. Trove (AWS cluster tooling)

Setup and operation of throwaway benchmark/drill clusters (`~/github/trove`).

- **Read `docs/cluster-bringup-runbook.md` first.** Every bringup mistake in it
  was made at least once.
- **`AWS_PROFILE=rolesanywhere`**; the policy lacks `ec2:RebootInstances` — use
  SSM to reboot nodes.
- **Pure-spot clusters, control plane included** — never on-demand; use the
  hand-rolled create path. Set `aws.controlPlaneNodeType` explicitly (runag).
- **Budget the bytes, not the hours.** Cross-AZ data transfer at $0.02/GB
  dwarfed instance cost on real bills (one 60s throughput test at 5.6 GB/s ≈
  $6.72). Trove pins no subnet, so nodes scatter across AZs and *all* traffic
  is billable — pin a single AZ/subnet before any throughput work.
- **Verify the tooling isn't mocking you.** The trove launchd daemon once
  sourced only the Azure env and silently served `sg-mock`/`i-mock` — zero real
  EC2 calls, everything green (runbj). If results look too clean, check the
  instrument's own credentials.
- AWS tags are `trove/<cluster>/<node>`; helm images key is `images.` (plural);
  helm-from-OCI: pull the tgz, then upgrade from it (v1.16.0).
- **Fresh clusters lack the `flint` StorageClass** kuttl expects — apply a
  flint-spdk clone before test runs.
- **Assert `distinct backing nodes == DS count` before any drill** — spot
  rebuilds can co-locate DSes and every number you take afterwards is fiction
  (runaw).
- Disk-init is **not** automatic on nodes added later; Node-Ready ≠ agent-ready
  (runao). A wedged roll unblocks by deleting the Node object (runak).
- Spot reclaim is a live actor mid-campaign: checkpoint results early, don't
  leave the decisive run for last (runbm lost its arbiter to a reclaim).

---

## 3. The lima rig

One VM (`flint-nfs-client`): real Linux kernel client (HWE, ≥6.11 for block
layout), real spdk-tgt, real cross-built MDS, staging through the production
CSI CLI. Modes are standing regression harnesses (`make test-pnfs-*-rig`).

**Mechanism rules:**

- **The macOS suite is NOT the suite.** Everything `#[cfg(target_os="linux")]`
  neither runs nor typechecks there. Cross-build aarch64-musl (zig shim recipe
  in the rig header) and run the lib suite inside the VM, every time.
- **Drive production code, not bash replays of it.** The rig staged via
  `pnfs-csi-cli stage` (the NodeStage code path) and immediately caught a fence
  regression the unit tests missed — the bash replay it replaced never could
  have.
- **A fresh lima VM is a test fixture.** Stock 24.04 = kernel 6.8 = the exact
  must-refuse case for the kernel admission check; create, prove, delete.
  A second VM + SIGSTOP on its hypervisor = a frozen-VM zombie client.
- lima VMs sit on isolated user-nets (each is 192.168.5.15) — cross-VM traffic
  goes through the host (`tcp-proxy.py` + lima's loopback auto-forward).

**Hygiene rules (each one paid for):**

- **A drill must be able to fail.** Two harness bugs once let drills pass *by
  not looking* — a dead API server made never-observed oracles pass silently
  (runaq/runar). Assert the observation happened, not just that no error was
  seen.
- **A drill's own config can hide the bug.** The callback channel was broken
  for every production mount — flint mounts `vers=4.2`, and the CB_COMPOUND
  header hardcoded `minorversion: 1`, which Linux resolves clients by, so every
  callback answered NFS4ERR_BADSESSION before the op was read. The one drill
  that exercises recalls mounts `minorversion=1`, so it passed for years. Match
  the drill's configuration to production, or state explicitly which production
  shape it is NOT covering.
- **The client's own debug log is a first-class oracle.** Two wire-format
  refusals (NFS4ERR_INVAL, NFS4ERR_BADSESSION) were opaque from the server side
  and instantly legible from the kernel's: `echo 0x1100 >
  /proc/sys/sunrpc/nfs_debug` then `dmesg` gave the decoder's own verdict
  (`decode_devicenotify_args: status 22 ndevs 0` → and after the fix, `type 2
  layout 0x5` plus `bl_free_deviceid_node`, the cache drop we were paying for).
  `rpcdebug` itself buffer-overflows on modern userland; write the sysctl.
- **Fix a bug on one twin path and you have fixed half a bug.** The
  CB_LAYOUTRECALL seqid bump (RFC 8881 §12.5.3 — the client refuses a recall
  whose seqid it already holds) was fixed once on the truncate recall path and
  left undone on the DS-death path for months. What hid it: a refused recall is
  answered by forced server-side revocation, so from outside the drill it looks
  exactly like a recall that worked — and the drill's own assertion demanded the
  stateid come back *unchanged*, pinning the bug in place. When two call sites
  produce the same wire message, give them one shared function, and be
  suspicious of any test asserting "unchanged".
- **Read the client's constants, not just the RFC.** RFC 8881 declares
  `NOTIFY_DEVICEID4_CHANGE = 1`; Linux defines it as `1 << 1` because every
  transmission of it is a bitmap, and its decoder compares the received word
  against *its* constant. The RFC's ordinal on the wire is refused. When a
  wire value has an obvious reading and a deployed implementation, pin the
  deployed one in a byte-level test with the source citation.
- **`grep -q` + `pipefail` = SIGPIPE false negatives** in drill asserts — use
  `grep -c`. Never pipe a drill through `tail` (runag/runai).
- **Never `nvme disconnect` under a fenced writer's in-flight O_DIRECT pwrite**
  — it wedges the writer *and* `nvme-delete-wq` in D-state; only a VM reboot
  recovers. Wait for the writer's errno first; skip the disconnect if it stays
  blocked.
- **Don't run the TLA gate concurrently with rig runs** — host CPU starvation
  once wedged the MDS accept loop mid-drill and cost 48 minutes of diagnosis.
- D-state is the rig's occupational disease (2GiB VMs, fenced writers,
  destructive REMOVEs): force-reboot the VM without ceremony, and make each
  drill's cleanup assume the last run died mid-wedge.
- kind (`kindMode`) runs a **real spdk-tgt** — use it for CSI/operator flows;
  ublk drills stay on metal.

---

## 4. Model-based validation (TLA+)

Five-plus modules, one gate script (`scripts/check-tla.sh`), every run required.

- **Model the IMPLEMENTATION, not the design.** THE ABSTRACTION WAS THE BUG
  three separate times — a model of what we *meant* verifies nothing about what
  we *built*.
- **Model-before-code pays.** FlintExtents refuted its own §9 design (the
  grant-side belts could not close the grant-vs-reclaim races; safety had to
  move to the free side) *before* the allocator was written — the sqlite
  transactions were then built to the corrected shape.
- **Every green needs a license.** A theorem that holds over an empty or
  unreachable state space is a lie: pair every strict run with (a) **mutation
  runs** — single-flag must-VIOLATE cfgs proving the invariant has teeth, and
  (b) **probe runs** — properties asserting the interesting actions actually
  execute in the checked space.
- **A/B every mutation**: flip exactly one thing; if the gate can't tell the
  mutant from the shipped config, the gate isn't checking what you think.
- Replay TLC counterexample traces as unit tests — the model's findings become
  regression pins in the code's own suite.
- TLC mechanics that bite: unbounded `CHOOSE` for sentinels errors (use a
  model-value constant); terminating models need `CHECK_DEADLOCK FALSE`;
  state-enumerating predicates go stale when the state set grows — prefer
  complement forms ("not free ∧ held" over listing states).
- Count gate runs by *invocations* (`grep -c '^strict_run \|...'`), never by
  eyeball — the header once read wrong because function definitions inflated
  the count.

---

## 5. Measurement discipline

The instrument bugs found across benchmark campaigns outnumber the real
performance findings. Rules:

- **A ratio between legs differing in more than one way is not an
  attribution** (runbl). The "flint loses 60% at 4k" result was a
  numjobs/locality artifact; the real cost was ~13%.
- **One window, both directions, one mount per client, re-baseline at the
  end** — the rig drifts ~2× within a session, so cross-time ratios are void
  (runay).
- **Know the device ceiling before trusting a win.** A result 1.55× above the
  measured device max is an instrument bug wearing a medal (runbm — fio
  `--name` derives filenames; reads hit unallocated lvol clusters = a
  CPU-bound zero path). Arbitrate with `bdev_get_iostat`, the device's own
  counters.
- **`ss` is the connection-topology truth** — runbe served every byte through
  the MDS proxy path with zero LAYOUTGETs and looked normal until the socket
  table was read.
- **The network fabric taxes silently**: Cilium WireGuard collapsed all pod
  traffic into one UDP flow → one client core → −45% (runbb). Every historical
  number carries the tax of the fabric it ran on — record the fabric.
- SPDK reactors busy-poll: "core at 100%" means nothing; use
  `thread_get_stats`, not `ps` (runbj).
- Client caches lie at both ends: a "fast" read may be a throttled cache read
  (runbc), and a fully-cached RAM read can be *slower* than O_DIRECT (runbk).

---

## 6. Fleet and ops hard lessons

- **Never force-delete a consumer pod on a dead flint mount** — the D-state
  pins the volume and only a node reboot clears it (runab). The unstick
  ladder: `ss -K` first, reboot last (runz).
- DS rolls restart spdk-tgt → EIO under mounted PVCs; **drainRoll is ON by
  default since 1.22.0** — block-layout remote initiators raise the stakes
  because the roller is blind to them.
- Kubelet evictions explain "mystery" pod deaths — the audit log is where the
  answer lives (attach/detach campaign).
- Silent-zero reads are the house failure mode: F67 (MDS state loss ⇒ zeros)
  and six instrument bugs were all "reads zero instead of erroring." Fail
  loudly beats fallback quietly, every time it has come up.
- Durability claims need a power-loss test: the "durable" fence record sat on
  `synchronous=NORMAL` sqlite until a power-off drill emptied it — for block
  volumes sqlite IS the data map, so the pNFS MDS runs `synchronous=FULL`
  (re-priced: the wire path absorbs it; only serial allocator txns pay).
