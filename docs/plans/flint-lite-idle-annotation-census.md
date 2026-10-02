# Idle annotations: who writes, who reads, what each reader assumes (2026-10-02)

Why: the wake fixes (`b85f67bb`) changed one WRITER of
`chert.us/requested-at` (the NFS proxy now re-asks every 5 s while a hub
starts) and broke three READERS, found one box run at a time. This lists
every writer and reader of the annotations that coordinate the idle
lifecycle, with the rule each reader relies on, so the next change to a
writer starts from the list. Method: `git grep` on BOTH the constant and
the literal spelling, all file types (rigs, charts and docs write these
too).

## `chert.us/requested-at`: "someone wants this up"

One key, many writers, two contracts.

| Writer | When | Clock |
|---|---|---|
| NFS proxy `nfs_proxy/kube.rs` | a compound for a down hub; at most every 5 s per hub, for as long as it holds compounds | proxy pod |
| hub-gateway `POST …/wake` (`lite_gateway/proxy.rs` `arm_wake`) | EVERY call, also on a running share (a keepalive, by design) | gateway pod |
| hub-gateway git door (`lite_gateway/git.rs`), FlintRepo only | once per request, skipped while a live stamp exists | gateway pod |
| admins, `docs/flint-lite-operator.md` (`kubectl annotate … requested-at=$(date …)`) | once | the admin's laptop |
| rigs: `lite-ladder-cycles.sh`, `gateway-kind-e2e.sh`, forge e2e | once | rig host |

| Reader | Rule it applies | Since |
|---|---|---|
| lite `idle::decide`, down branch (`wake_requested`) | live (younger than `suspendAfterSecs`) AND not older than `idle-since` | `b85f67bb` |
| lite park-as-CR (`reconcile.rs` 4b, `wake_requested`) | same | `b85f67bb` |
| lite hibernate verification (`verify_yields_to_wake`) | not older than `idle-since` | `b85f67bb` |
| lite disk reclaim (`reclaim_yields_to_wake`) | not older than `idle-since` | `b85f67bb` |
| lite `idle::decide`, running branch | age < `suspendAfterSecs` holds the share up | earlier (the stamp is a keepalive) |
| lite `implausible_request` | a stamp further in the future than one threshold is ignored and reported | earlier |
| forge operator (`forge_operator/idle.rs`) | live only; the stamp is NEVER cleared (X24) | earlier |
| gateway `ShareView.wake_requested`, share | presence; **nothing acts on it** | n/a |
| gateway `ShareView.wake_requested`, repo | live; the git door's re-arm brake | earlier |

Lite's contract: the operator CLEARS the stamp when it starts a wake.
Forge's: never cleared, read by age.

**Findings.**
1. **The new rule compares two clocks.** The stamp carries its writer's
   clock and `idle-since` the operator's. A request made within the skew
   just after a transition reads as older than it and is ignored.
   Measured in the sequence test, writer clock behind:
   - a one-shot writer 3 s behind loses 20 of 8,800 sequences;
   - the proxy (re-asks every 5 s) is fine up to 5 s and late past it
     (18 of 8,800 at 8 s).

   NTP'd nodes are far inside that; a laptop's `kubectl annotate` or a
   badly synced node is not. Options:
   - (a) a tolerance: count a stamp up to `WAKE_EVERY` (5 s) before
     `idle-since`. The stale re-ask is written during a hub's startup,
     and the next NATURAL transition is at least `suspendAfterSecs`
     later, so only a forced transition within 5 s of a re-ask would
     see it again;
   - (b) remove the source: the proxy stops re-asking once the share's
     `idle-state` is `Active` (it already watches FlintShares). Then the
     only stale stamps are keepalives on running shares, which the
     live-window rule already handles;
   - (c) both. **Recommended: (c).** (b) removes the cause, and (a)
     keeps one-shot writers safe against modest skew.

   **Done (c), test-first:**
   - `idle::REQUEST_SKEW_SECS` = 5: a stamp up to 5 s before `idle-since`
     counts;
   - the proxy's `HubRow::wants_a_stamp` is false for a share whose
     `idle-state` is Active (its table lags the watch by about 2 s, which
     is inside the allowance).

   The sequence test now has three writers:
   - the proxy, with that 2 s lag;
   - a one-shot writer;
   - the gateway keepalive, which stamps running and starting shares
     too, so stale stamps still exist for the operator to ignore.

   And a writer clock 3 s behind. Red before: 584 of 14,880 sequences,
   a one-shot writer 3 s behind stranded. Controls: allowance 0, and
   each of the three `b85f67bb` fixes reverted, all fail (584; 1,360;
   344; 1,360).

   One trap on the way: once the simulated proxy stopped re-asking for
   Active shares, the reverted fixes all passed. The proxy had been the
   test's only source of stale stamps. The keepalive writer restored
   them.
