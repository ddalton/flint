# The crash world at both `OrphanTrack` arms — 2026-09-19, Linux box

`LeanBarrierLeaseSentinelImplCrash1.cfg` on the review's final module
(`LeanSubtree.tla` md5 `c4208d3ad9344ecec355da78b6fdf988`), 8 workers,
20 GB heap, states on the NVMe. The 2026-09-16 Mac run of the TRUE arm was
stopped for disk at 158M states, depth 21 (`../2026-09-16-crash1-orphantrack/`).

- `Crash1OrphanFalse` (02:20–02:27 UTC): `Inv_HITLTracked` VIOLATED at
  depth 21, as SAFETY §4.4 requires of this arm.
- `Crash1OrphanTrue` (02:27–03:19 UTC, 52 min): no `Inv_HITLTracked`
  violation to depth 25 — 148,870,390 distinct states, 549,890,823
  generated, 45M still queued — and the run STOPPED there on
  `Inv_AckImpliesCited` (trace in the log; H10 in the review record,
  open). No sweep action is in that trace: the route is independent of
  `OrphanTrack`, and the FALSE arm never reaches it because it stops
  earlier on `Inv_HITLTracked`. The question §4.4 asks of the TRUE arm is
  therefore still unanswered past depth 25.
