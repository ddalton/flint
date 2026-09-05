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
# COST". Three facts about the shipped code compose:
#
#   1. `flint-sync`'s claim loop never gives up: `Waiting` sleeps 10s
#      and retries forever (bin/flint_sync.rs:290-317).
#   2. `export::run_barrier` awaits that subprocess with no timeout
#      (export.rs:254).
#   3. `export::maybe_run` is awaited INLINE in the serving loop
#      (server.rs:288), and the lease heartbeat is a timer on that same
#      `select!` (server.rs:6,158-199).
#
# If those compose as read, a second writer on B does not merely lose:
# it stops forge's heartbeat, and the casualty is the repository's
# lease on prefix A — a different prefix from the one misconfigured.
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
renewed() { s3_cat "$A/git/epoch" | sed 's/.*"renewed_unix":\([0-9]*\).*/\1/'; }

head_ "setup — forge on $A exporting to $B"
new_bare_repo "$WORK/A.git"
forge_up A "$WORK/A.git" "$A" \
  "FLINT_FORGE_EXPORT_REF=refs/heads/main" \
  "FLINT_FORGE_EXPORT_PREFIX=$B" \
  "FLINT_FORGE_EXPORT_EVERY_SECS=0"
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

head_ "the blast radius"
sleep 8
if grep -q "waiting on the standing lease" "$WORK/forge-A.log" 2>/dev/null || \
   forge_log A | grep -q "waiting on the standing lease"; then
  ok "forge's export is blocked waiting on the foreign holder"
else
  note "no wait line yet in forge's log"
fi

# Observable 1: does forge still renew the lease on A?
r3=$(renewed); sleep 20; r4=$(renewed)
if [ -n "$r3" ] && [ -n "$r4" ] && [ "$r4" -gt "$r3" ]; then
  ok "forge keeps renewing its lease on $A while the export is blocked"
else
  bad "forge STOPPED renewing its lease on $A ($r3 -> $r4) — a second writer on B froze A"
fi

# Observable 2: can the repository still take a push?
printf 'four\n' >> "$WORK/wc/README.md"
git_c "$WORK/wc" commit -qam four
t0=$(date +%s)
timeout 30 bash -c "cd '$WORK/wc' && REMOTE_USER=driller FLINT_FORGE_SOCKET=/tmp/fc-A.sock git push -q origin HEAD:refs/heads/main" >/dev/null 2>&1
prc=$?; t1=$(date +%s)
if [ $prc -eq 0 ]; then
  ok "the repository still accepts pushes ($((t1-t0))s)"
else
  bad "the repository no longer accepts pushes (rc=$prc after $((t1-t0))s) — misconfiguring B took down A"
fi

# Observable 3: is the wedge self-limiting?
if forge_log A | grep -qi "fenced\|deposed\|superseded"; then
  bad "forge was deposed while blocked on the export"
else
  note "forge was not deposed within this window"
fi

kill $LEANB 2>/dev/null; wait $LEANB 2>/dev/null
forge_down A
verdict "C2"
