# Validating 1.31.0 on real clusters

The 1.30.0 drill found four defects. Three are fixed in 1.31.0 and one is
not. This run exists to prove the three fixes on the same infrastructure
that found them, and to run the two legs 1.30.0 could not run *because*
of those defects.

Every leg here has an oracle that can fail, and an anti-vacuity guard
that is independent of that oracle. Where a leg's stimulus is
nondeterministic on real hardware (fix 1's device drift), it is replaced
by a deterministic injection of the same state — stated as such, because
"we could not reproduce it" is not evidence of a fix.

## Legs

| Leg | Claim | Oracle | Anti-vacuity |
|---|---|---|---|
| **V1** | A stale `dev` no longer empties the manifest | after a restart with every `tier_generation.dev` deliberately wrong, the manifest keeps ALL its file entries and `beyondRpo` is 0 | the hub must LOG the re-homing (`re-homed N generation row(s)`) with N equal to the number of rows injected — a silent pass means the injection missed |
| **V2** | The clean epoch release lands | after a clean shutdown the epoch object carries `released: true` | the SAME cell must read `released: false` while the hub is live, sampled before the shutdown |
| **V3** | An identity-changing wake no longer pays the lease | hibernate→wake reaches Ready in ≈pod-start, not ≈79s, with `epochTakeovers=0` | a foreign takeover in the same session must still take the full lease — the fast path must not have become "never wait" |
| **V4** | Expired leases decay without inbound traffic | under a real partition `activeLeases` reaches 0 within lease+sweep (~120s) | the same instrument read 1 for 770s on 1.30.0; a live mounted client must still read 1 |
| **V5** | Downloads no longer buffer whole | a 512 MiB GET moves `VmHWM` by ≪ the response size | a 4 MiB GET (below the threshold) must still buffer — same instrument, opposite result |
| **E2** | Hibernate destroys the PVC only after a verified flush, and bytes return identical | new PVC uid, all files byte-identical | one planted corruption must make the comparison report exactly 1 |
| **B4** | The PVC is deleted only after the bucket says released | `DiskReclaimed` timestamp is after the epoch object's `released: true` LastModified | the ordering must be read from the BUCKET's clock, not the operator's |

## What this run does NOT cover

- **The write reserve** (D8) is not fixed in 1.31.0 and is not re-run —
  it would re-measure a known-open defect.
- True EC2 termination, and the 3000-CR fleet budget: same reasons as
  the 1.30.0 drill.

## Rig

Two clusters is unnecessary here — every leg above is single-cluster.
One all-spot cluster (CP + 2 workers, `i4i.large`) and one versioned
bucket, torn down at the end. Cost ≈ $0.13/hr.
