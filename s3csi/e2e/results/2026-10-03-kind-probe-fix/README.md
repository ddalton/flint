# Kind S9 / S24 / S34, 2026-10-03 — the shared-mounter probe fix

The fix for the defect `../2026-10-03-kind-s31-s34/` found: the join
path replaced a shared mounter whose statfs did not answer in 3 s, and a
mounter whose 16 FUSE threads are all on slow reads does exactly that
while serving every member. Now (`fuse::Probe`, `shared_liveness`):
death needs evidence — ENOTCONN, no mount, or the worker not Running by
a GET; silence from a mounter with none of those is BUSY, and the join
answers `Unavailable` (kubelet retries) instead of replacing. The
republish probe says `MounterUnresponsive` for silence and keeps
`MounterDead` for death.

Images cross-built on the Mac from the fix (binary md5 checked on the
box, `MounterUnresponsive` present in it); one kind node on the box
(load ~10-20 from other sessions' runs). Unit tests on the box: 115/115
s3csi:: + passthrough::, and the new
`a_shared_mounter_is_dead_only_on_evidence_and_silence_is_busy` FAILS
with the old rule put back (silence → Dead) and passes restored.

| run | log | result |
|---|---|---|
| 1 | `run1-legs-S9-S24-S32-S34.log` | S34 no replace, 24/24 readers clean — but the plugin never logged a busy join (the join went in at 8 s, before statfs slowed): **vacuous**, and the old oracle ("statfs ≥ 3 s is BAD") was wrong for the fix. S9 started a second before `reader` published; S32 needs S31's pod. Rig issues, not the fix. |
| 2 | `run2-legs-S9-S24-S34.log` | **86/0.** S34 now joins once a node statfs passes 3 s (here at 20 s; statfs then 8-20 s); the join met the mounter silent twice and waited; no replace, same worker uid, 24/24 readers 0 errors, no MounterDead. S9: a dead per-pod worker still gets MounterDead within 20 s. S24 unchanged. |
| 3 | `run3-legs-S34.log` | **61/0.** Run 2's S34 plus the other arm: the shared worker killed, a new member joins and the plugin REPLACES it — `why=statfs: ENOTCONN (the mounter's FUSE connection is gone)` — and the new member reads through a fresh worker. No other leg kills a shared worker and then joins. |

`plugin-excerpts.log`: the plugin's busy and replace lines from runs 2
and 3 (full logs not kept; 15k lines each).

Not observed on a cluster: a `MounterUnresponsive` event — no member's
republish fell inside the silent window in these runs.
