# F46 — a node that briefly served leaves residue that permanently blocks the next server

**Status:** FOUND 2026-07-27 live on cluster `runae` (drill 3.6e run 3,
driver `1.20.0-rc4`); **FIXED same day** (§6 — mint unification + belt +
teardown hygiene + defer-bound fix + liveness marker; 863 lib tests);
**LIVE-VALIDATED same day on cluster `runag` (drill 3.6e run 4, driver
`1.20.0-rc5`)**: chain kill→swap 172s→standby 194s→cutover 301s→in_sync
344s, settle HELD with the pod-liveness gate (nfs Running/Ready, no
assembly-blocked marker, head_in_use=0), latent-pin sweep clean, db PASS
zero acked loss with no manual intervention, and the post-drill export
capture (`tests/chaos/artifacts/3-3.6e-1785166909/export-domains-post-drill.txt`)
shows every leg exported under a single inner-domain NQN — no wrapper
subsystems, no empty shells, on any surviving node. **Severity: P1** — the
volume reports a perfectly healthy `2/2 in_sync` record while being
**completely unmountable**, with no self-heal path. Predicted in advance by
[F45](f45-identity-domain-audit.md) as finding **S3** ("the systemic one");
this is S3 realized in production terms.

Related: [F44](f44-cutover-leg-detach-leak.md) is the same *family* —
residue left on a node the NFS server moved off — but a different object
(F44: an initiator controller; F46: the export/claim state of a leg head)
and a different victim (F44 blocked catch-up; F46 blocks assembly).

## 1. What the drill showed

Run 3 reproduced the entire F43+F44 chain cleanly, then died anyway:

```
F42 no-stall ✓ · identity swap → aws-4 269s · standby 280s
nfs BOUNCED 397s (cutover took the claim) · in_sync 2/2 429s ✓
settle +360s: HELD (2/2 in_sync, writer_set=2, head_in_use_events=0) ✓   ← F44 fix holds
LATENT PIN: volume controllers still attached on non-server node: runae-aws-4  ← new sweep fires
```

…and meanwhile the NFS pod was **Pending, unable to mount**, for the whole
settle window. The record and reality had diverged.

## 2. Symptom

```
AssemblyDeferred (forever, ~every few s):
  F36c: deferring degraded assembly on runae-aws-1 — last-writer leg(s)
  transiently unavailable (3651662d… on runae-aws-4 (claim-blocked));
  bound 180s, then serve-with-risk

FailedMount:
  Failed to create RAID: F36c freshness gate: last-writer leg(s) transiently
  unavailable (3651662d… on runae-aws-4 (claim-blocked))
```

The pod ping-pongs: cutover lands it on aws-4 → `FailedMount` → rescheduled
to aws-1 → `FailedMount` → … Neither node can assemble.

## 3. Evidence (all captured live)

**The record claims full health:**
```
replicas: aws-1 44a81b49 (live -)        in_sync
          aws-4 3651662d (live faa78582) in_sync
writer_set: [3651662d, 44a81b49]          ← 2 writers, looks perfect
```

**The leg is genuinely attached and current on the consumer (aws-1):**
```
nvme_nqn_…_volume_pvc-5574462a…_1n1   uuid=faa78582   ← the LIVE head, attached
```

**But aws-4 exports that head under BOTH id domains — one of them empty:**
```
nqn…:volume:nfs-server-pvc-5574462a…_1   hosts=node:runae-aws-1   ns=(EMPTY)   ← wrapper
nqn…:volume:pvc-5574462a…_1              hosts=node:runae-aws-1   ns=faa78582  ← inner
```

**And the head is claimed, with no raid holding it:**
```
bdev_raid_get_bdevs on aws-4 → (none)
head faa78582 → claimed=true  claim_type=exclusive_write
```

**Plus a stale wrapper-domain controller from aws-4's ~5-minute stint as
server** (06:49:32 → 06:54:30), pointing back at aws-1's leg:
```
aws-4: nvme_nqn_…_volume_nfs-server-pvc-5574462a…_0 → 172.31.8.159 (aws-1)
```

## 4. Analysis

**Proven:** the leg the gate calls unavailable is attached, live, and
current on the consumer; the only thing "wrong" with it is a claim, and the
claimant is its own NVMe-oF export (nvmf namespace registration takes
`exclusive_write` — the legacy v1 claim path noted in the F43 SPDK
analysis). No raid holds it. The volume is unserviceable indefinitely.

