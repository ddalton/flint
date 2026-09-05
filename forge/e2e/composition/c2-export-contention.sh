#!/usr/bin/env bash
# C2 — a read-write lean workspace mounted over forge's EXPORT prefix.
#
# THE LEGAL SHAPE, VIOLATED IN THE ONE WAY THE DESIGN NAMES. Prefix B
# is forge's export: forge writes it, lean and passthrough read it. The
# design says a lean workspace mounted READ-WRITE over B would be a
# second writer of that manifest. Unlike C1, this violation IS
# arbitrated — forge's export runs the real `flint-sync` binary, so
# both parties contend on the SAME cell, <B>/.flint/lean/epoch.
#
# SO THE QUESTION IS NOT "IS IT CAUGHT" BUT "WHAT DOES CATCHING IT
# COST". When this drill was first written the answer was: everything.
# Three shipped facts composed —
#
#   1. `flint-sync`'s claim loop never gives up: `Waiting` sleeps 10s
#      and retries forever (bin/flint_sync.rs:290-317).
#   2. `export::run_barrier` awaited that subprocess with NO timeout.
#   3. `export::maybe_run` is awaited INLINE in the serving loop
#      (server.rs:288), and the lease heartbeat is a timer on that same
#      `select!` (server.rs:6,158-199).
#
# — so a second writer on B did not merely lose. It stopped forge's
# heartbeat and its pushes, and the casualty was the repository's lease
# on prefix A, a DIFFERENT prefix from the one misconfigured. Measured
# on this rig: the lease froze at an unchanged timestamp and a push
# that had been acked in 0s timed out at 30s.
#
# THE FIX, WHICH THIS DRILL NOW GUARDS. `run_barrier` spawns the child
# with `kill_on_drop` and waits under `FLINT_FORGE_EXPORT_TIMEOUT_SECS`
# (default 300, the export floor's default); on elapse the child is
# killed and the error is `ExportBlocked`, which names the prefix and
# the likely cause. `plan` then holds the export off for one timeout
# before retrying — without that the loop would re-enter the doomed
# barrier on the next batch and rebuild the same outage one batch at a
# time.
#
# So the drill now asserts RECOVERY, not just the wedge: the lease
# resumes, pushes resume, and the log says why. The violation is still
# a misconfiguration and the export still does not run — what changed
# is that it no longer takes the repository with it.
#
# ANTI-VACUITY. "The lease stopped advancing" is only evidence if it
# was advancing before, and "the push hung" is only evidence if pushes
# worked before. Both are measured on this rig, in this run, first.
#
#   bash forge/e2e/composition/c2-export-contention.sh
set -uo pipefail
cd "$(dirname "$0")/../../.."
export WORK=${WORK:-/tmp/fc-c2}
rm -rf "$WORK"; mkdir -p "$WORK"
source forge/e2e/composition/rig.sh
trap rig_clean EXIT
rig_init || { echo "rig_init failed"; exit 1; }
rig_purge c2/

A=c2/A; B=c2/B
TMO=${TMO:-20}   # export barrier timeout for this drill; default is 300
renewed() { s3_cat "$A/git/epoch" | sed 's/.*"renewed_unix":\([0-9]*\).*/\1/'; }

head_ "setup — forge on $A exporting to $B"
new_bare_repo "$WORK/A.git"
forge_up A "$WORK/A.git" "$A" \
  "FLINT_FORGE_EXPORT_REF=refs/heads/main" \
  "FLINT_FORGE_EXPORT_PREFIX=$B" \
  "FLINT_FORGE_EXPORT_EVERY_SECS=0" \
  "FLINT_FORGE_EXPORT_TIMEOUT_SECS=$TMO"
wait_key "$A/git/epoch" 30 && ok "forge holds its lease on $A" || bad "no lease on $A"

new_clone "$WORK/A.git" "$WORK/wc"
printf 'one\n' > "$WORK/wc/README.md"
git_c "$WORK/wc" add README.md; git_c "$WORK/wc" commit -qm one
push "$WORK/wc" HEAD:refs/heads/main >/dev/null 2>&1
wait_key "$B/.flint/lean/current" 40 && ok "the export published a lean workspace at $B" \
  || bad "the export never published"

# ── baseline: the two things whose ABSENCE is the finding ────────────
head_ "baseline — both observables are live before the violation"
r1=$(renewed); sleep 6; r2=$(renewed)
if [ -n "$r1" ] && [ -n "$r2" ] && [ "$r2" -gt "$r1" ]; then
  ok "forge's lease is being renewed ($r1 -> $r2)"
else
  bad "the lease was not advancing even before the violation ($r1 -> $r2)"
fi
printf 'two\n' >> "$WORK/wc/README.md"
git_c "$WORK/wc" commit -qam two
t0=$(date +%s)
timeout 30 bash -c "cd '$WORK/wc' && REMOTE_USER=driller FLINT_FORGE_SOCKET=/tmp/fc-A.sock git push -q origin HEAD:refs/heads/main" >/dev/null 2>&1
prc=$?; t1=$(date +%s)
[ $prc -eq 0 ] && ok "a push is acked in $((t1-t0))s before the violation" \
               || bad "pushes were already failing before the violation (rc=$prc)"
