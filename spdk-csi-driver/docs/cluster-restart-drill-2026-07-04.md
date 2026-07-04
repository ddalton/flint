# Cluster-restart drill session (2026-07-04, cluster runk)

**Question drilled:** is a full cluster restart supported? Prior to this
session every drill restarted *components* (one node, one leg, the
controller); no drill had ever rebooted the whole cluster. This session
closes that gap: (a) rolling reboot of all three workers under load,
(b) simultaneous reboot of all three workers under load.

**Cluster.** trove project 28 (`runk`), us-west-1, ALL-SPOT: 1× i4i.large CP
+ 3× i4i.large workers (435 GiB instance-store lvstores on `0000:00:1f.0`).
flint `identity-p3.2` (controller + node DS), spdk-tgt `1.5.0` (patched).
Controller env for the session: `FLINT_EPOCH_SCHEDULER=enabled` (30 s),
`FLINT_CATCHUP=enabled`, `FLINT_HOT_REJOIN=enabled`, cutover off.
Reboots via SSM `AWS-RunShellScript: reboot` (graceful OS reboot — the
realistic maintenance shape; note `ec2:RebootInstances` and
`ec2:GetConsoleOutput` are NOT authorized for the rolesanywhere role, so a
guest-OS wedge cannot be broken from outside — it has to resolve itself).

**Fixture.** 2 Gi `flint-3r` PVC (replicas on all three workers), consumer
on `runk-aws-2`, Nvmeof block frontend; 1 GiB urandom bulk (md5 recorded)
+ 5/s fsync'd append ledger (resume-from-last-seq on restart; gap check =
acked-write-loss check).

## Drill (a) — rolling reboot (aws-1 → aws-3 → aws-2-consumer): PASS

| leg | timeline | heal path |
|---|---|---|
| aws-1 (replica) | reboot 17:57:06 → stale +45 s → node Ready +1 m 50 s → standby +2 m 20 s → **in_sync +3 m 20 s** | thin-aware **full build** (correct: volume was 65 s old — epoch-1 younger than T_back, no eligible shared base), then **inline window 1747 ms** (27 MiB fenced final delta, zero esnap exposure) |
| aws-3 (replica) | reboot 18:00:59 → same cadence → **in_sync +3 m 20 s** | **incremental**: revert to epoch-3, delta replay; **esnap window 164 ms** + 3 s localization — both window flavors exercised in one drill |
| aws-2 (consumer) | reboot 18:05:08 → NotReady +63 s → **guest OS wedged in shutdown ~16 min** → Ready ~+16 m 45 s → pod re-created → writer resumed, all 3 in_sync immediately | no heal needed: with the consumer down nothing wrote, so nothing diverged — records never left `in_sync`×3 |

Ledger: **zero gaps** through all three reboots; bulk md5 identical.

