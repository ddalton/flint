# F54 — E_f host-fence duplicate fails the retry: the F48 classification bug, one RPC later

**Found:** 2026-07-28, runak (trove project 55), drill 2.9 on `1.21.0-rc4`.
**Severity:** MEDIUM — no data risk; costs one full hot-rejoin backoff cycle
(~7 min of avoidable degraded time) whenever an admission retries over its own
residue. **Fixed same day** (this tree): converge probes for every nvmf
builder in `prestage`.

## 1. What happened

Drill 2.9 (destroy the remote leg's lvstore in place) needed three hot-rejoin
attempts; in_sync landed at T0+1266s vs runaj run B's 264s. The middle attempt
was pure waste:

- Attempt #1 (02:53–02:54) built the E_f export skeleton on the survivor
  (aws-2): `nvmf_create_subsystem` + `nvmf_subsystem_add_host` +
  `nvmf_subsystem_add_listener`, entered the window, failed at the ns swap
  (consumer-side zombie controller — see §3), and unwound **leaving the
  skeleton in place** (in-window unwinds keep prestage state by design; only
  a prestage-stage failure tears the skeleton down).
- Attempt #2 (03:01:17) re-ran prestage over that residue:
  - `nvmf_create_subsystem` → `-32603 Unable to create subsystem` — the F48
    duplicate shape; the F48 probe **adopted it correctly**.
  - `nvmf_subsystem_add_host` → bare `-32603 Msg=Internal error`. SPDK
    reports a duplicate host with **no recognizable text at all** (spdk-tgt
    logs nothing for it either). `is_already_exists` cannot match it, so the
    parse-only arm failed the whole admission for a subsystem that was
    already in the wanted state. Backoff: 7 minutes.
- Attempt #2's unwind (prestage-stage failure) deleted the skeleton; attempt
  #3 (03:08:17) built it fresh and committed first try (window 267ms).

F48's fix stopped at the create call because that was the only shape runah
produced. The lesson generalizes: **every** nvmf builder in the admission
path can be re-run over residue, and SPDK's duplicate errors are untextual.

## 2. The fix

`prestage` (hot_rejoin.rs) converges by **probing state, not parsing
messages**. On any failure of:

- `nvmf_create_subsystem` → probe: subsystem present? (`get_subsystem`,
  the pre-existing F48 probe, now shared)
- `nvmf_subsystem_add_host` (E_f fence AND copy-source re-admit) → probe:
  host in the subsystem's `hosts` list? (`subsystem_has_host`)
- `nvmf_subsystem_add_listener` → probe: `traddr:trsvcid` in
  `listen_addresses`? (`subsystem_has_listener`)

present → converged, continue; absent (or probe unreachable) → fail with the
original error — the honest direction. Test mock now reproduces the faithful
duplicate shape for add_host and exposes `hosts`/`listen_addresses`;
regression test `residual_ef_with_host_and_listener_does_not_fail_the_admission`
seeds the exact runak residue. 893 tests green.

## 3. The other backoff cycle (recorded, not changed)

Attempt #1's ns-swap failure ("old ns still visible on consumer within the
AER budget") was the F48 zombie-consumer shape: the lvstore destruction
faulted the raid base on the consumer but left the leg-0 nvme controller +
bdev behind. Prestage's consumer-side check (`get_bdev` presence) passed —
presence ≠ live path — and the sever couldn't fire earlier because its
criterion is a **stale** head while the leg was still *standby*; the failed
window's own demotion made the zombie severable (sever fired 57s later,
attempt #3 then succeeded). So the first backoff is an inherent
one-cycle race on current code, bounded by the sever. A deeper fix (probe
controller path-liveness in prestage, or extend the sever to standby heads)
is deliberately deferred — not a blind release-eve change.

## 4. Live validation

Gate: drill 2.9 on the fixed image must reach in_sync without an
`E_f host fence`-classified failure; the fully-built-residue path is only
exercised when a window fails first, so the unit/regression tests carry the
residue case and the drill carries the end-to-end timing.