sleep 6

# ── the violation: a live lean sidecar takes B's lease ───────────────
head_ "the violation — a read-write lean sidecar on the export prefix $B"
mkdir -p "$WORK/wb"
( lean run "$B" "$WORK/wb" FLINT_SYNC_FLOOR_SECS=5 > "$WORK/leanB.log" 2>&1 ) &
LEANB=$!
sleep 12
if grep -q "holding epoch" "$WORK/leanB.log"; then
  ok "the lean sidecar took the export prefix's lease"
else
  bad "the lean sidecar never took B's lease — the violation did not happen"
  head -5 "$WORK/leanB.log"
fi

# Now drive one more push. The push itself should still be acked (the
# export runs after the report), but the export that follows it will
# try to claim a lease it cannot have.
printf 'three\n' >> "$WORK/wc/README.md"
git_c "$WORK/wc" commit -qam three
timeout 40 bash -c "cd '$WORK/wc' && REMOTE_USER=driller FLINT_FORGE_SOCKET=/tmp/fc-A.sock git push -q origin HEAD:refs/heads/main" >/dev/null 2>&1
[ $? -eq 0 ] && note "the push that triggers the blocked export was itself acked" \
             || note "the push that triggers the blocked export was NOT acked"

head_ "the blast radius — bounded by the timeout, not unbounded"
note "waiting out the ${TMO}s export timeout"
sleep $((TMO + 6))

# Observable 1 — the BACKOFF, measured inside its own window. This has
# to come first: the hold-off after one failure is one timeout long, so
# a drill that spends that window on other checks would find it expired
# and call a working backoff broken. (It did, once. The bug it found
# was real but different: the hold-off was being stamped with a clock
# read BEFORE the barrier ran, so the timeout consumed all of it.)
printf 'four\n' >> "$WORK/wc/README.md"
git_c "$WORK/wc" commit -qam four
timeout 60 bash -c "cd '$WORK/wc' && REMOTE_USER=driller FLINT_FORGE_SOCKET=/tmp/fc-A.sock git push -q origin HEAD:refs/heads/main" >/dev/null 2>&1
printf 'five\n' >> "$WORK/wc/README.md"
git_c "$WORK/wc" commit -qam five
t0=$(date +%s)
timeout 60 bash -c "cd '$WORK/wc' && REMOTE_USER=driller FLINT_FORGE_SOCKET=/tmp/fc-A.sock git push -q origin HEAD:refs/heads/main" >/dev/null 2>&1
prc=$?; t1=$(date +%s)
if [ $prc -eq 0 ] && [ $((t1-t0)) -lt $TMO ]; then
  ok "a push behind a held-off export does not pay the timeout again ($((t1-t0))s < ${TMO}s)"
else
  bad "the export was re-entered immediately (rc=$prc, $((t1-t0))s) — the outage is merely paced"
fi
forge_log A | grep -q "holding off" \
  && ok "the export says it is holding off" \
  || bad "nothing in the log says the export is holding off"

# Observable 2 — is forge renewing its lease on A again?
r3=$(renewed); sleep 12; r4=$(renewed)
if [ -n "$r3" ] && [ -n "$r4" ] && [ "$r4" -gt "$r3" ]; then
  ok "forge is renewing its lease on $A again ($r3 -> $r4)"
else
  bad "forge is NOT renewing its lease on $A ($r3 -> $r4) — a second writer on B froze A"
fi

# Observable 3 — does the repository still take a push?
printf 'six\n' >> "$WORK/wc/README.md"
git_c "$WORK/wc" commit -qam six
t0=$(date +%s)
timeout 60 bash -c "cd '$WORK/wc' && REMOTE_USER=driller FLINT_FORGE_SOCKET=/tmp/fc-A.sock git push -q origin HEAD:refs/heads/main" >/dev/null 2>&1
prc=$?; t1=$(date +%s)
[ $prc -eq 0 ] && ok "the repository still accepts pushes ($((t1-t0))s)" \
               || bad "the repository no longer accepts pushes (rc=$prc after $((t1-t0))s)"

# Observable 4 — the operator is told why, and sent to the right object.
if forge_log A | grep -q "export blocked"; then
  ok "forge logged the blocked export"
  forge_log A | grep -o "check who holds [^ ]*" | head -1 | sed 's/^/  ....  /'
else
  bad "the blocked export was silent — nothing in forge's log names the cause"
fi

# Observable 5 — forge must not have been deposed while blocked.
if forge_log A | grep -qi "fenced\|deposed\|superseded"; then
  bad "forge was deposed while the export was blocked"
else
  ok "forge was not deposed"
fi

kill $LEANB 2>/dev/null; wait $LEANB 2>/dev/null
forge_down A
verdict "C2"
