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
  `grep -c`. Never pipe a drill through `tail` (runag/runai). *Recurred
  2026-08-11* in a freshly written assert: the pattern matched, the line was
  printed in the failure message, and the drill failed anyway. A rule you have
  written down is not a rule you have applied — grep the diff for `grep -q`
  before running a rig, not after.
- **"Same predicate, evaluated later" is a new behaviour, and the model has to see
  it.** The quarantine sweep looked like a free re-application of a belt the model
  already gates — the argument for skipping the tranche was written down before it was
  tested. TLC refuted five successive designs before any code existed: the release
  needed provenance, the delivery retry had to be modelled or the sweep was unreachable
  (the *probe* caught that — a green meaning "it never fired"), other free paths had to
  un-park, only real extents could be parked, and the grant path re-handed out a parked
  range. When a change re-applies an existing guard at a new TIME, the module has never
  occupied that state, and "no model change needed" is the claim most worth
  disbelieving.
- **A drill that cannot reach the state it names proves the opposite thing, quietly.**
  The quarantine-sweep drill staged a fence with the tgt down and expected the reclaim to
  park the range; it freed it, correctly — a REACHABLE client returns its layout, and a
  return is quiescence. Quarantine is only reachable for a holder that *cannot* be
  reached, so a single-host drill can only ever exercise the clean-free path while
  wearing the other path's name. Withdrawn rather than weakened. Before writing a drill,
  ask what has to be UNREACHABLE for the state under test to exist — if the answer is
  "the host running the drill", the harness needs a second host.
- **A counterexample indicts the abstraction before it indicts the design — go read the
  code.** The fifth refutation above was filed as an open hole in the two-step grant
  window and cost the tranche a session. It was not a hole: the model rendered a
  quarantined range as still-allocated, which made it an ORPHAN (allocated, no live
  holder) and therefore re-grantable by the module's own rule, while the code moves the
  range to a THIRD table that neither the allocator nor the re-grant path reads. The
  fix was one new state value, not a new belt — and the A/B that pins it now finds the
  corruption in nine states. Before writing "the model needs a guard the code already
  has", check that the code has it *for the reason you think*; the answer took one grep
  of `reclaim_complete` and settled a constraint that had been carried as unresolved.
- **`fail` inside `$( )` is not a failure, it is a value.** Its message becomes the
  variable's contents and its `exit` leaves only the subshell, so the caller
  proceeds with the error as its data and reports something unrelated three lines
  later. Written down after it cost two fence-drill runs — and then it cost two
  more in a brand-new kind script the same day, because the rule was recorded but
  the *shape* was not. The shape: helpers set a global and return a status; they
  never run inside command substitution and never call `fail`.
- **Never flip `set -e` on inside a script that does not use it.** The rig runs
  `set -uo pipefail`; a `set +e … set -e` pair copied from elsewhere leaves errexit
  ON for everything after, so the next unguarded `grep` that finds nothing kills
  the run **with no message at all**. Two of the fence assertions died that way
  before they ever ran. Carry a remote command's exit code back inline
  (`cmd; echo RC=$?`) instead of touching shell options — and never call the
  `fail` helper inside `$( )`, where its message lands in the variable and the
  exit only leaves the subshell.
- **A probe wired with a flag the tool does not have prints "nothing found"
  forever.** Five `nvme resv-report` calls in the fence and preempt drills passed
  `-c`, which nvme-cli 2.8 has no such option for; every one failed, `2>/dev/null
  || true` turned the error into an answer, and each drill dutifully logged
  `resv-report pre-fence: <none>` across every campaign. Nobody noticed, because
  `<none>` is exactly what an unregistered namespace would print. Same shape as
  the field-nobody-writes bug below: when a probe's failure mode and its
  negative result are the same string, capture the exit code and the raw stderr,
  or you have built a decoration.
- **A gate that reads a field nobody writes disables itself silently.** The
  block capacity gate passed its unit tests against a fake and then did nothing on
  its first rig run: it read `total_clusters`, SPDK emits `total_data_clusters`,
  and the driver's own lvstore parser had been returning 0 for that field all
  along. A fake answers with the shape you imagined; only the rig answers with the
  shape that exists. When a check's "safe" branch is *proceed*, a parse failure is
  indistinguishable from a pass — so make the rig assert the REFUSAL, not just the
  happy path.
- **Assert the mechanism, not only the outcome.** A check that "the report
  reached zero" passes identically whether a lease filter cleared it or a row
  quietly vanished — and those two worlds behave differently the next time. The
  block-status drill asserts zero AND that the underlying row is still there,
  so it can only pass for the intended reason.
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
- **A model cannot tell you your code is too STRICT.** Invariant checking finds
  behaviours that break safety; it says nothing about legal behaviours you
  refuse. flint's LAYOUTCOMMIT demanded a live grant row where the model only
  needed the generation to still match — every invariant stayed green while the
  code silently lost data for any client that returned its layout before
  committing (which is what Linux does). Only a real client found it. Where a
  precondition is stronger than the theorem it protects, write down WHICH
  theorem it protects — the gap is where over-strictness hides.
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
  default since 1.22.0**. The roller was blind to block-layout initiators
  until 2026-08-11 — it now refuses a node whose tgt serves a live
  `pnfs-block` export, and the way it was blind is the lesson:
  **a consumer model built from one kind of record cannot see a client of
  another kind.** Block volumes are single-replica, so they failed the
  `replicas_from_pv` guard and never entered the roller's world at all — not
  "no consumer found", but never asked. When you add a class of workload,
  re-derive every safety predicate that enumerates workloads.
- **Ask what deletes each row before you report it as liveness.** The same
  fix nearly shipped a permanent refusal: node attachments are removed by
  ControllerUnpublish, but a client-earned admission is removed by *nothing*
  in the normal lifecycle (only a fence or DeleteVolume), so one ordinary
  unmount would have wedged that node's rolls for the life of the volume.
  Durable-row-as-liveness needs an expiry story, and "who deletes this?" is
  the question that finds its absence.
- Kubelet evictions explain "mystery" pod deaths — the audit log is where the
  answer lives (attach/detach campaign).
- Silent-zero reads are the house failure mode: F67 (MDS state loss ⇒ zeros)
  and six instrument bugs were all "reads zero instead of erroring." Fail
  loudly beats fallback quietly, every time it has come up.
- Durability claims need a power-loss test: the "durable" fence record sat on
  `synchronous=NORMAL` sqlite until a power-off drill emptied it — for block
  volumes sqlite IS the data map, so the pNFS MDS runs `synchronous=FULL`
  (re-priced: the wire path absorbs it; only serial allocator txns pay).