2. **`ShareView.wake_requested` for a share is presence and unused.**
   Its comment now says so (`b85f67bb`). Either drop it for shares or
   compute it with the operator's rule, so the next reader cannot pick
   up the wrong one. **Done:** it is `idle::woken_since_idle`, with a
   test.
3. **One key, two contracts.** Lite clears it; forge never does. The
   shared `idle::clock` keeps the age rule identical; the since-idle
   rule is lite-only. Worth saying in the CRD docs of both kinds.

## `chert.us/idle-state` and `chert.us/idle-since`: what the ladder did, and when

| Writer | Notes |
|---|---|
| lite operator: `idle_state_patch` only (ladder, verify, rungs) | since `7f4ebf77`, the one writer of a transition |
| forge operator `forge_operator/reconcile.rs` | its own patch; Active/Suspended only |
| rigs: `step5-scale-kind.sh`, `step6-box-fleet.sh`, `fleet-scale.sh` (shares born parked) and `step6-box-wake*.sh` (forced transitions) | always write BOTH |

| Reader | Use |
|---|---|
| `idle::state_of`, render | replicas 0 when down; claim plan |
| `idle::decide` | `down_for` for the hibernate rung |
| `woken_since_idle` | the request boundary (new meaning, `b85f67bb`) |
| `lite-ladder-cycles.sh` | counts distinct `idle-since` values as its move oracle |

**Findings.**
4. **`idle-since` now has a second job.** It is the boundary a wake
   request must beat. A writer that sets `idle-state` WITHOUT
   `idle-since` gets `woken_since_idle`'s fallback (presence), the old
   behaviour. Every in-repo writer sets both. **Done:** documented in
   `docs/flint-lite-operator.md` (Waking) and in `idle.rs`'s module doc.
   The same doc's claims that the wake acts on the key's PRESENCE, and
   that suspension "avoids comparing clocks", were wrong and are
   corrected.

## `chert.us/wake-intent`: warm or cold import for the next boot

No writer in the repo; documented for front doors. The render reads it.
The operator clears it once the share is Ready AND Active.

**Finding.**
5. **An intent can outlive its wake.** A wake that turns into a hibernate
   verification never reaches Active, so the intent is not cleared. It
   then:
   - applies to the verification boot's import (a warm fill for a hub
     about to hibernate);
   - survives into the next wake.

   It is a hint, so the cost is one boot in the wrong mode. **Done:**
   every idle transition except into Active drops it (`idle_state_patch`,
   the one writer of transitions), with a test.

## The rest: single-writer or explicit intent, no finding

- `render-hash`, `render-verified-at`: the operator, on its own
  Deployment (the loop fixed in `66e47b9a`).
- `persistence-target`: the operator only, and it carries the size it was
  computed against, so a stale target is recognisable. This is the
  pattern to copy: a stamp that says what it answers.
- `abandon`: an admin's explicit intent; presence is the contract, and
  nothing races to clear it.
- `api-token-version`: admin-written, monotonic, parsed defensively.

## Gaps

- No formal model covers the idle ladder or the wake protocol.
  `formal/FlintShareDisk.tla` models the request as a boolean, for
  rebuilds only.
- The sequence test (`reconcile::tests::idle_sequence`) covers lite only:
  not forge, not one-shot writers, not clock skew. Adding a skewed
  one-shot writer would pin finding 1.