**Finding CR-1 (the session's headline): graceful OS reboot of the
*consumer* node under active write load wedges the guest OS in shutdown
for ~16 minutes.** The replica-only nodes rebooted in <2 min; the consumer
node hosts a dirty ext4 on the nvme-tcp loopback whose backing spdk-tgt is
killed earlier in the same shutdown — the final unmount/sync then hangs
until systemd's timeout cascade gives up. EC2 showed the instance
`running/impaired` throughout (SystemStatus ok — host fine, guest hung).
Consequences:
- **Runbook rule: drain/stop consumers of flint volumes before an OS
  reboot of their node.** A drained consumer node reboots clean (no dirty
  mount) — the (b) clean variant confirms replica-node reboots are the
  benign case.
- Product follow-up candidate: node-DS shutdown ordering (keep spdk-tgt
  alive until kubelet has unmounted flint volumes, or force-detach the
  loopback on shutdown). Bounded (~16 min) but ugly; on spot
  infrastructure with no out-of-band reboot authority this is real outage
  time.
- The eviction at NotReady+5 min removed the bare fixture pod; a
  Deployment self-heals this hands-off (validated in drill (b)).

## Drill (b) — simultaneous reboot of all 3 workers under load

Writer converted to a Deployment (replicas=1, Recreate) so failover is
unattended. All three workers rebooted in one SSM call at 18:21:46 (T0);
kubernetes CP stayed up for observability; the flint controller (on a
worker) died and rescheduled with the workers.

**Timeline (T0 18:21:46, all three workers rebooted in one SSM call):**

- +65 s: all three NotReady; writer's last acked append rode the collapse
  (raid queued, then died with the node) — nothing acked was lost.
- +66 s→+1 m 6 s: **aws-1 and aws-3 Ready again** (replica-only nodes are
  the benign case, again). flint controller rescheduled with them.
- +5–6 min: eviction removed the writer pod on dead aws-2; the Deployment
  created a replacement on aws-3, which correctly parked on the
  **Multi-Attach guard** (RWO volume still attached to aws-2).
- +10.7 min: **aws-2 guest OS finally finished its wedged shutdown** (CR-1
  again, 10.7 min this time) and returned; its kubelet finalized the old
  pod; detach completed; `VolumeDataPathLost` fired correctly on aws-2 (a
  live attachment with no raid — the first-strike visibility fix working).
- **+15 m 15 s: writer Running on aws-3** — ControllerPublish fenced and
  re-homed the attachment, NodeStage assembled a **full 3/3 raid
  immediately**: with the consumer dark the whole blackout, no writes
  flowed, so no replica diverged — `in_sync`×3 records were correct
  throughout and **no catch-up was needed at all**.

Ledger: **zero gaps** (resume generation started at exactly
last-acked + 1); bulk md5 identical. Zero `ReplicaStale` events in the
entire drill.

**Reading:** a simultaneous whole-cluster blackout is the *easy* case for
the storage layer — fail-stop with nothing acked lost, and because nobody
can write while legs are missing, recovery is pure reassembly with no
rebuild. The outage duration is dominated by CR-1 (the consumer-node
shutdown wedge) plus the Kubernetes eviction/Multi-Attach serialization
(~4.5 min of the 15). Replica-only nodes reboot in ~70 s.

## Drill (c) — control-plane node reboot under load

CP (single control-plane, also spot) rebooted at 18:38:28 with the writer
live on aws-3. Expectation: the data path is API-independent — writes
continue through the entire API outage; the ledger's per-line timestamps
are the proof (post-hoc gap analysis over the dark window).

**PASS — the data path is fully API-independent.** API down 73 s after the
reboot command (clean graceful shutdown — no consumer mount, no CR-1);
guest OS back in ~2.5 min, kubelet + apiserver listening shortly after.
The ledger recorded **2522 appends across the dark window with zero
time-gaps > 2 s and zero sequence gaps** — the writer never noticed.
Orchestrators (controller lives on a worker) lost the API briefly and
resumed cleanly; epoch cuts continued after the window.

**Finding CR-2 (trove, not flint): the management WG tunnel does not
survive a CP reboot.** `/etc/wireguard/wg0.conf` is provisioned but
`wg-quick@wg0` is never enabled, so the tunnel — and ALL kubectl access —
stays dead after boot until someone runs `wg-quick up wg0` out-of-band
(via SSM here). ~6 of the ~9 observed dark minutes were this, not
Kubernetes. Fixed live on runk + `systemctl enable wg-quick@wg0`;
permanent fix belongs in trove's provisioning path. Operational note: a
CP reboot therefore looks like a total management blackout on trove
clusters today — check the tunnel before diagnosing the cluster.

## Verdict

**Full cluster restart is supported, with one operational rule.**
Reboot-in-place (rolling or simultaneous) loses nothing: every acked write
survived every variant, records stayed truthful, and every heal was
hands-off (catch-up chose full-build vs incremental correctly; both hot
rejoin window flavors committed; the Deployment failover needed no
operator). The rule: **drain workloads off a node — or accept ~10–16 min
of guest-OS shutdown wedge (CR-1) — before an OS reboot of a node that
hosts a flint consumer.** Replica-only nodes reboot freely (~70 s).
Full cluster *stop* (instance stop/terminate) remains unsupported by the
substrate: replicas live on instance-store NVMe, which does not survive
it — that is backup/DR territory, not restart.

## Baseline gate addition (same session)

The incremental-rebuild pipeline is now a **standing kuttl suite**
(`tests/system/kuttl-testsuite-replica-rebuild.yaml`, `make
test-replica-rebuild`, wired into `make test` after clean-shutdown):
2-replica volume + fsync writer, non-consumer leg killed via spdk-tgt,
asserts stale → catch-up → re-admission → `in_sync` + md5/ledger/zero
writer restarts. Self-contained: enables the orchestrator env if dark and
restores the exact prior state after (container name resolved dynamically
— the controller deployment's driver container is `flint-csi-controller`,
not `flint-csi-driver`, and `kubectl set env -c` silently no-ops on a
mismatch). First validated on runk from dark chart defaults: PASS in
518 s (stale +52 s, healed ~7 min at 30 s epochs, admission via hot
rejoin, writer untouched). Two authoring findings worth remembering: the
replica-sync-state PV annotation is a JSON *string* (`jq fromjson`
required), and kuttl ≥0.15 ignores `$patch: delete` manifests — use the
TestStep `delete:` list.
