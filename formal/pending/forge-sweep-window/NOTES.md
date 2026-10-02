# Lesser finding 5: the sweep's HEAD-to-DELETE window (2026-10-02)

flint-27 found it in the 09-23 review and handed it to flint-46 with no prior sandbox.

## The claim under test

`sweep.rs::sweep` reads the snapshot once per pass and checks its etag. Then, per candidate, it HEADs the key (rule 2: the age is past the grace by the store's clock) and later issues an unconditional DELETE, across awaits. `fold.rs::sweep_ledger` has the same shape over the keys a fold superseded.

The race is between two processes:
- The sweeper is a holder deposed mid-pass that hasn't noticed yet.
- A successor takes over, restores, and accepts a retried push. A retry carries the same objects, so its pack has the same content-derived name. The successor re-uploads that key and names it.
- The sweeper's pending DELETE then removes a pack the snapshot names.
- A conditional delete can't tell the two uploads apart: same bytes, same etag.

## The model

`ForgeSyncSweepWindow.tla` is `../forge-needed/ForgeSyncRewind.tla` (md5 5ee1b260) with these additions:
- **`SweepSplit`:** `SweepHead` checks the etag and the grace, and records the candidate. `SweepDel` deletes it later, unconditionally.
- **The loop's serialization:** while this process has a DELETE pending, its own BatchStart, Rewind and FoldCommit wait.
- **A crash** clears the pending DELETE.
- **CleanRelease** waits for the pass. A release is the loop's last act, and the first trace was unfaithful without this.
- **`PushRetry`:** a client retries a push it was told failed (budget `MaxRetries`).
- **`SweepRechecks`:** the DELETE first re-checks the snapshot. This is the obvious fix.
- **`SweepUnderLease`:** the DELETE happens only while this process holds the lease. This is the fix that shipped.
- **Probes:** `ProbeSweepTakesNamed` (the interleaving is reachable) and `ProbeSweepDeleted` (under the fence a DELETE still happens).

The retry worlds (`gen.sh`) have no folds, rewinds or re-pushes: only the window.

## Results (tlc-rs; TLC is in RESULTS.txt)

| world | result |
|---|---|
| Retry (the code) | Inv_LandedPackComplete VIOLATED, depth 33 |
| RetryRechecks (re-check the snapshot) | VIOLATED, depth 33 |
| RetryAtomic (the old one-step sweep) | HOLDS, 16,155,793 distinct |
| RetryUnderLease (only the holder deletes) | HOLDS, 16,724,450 distinct |

**The code's trace** (`out/Retry-counterexample.txt`):
1. s1 is serving, and its sweep HEADs orphan p1, left by a crashed batch whose client was told it failed.
2. s2 takes over and rotates the snapshot.
3. The client retries p1 against s2, and s2 uploads p1.
4. s1, deposed, DELETEs p1.
5. s2's CAS names p1.

**Why the re-check fails** (`out/RetryRechecks-counterexample.txt`):
- A straggler's restore completed onto the successor's rotated snapshot, so its belief matches the current etag.
- The holder's upload of p1 waits for its CAS, and the snapshot doesn't show it yet.
- The straggler's re-check therefore passes, and it deletes p1.
- No snapshot check can see an in-flight upload. In the atomic model only the grace covered that.

**What closes it:** only the lease holder deletes, which is lean's R4b. A holder's own uploads are serialized with its sweep.

## Limits

**The model's grace** counts only in-flight uploads as young, so a crashed batch's orphan is sweepable at once. In reality the HEAD would see a recent upload and skip it. The pattern doesn't depend on that, though. It needs an orphan older than the grace (a retry more than an hour after the failure) to be re-uploaded between the HEAD and the DELETE, and a re-upload that lands after the HEAD refreshes the age too late.

**The lease check** is atomic with the DELETE in the model. In code it's a renewal (`lease::renew`) immediately before the DELETE:
- A deposed process's renewal returns 412 and it's fenced.
- A live holder's renewal moves the token, so a challenger must restart its count of `QUIET_POLLS` quiet polls. The DELETE therefore lands inside the holder's term.

A stall exactly between the renewal and the DELETE request leaves the same residual time window every lease-fenced write here already has.

## The code fix

The fix renews before each DELETE in `sweep::sweep` and in `fold::sweep_ledger`. Its tests, each failing on the shipped code and with its own renewal removed:
- `a_deposed_sweepers_delete_never_takes_a_pack_its_successor_named`
- `a_deposed_ledger_sweep_never_takes_a_pack_its_successor_named`

## Cost and cadence (flint-27's question)

**Requests.** The fix adds one conditional PUT on the lease cell per DELETE.
- `sweep::sweep` has no request budget. Its deletes are the orphans past the grace, about two per push under tiers, once per `sweep_every_secs`.
- `fold::sweep_ledger` counts the renewal in its budget (`requests += 2` per delete), so a pass deletes about half as many keys and still holds the loop about a second.
- A cheaper form, renewing only when the last landed renewal is older than half the term, is possible. It was not taken: the per-DELETE renewal is the simple proof.

**Takeover.** The sweeps run on the maintenance tick in phase `Serving`, which isn't a must-progress phase. In that phase the renewer task already renews on every heartbeat, so a live serving holder's token is always moving and no challenger can take it over.
- The extra renewals don't change who can win a takeover or when. A challenger still wins only against a stalled or dead holder, and a stalled process makes neither kind of renewal.
- What they add is a synchronous proof, at the DELETE, that this process still holds the lease. A deposed process's renewal returns 412 and it's fenced.
