# runam campaign — P4/F54§3 live gate + the F55 find (2026-07-28)

**Cluster:** runam (trove project 57), 4× i4i.xlarge workers + spot CP, all
spot, us-west-1, k8s v1.34.10. **Image:** `dilipdalton/flint-driver:1.22.0-rc2`
(amd64, `sha256:e690a928…`) built from main @ `9e8be9e` — the P4
dead-target-timeouts (`7feb876`) + P2 tranche 2 (`ff38bd0`) + F54 §3
(`9e8be9e`) stack. Chart 1.21.0, driver tag overridden. Deleted same day,
zero residue verified both regions.

**Verdicts: P4 CLOSED live (stall 36s ≤ 60s budget, was 150–177s). 2.9 ×3
PASS (no regression). F55 found by the drill, attributed to a deterministic
mechanism, and fixed same day (`a4902ef`) — its live gate is owed.**

## P4 gate — 3.6e: stall 36s (budget 60s)

`degrade=76s` from the terminate API call (~30–40s of that is the instance
actually dying; then TCP_USER_TIMEOUT ≈8s + fast_io_fail 20s — the design
math landed). Ledger gap through the kill: **36s**. Whole chain sped up:
swap 129s (was 213s), standby 140s, cutover 256s, in_sync 298s. All
F43/F44/F46 criteria green (settle held, latent-pin sweep clean, cutover
1/1/0, yields=0 seizures=0, witness clean, no acked-tail risk). The
`Dead-target timeouts applied` marker verified on all five nodes before
any drill ran.

## 2.9 ×3 — F54 §3 / tranche-2 no-regression gate

in_sync 397s / 374s / 354s, stall ≤2s, 0 window flips, 0 severs, 0 E_f
dups, db PASS ×3 — the runal class, no regression from the new stack. The
zombie race didn't fire in any run; §3's race path stays carried by its
red-verified regression test.

## F55 — found, attributed, fixed (docs/f55-bounce-truncated-reply-eio.md)

3.6e's db verdict FAILed on the pglog scan: postgres checkpointer PANIC
(`could not fsync ... Input/output error`) 4s after in_sync, abort wedged
~5min, 227s outage, **zero loss (1663/1663)**. Attribution chain, all on
this cluster:

1. New-instance innocence: no error replies logged, state fully restored
   (2 clients / 38 stateids), sessions re-created in ~1s; mount is `hard`.
2. Deterministic repro: bounce mid-checkpoint (checkpoint_timeout=30s) →
   PANIC 0.6s after pod delete, healthy 2/2 raid, no node loss. The
   dying instance's streamed log (`f55-outgoing-server.log`) shows its
   "connections dropped" SIGTERM line 6ms before the client-side PANIC.
3. Quiesced-bounce discriminator: idle bounce → 0 PANICs, post-bounce
   forced sync write OK. The poison strictly requires a reply in flight.

Mechanism: runtime shutdown cancels connection tasks mid-reply-write — a
truncated frame, unlike an unanswered request, is not retransmittable.
Fix: `DrainGate` frame-atomic shutdown (drain-then-exit, bounded 3s).
Every prior "clean" 3.6e simply never straddled a checkpoint.

## Fleet note

The replacement worker requested mid-campaign (`runam-aws-1785269786`,
trove timestamp naming) was spot-reclaimed ~2min after joining —
i4i.xlarge capacity thin in us-west-1 again. The F55 repro was redesigned
to consume no nodes instead.

## Files

- `3-3.6e-1785268106/` — the 3.6e artifacts (P4 gate + F55 first observation)
- `f55-outgoing-server.log` — dying instance's log, first repro
- `f55-quiesced-outgoing.log` — quiesced-bounce discriminator
- `runam-36e.log`, `runam-29-run{1,2,3}.log` — drill stdouts
- `../results.csv` — one row per drill