**Inferred (needs code confirmation before fixing):** the "claim-blocked"
attribution is **name-keyed and single-domain**. The consumer stages under
the wrapper handle, so it looks for the leg's export as
`volume:nfs-server-<pv>_1` — which exists but is EMPTY — while the namespace
actually lives in the inner-domain `volume:<pv>_1`. The gate therefore
concludes the head is exported to *somebody else* and refuses. This is
exactly F45's S3: replica-leg export NQNs are minted in different domains by
stage (wrapper) vs catch-up (inner) vs replica-node reconcile (wrapper), so
any name-keyed matcher over legs is guessing.

The dual subsystem is itself the bug's fingerprint: a leg admitted by
catch-up gets an inner-domain export, then reconcile re-mints the wrapper
shape — empty, because the namespace is already claimed by the inner one.

## 5. Why runs 1–2 didn't show it

Run 1 died earlier (F44 deadlock). Run 2 died differently (F45/B2 subsystem
migration). Only run 3 — with F44 and B2 both fixed — survived long enough
for the *second* cutover to land the pod on a node that had already served,
which is what creates the dual-domain export. Each fix has been extending
the reachable frontier by one failure.

## 6. Fix (IMPLEMENTED 2026-07-27, one commit with corrections to the
## original directions)

1. **Unified the replica-leg export mint** (F45 S3) — DONE, at the choke
   point: `identity::replica_export_nqn` now normalizes through
   `storage_id_of_handle`, so every mint site (stage-side assembly, which
   previously built `volume_nqn("<staged>_<i>")` by hand; catch-up; the
   node-agent reconcile, keyed on the wrapper volumeHandle) produces the
   inner shape from whichever handle it holds. Hot-rejoin pads got their
   own `export_pad` trait method so they stop squeezing through the leg
   naming. `nvmeof_export::resolve_replica_export_nqn` is the transition
   belt: a leg still served by a pre-unification wrapper export is
   **adopted** (a live consumer may be attached through it), a fresh mint
   is canonical, and an EMPTY wrapper shell — the exact run-3 state — is
   retired on sight.
2. **Claimant-aware probe — SUBSUMED, with a correction.** As originally
   written ("is the claimant a host other than my consumer?") it would
   not have fixed run 3: the claimant host WAS the consumer
   (`hosts=node:runae-aws-1`), so detection alone leaves attach failing
   against the wrong-domain subsystem. The belt's adopt-existing-export
   behavior IS the claimant-aware fix in load-bearing form: assembly now
   targets the subsystem that actually holds the namespace.
3. **Teardown residue — DONE, but scoped narrower than stated.** "Clear a
   briefly-serving node's export/claim state" taken literally would sever
   live consumers: run 3's inner export ON aws-4 WAS the legitimate
   serving export for aws-1's leg attach. Teardown step 4 now retires
   only per-replica export subsystems that are **empty** (exact NQN shape
   in either domain, zero namespaces = no data path); ns-bearing leg
   exports survive unstage untouched.
4. **Record honesty — DONE, with a correction: `in_sync` is not demoted.**
   Both legs genuinely held every acked write — demoting would trigger a
   pointless rebuild of a perfect leg. Instead a distinct liveness marker
   `flint.io/assembly-blocked` (PV annotation, `<since>|<detail>`) is set
   while the F36c gate defers and cleared when it stops. Data lineage and
   assembly liveness are now separate signals.
5. **The wedge is bounded (new item — the piece the original list
   missed).** The defer deadline re-armed whenever the missing set
   *changed*; run 3's server ping-pong alternated the missing set
   {A}↔{B} faster than the 180s bound, making the defer unbounded.
   `freshness_gate::change_is_progress` now re-arms only on a strict
   shrink (genuine partial progress); oscillation and growth carry the
   running deadline, so any future claim-shaped collision degrades to
   the evented serve-with-risk path instead of deferring forever.

Also in the same change: F45 S5's dead delete in DeleteVolume now removes
`replica_export_nqn` in both domains (the alias shape it deleted was
minted by nothing).

## 7. Drill hardening applied / needed

- **Applied:** the latent-pin sweep caught this (it fired on aws-4) — keep
  it as a hard gate.
- **Needed:** the settle assertion must check the NFS pod is **Running and
  serving**, not merely that the record says `2/2 in_sync`. Run 3's settle
  window passed while the pod was Pending. Record state alone is not
  liveness.

## 8. Artifacts

`tests/chaos/artifacts/3-3.6e-1785134584/` — cluster `runae`, trove
project 49, driver `1.20.0-rc4` (sha256:6b826dfe…).
