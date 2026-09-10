#!/usr/bin/env bash
# C8 — TWO FORGE SERVERS ON ONE PREFIX, both taking client traffic.
#
# THE CLAIM UNDER TEST. Forge's central safety claim is "one prefix has
# exactly one writer". The operator does not merely hope for it — the
# Deployment is `Recreate` with replicas in {0,1} precisely so two
# servers for one repository never coexist (`forge_operator/render.rs`
# :815). So the state this drill builds is one the SHIPPED topology
# refuses to create, and it is reachable anyway by exactly one route:
# something OUTSIDE that operator points a second forge at the same
# bucket and prefix — a second FlintRepo, a second cluster, a hand-run
# syncer, a restored backup brought up beside the original.
#
# WHY IT NEEDS A DRILL RATHER THAN A READING. F16 proved the lease on
# the wire and it proved the right things: a live holder is not
# superseded, a quiet one is, a deposed one cannot write. But every one
# of its pushes goes at the HOLDER. Nothing has ever measured what a
# client gets when its push lands on the OTHER one, and in a cluster
# that is not an exotic case — two pods behind one Service is a coin
# flip per connection.
#
# The failure that would matter is not a refusal. It is an ACCEPTANCE:
# the standby's syncer parks in `claimingEpoch` before `uds::serve` is
# spawned (`server.rs:303`), so its socket does not exist — but the
# bare repository beside it is a real git repository, and a push that
# reaches plain git succeeds locally and is never seen by forge or by
# the bucket. The client would be told everything is fine about a
# commit that exists nowhere but a standby's disk cache. What stands
# between those two outcomes is `receive.procReceiveRefs = refs/` and
# `core.hooksPath`, which `init_bare` sets — and on a standby that runs
# only from the PREWARM pass (`follow.rs:263`), never from the claim.
# So the safety of the standby's door rests on a warm-up whose stated
# purpose is speed. D7 turns prewarm off and asks the question again.
#
# WHAT IS ASSERTED
#
#   D0  a race for a fresh cell resolves to exactly one writer
#   D1  a push at the standby is REFUSED, and the holder's still lands
#   D2  under concurrent traffic at both doors, acked == in the bucket
#   D4  the holder freezes: every ACKED push survives the takeover
#   D5  the frozen holder returns and cannot write a thing
#   D6  the bucket, read back cold, holds the acked set and nothing more
#   D7  a standby prepares its repository before it claims, and refuses
#       by POSTURE — naming the prefix, not a missing file
#
# ANTI-VACUITY, and it is most of this file.
#
#   * D1's refusal means nothing unless the SAME push shape lands on
#     the holder, so the control runs first and D1 is INCONCLUSIVE
#     without it (F16 P4's rule).
#   * D2's "the standby acked nothing" is what a rig too blunt to push
#     at two servers at all would also report. D3 is the control: the
#     same generator, the same two servers, on TWO prefixes, where both
#     genuinely hold — both must ack everything. Only then is D2's
#     asymmetry a property of the lease.
#   * "The standby did not write" is worth nothing unless it TRIED, so
#     every refused push is checked to have reached git at all, and the
#     zombie leg pushes a uniquely marked commit rather than watching an
#     idle process.
#   * A standby that CRASHED passes D1, D2 and D5 for free. Both
#     processes are checked alive, polling, and printing their quiet
#     count at every leg that depends on the standby being awake.
#   * The final word on what the bucket holds is not either server's
#     opinion of itself: D6 brings both down and restores into an EMPTY
#     repository from S3 alone. That ref set must equal the acked set
#     exactly — no push lost, no push invented.
#
#   bash forge/e2e/composition/c8-two-forges.sh
#
# Knobs: ROUNDS (default 6), MINIO_PORT, BUCKET, WORK, KEEP.
set -uo pipefail
cd "$(dirname "$0")/../../.."
export WORK=${WORK:-/tmp/fc-c8}
rm -rf "$WORK"; mkdir -p "$WORK"
source forge/e2e/composition/rig.sh
trap rig_clean EXIT
rig_init || { echo "rig_init failed"; exit 1; }
binary_is_fresh || exit 1
rig_purge c8/
rig_gate

P=c8/one
PA=c8/two-a
PB=c8/two-b
ROUNDS=${ROUNDS:-6}
PORT_A=9861; PORT_B=9862; PORT_C=9863
ACKED=""          # refs the client was told landed
REFUSED=""        # refs the client was told did not

# ── observation ──────────────────────────────────────────────────────
# Two independent readings of every claim about who the writer is: the
# process's own /status and the bucket's epoch cell. A server's opinion
# of whether it is the writer is exactly the sensor that lies in a
# split brain, so it is never the only witness here.
st()  { curl -sS --max-time 3 "http://127.0.0.1:$1/status" 2>/dev/null; }
sraw() { st "$1" | jq -r "$2" 2>/dev/null; }
sid()  { sraw "$1" '.serverId // empty'; }
cell() { s3_cat "$1/git/epoch" 2>/dev/null | jq -r "$2" 2>/dev/null; }
alive() { local p="$WORK/forge-$1.pid"; [ -f "$p" ] && kill -0 "$(cat "$p")" 2>/dev/null; }
quiet_line() { forge_log "$1" | grep -o '[0-9]\+/6 quiet polls' | tail -1; }

# Is this process the writer? NOT `phase == serving`: the phases move
# through `pushing` and `sweeping` while perfectly healthy, so an exact
# match samples a race. The pair the code's own `serving()` rests on is
# the lease being held and no fence.
is_writer() { [ "$(sraw "$1" '.epoch.held')" = true ] && [ "$(sraw "$1" '.fenced')" = null ]; }

wait_writer() {  # wait_writer <port> <secs>
  local n=${2:-60}
  for _ in $(seq 1 "$n"); do is_writer "$1" && return 0; sleep 1; done
  return 1
}

# Holding the lease is NOT yet accepting pushes: `uds::serve` is spawned
# after the restore (`server.rs:303`), so a push aimed at a server that
# has claimed but not finished restoring is refused for a reason that
# has nothing to do with this drill. `rpoClean` is the code's own
# conjunction of held + cell loaded + serving.
wait_serving() {  # wait_serving <port> <secs>
  local n=${2:-90}
  for _ in $(seq 1 "$n"); do
    [ "$(sraw "$1" '.rpoClean')" = true ] && return 0
    sleep 1
  done
  return 1
}

# ── the client ───────────────────────────────────────────────────────
# One clone per server, because each server has its OWN bare repository
# — that is what a second forge on one prefix IS. A push names the
# socket of the server it is aimed at, which is the local stand-in for
# which pod the Service picked.
mk_commit() {  # mk_commit <clone> <text>
  printf '%s\n' "$2" > "$1/f.txt"
  git -C "$1" add f.txt >/dev/null 2>&1
  git -C "$1" -c user.name=driller -c user.email=driller@invalid \
      commit -qm "$2" >/dev/null 2>&1
}

push_at() {  # push_at <tag> <clone> <ref>  -> rc, output in $PUSH_OUT
  PUSH_OUT=$(FORGE_SOCKET="/tmp/fc-$1.sock" push "$2" "HEAD:$3" 2>&1)
  PUSH_RC=$?
  return $PUSH_RC
}

# The two pushes must be in flight at the same moment, and their PIDs
# must be waited on BY NAME. A bare `wait` here waits for every job of
# this shell — and `forge_up` backgrounds the servers themselves, which
# never exit. The first shape of this drill hung there for ten minutes
# after round 1 with nothing running.
push_bg() {  # push_bg <tag> <clone> <ref> <stem>  -> pid in $BG_PID
  ( FORGE_SOCKET="/tmp/fc-$1.sock" push "$2" "HEAD:$3" > "$4.out" 2>&1; echo $? > "$4.rc" ) &
  BG_PID=$!
}

record() {  # record <rc> <ref>
  if [ "$1" -eq 0 ]; then ACKED="$ACKED $2"; else REFUSED="$REFUSED $2"; fi
}

# ── D0: a race for a fresh cell ──────────────────────────────────────
head_ "D0 — two servers race for one fresh prefix; exactly one may win"
new_bare_repo "$WORK/a.git"; new_bare_repo "$WORK/b.git"
forge_up A "$WORK/a.git" "$P" FLINT_FORGE_STATUS_ADDR=127.0.0.1:$PORT_A
forge_up B "$WORK/b.git" "$P" FLINT_FORGE_STATUS_ADDR=127.0.0.1:$PORT_B
wait_key "$P/git/epoch" 40 && ok "a lease cell appeared under $P" \
  || { bad "no lease cell after 40s — nothing claimed"; verdict "C8"; exit 2; }
sleep 12

alive A && alive B && ok "both servers are still running" \
  || { inconc "a server died; nothing below can distinguish a lease from a corpse"
       forge_log A | tail -5; forge_log B | tail -5; verdict "C8"; exit 2; }

IDA=$(sid $PORT_A); IDB=$(sid $PORT_B); HOLDER=$(cell "$P" .holder_id)
[ -n "$IDA" ] && [ -n "$IDB" ] && [ "$IDA" != "$IDB" ] \
  && ok "the two servers are distinct incarnations ($IDA / $IDB)" \
  || inconc "the two servers do not report distinct ids (A=$IDA B=$IDB)"

if   [ "$HOLDER" = "$IDA" ]; then W=A; S=B; WP=$PORT_A; SP=$PORT_B
elif [ "$HOLDER" = "$IDB" ]; then W=B; S=A; WP=$PORT_B; SP=$PORT_A
else bad "the cell names '$HOLDER', which is neither server"; verdict "C8"; exit 2; fi
ok "the bucket names exactly one of them: $W ($HOLDER)"

is_writer $WP && ok "and $W's own /status agrees it holds the lease" \
  || bad "$W holds the cell but does not think so — the two observations disagree"
is_writer $SP && bad "$S ALSO claims the lease — SPLIT BRAIN" \
  || ok "$S does not claim the lease"
[ "$(sraw $SP .phase)" = claimingEpoch ] \
  && ok "and $S is parked in claimingEpoch, not serving" \
  || note "$S's phase is $(sraw $SP .phase)"

# The strongest control there is for "the standby is held off": it is
# the mechanism itself. The line is printed every heartbeat, and the
# count staying at 0 proves the holder's token is moving under it.
Q=$(quiet_line $S)
if [ -n "$Q" ]; then
  ok "$S is polling the cell and reports $Q"
  case "$Q" in 0/6*) ok "and the count is ZERO — $W's token is moving under it" ;;
               *)    bad "the quiet count reached $Q while $W was renewing" ;; esac
else
  inconc "$S printed no quiet-poll line; it may not be observing the cell at all"
fi

CW="$WORK/clone-$W"; CS="$WORK/clone-$S"
new_clone "$WORK/$(echo $W | tr 'AB' 'ab').git" "$CW"
new_clone "$WORK/$(echo $S | tr 'AB' 'ab').git" "$CS"

# ── D1: the standby's door ───────────────────────────────────────────
head_ "D1 — a push at the standby must be refused; the holder's must land"
wait_serving $WP 90 || inconc "$W never reached rpoClean; a push at it is not yet a control"
# THE CONTROL RUNS FIRST. A refusal at the standby is evidence of
# fencing only if this very shape of push lands when it is aimed at a
# server that holds the lease.
mk_commit "$CW" "d1-holder"
push_at $W "$CW" refs/heads/d1-holder
D1_ARMED=no
if [ "$PUSH_RC" -eq 0 ]; then
  ok "a push at $W (the holder) LANDS — the standby's refusal will mean something"
  record 0 refs/heads/d1-holder; D1_ARMED=yes
else
  inconc "the holder refused the control push: $(printf '%s' "$PUSH_OUT" | tr '\n' ' ' | cut -c1-160)"
fi

mk_commit "$CS" "d1-standby"
push_at $S "$CS" refs/heads/d1-standby
D1_RC=$PUSH_RC; D1_OUT=$PUSH_OUT
note "the client at the standby said: $(printf '%s' "$D1_OUT" | tr '\n' ' ' | cut -c1-200)"
if [ "$D1_ARMED" = no ]; then
  inconc "D1 is not armed — the control push never landed"
elif [ "$D1_RC" -ne 0 ]; then
  ok "the standby REFUSED the push (rc=$D1_RC)"
  record 1 refs/heads/d1-standby
  # A refusal is not enough: the FIRST shape of this drill measured one
  # that reached the client as `No such file or directory (os error 2)`
  # — true, and it names nothing an operator can act on. The door
  # answers by posture now, so the message is part of the contract.
  case "$D1_OUT" in
    *standby*) ok "and the refusal names the POSTURE ('standby'), not a missing file" ;;
    *)         bad "the refusal does not name the posture: $(printf '%s' "$D1_OUT" | tr '\n' ' ' | cut -c1-140)" ;;
  esac
  case "$D1_OUT" in
    *"$P"*) ok "and it names the prefix ($P), so an operator knows which repository" ;;
    *)      bad "the refusal does not name the prefix $P" ;;
  esac
  case "$D1_OUT" in
    *"No such file"*) bad "the client is still being told about a missing file" ;;
    *)                ok "and it no longer mentions a missing file" ;;
  esac
else
  bad "the standby ACCEPTED a push it cannot publish — the client believes a commit is safe"
  record 0 refs/heads/d1-standby
fi
# Did it reach git at all? A push refused by the transport before the
# hook ran would pass the line above without testing forge.
git -C "$WORK/$(echo $S | tr 'AB' 'ab').git" rev-parse -q --verify refs/heads/d1-standby >/dev/null 2>&1 \
  && bad "the standby's own repository moved the ref — plain git accepted it behind forge's back" \
  || ok "and the standby's repository did not move the ref either"

# ── D2: concurrent traffic at both doors ─────────────────────────────
head_ "D2 — $ROUNDS rounds of simultaneous pushes at BOTH doors"
for i in $(seq 1 "$ROUNDS"); do
  mk_commit "$CW" "d2-w-$i"; mk_commit "$CS" "d2-s-$i"
  push_bg $W "$CW" "refs/heads/d2-w-$i" "$WORK/r$i-w"; p1=$BG_PID
  push_bg $S "$CS" "refs/heads/d2-s-$i" "$WORK/r$i-s"; p2=$BG_PID
  wait $p1 $p2
done
WOK=0; SOK=0
for i in $(seq 1 "$ROUNDS"); do
  rw=$(cat "$WORK/r$i-w.rc" 2>/dev/null || echo 99)
  rs=$(cat "$WORK/r$i-s.rc" 2>/dev/null || echo 99)
  [ "$rw" -eq 0 ] && WOK=$((WOK+1)); record "$rw" "refs/heads/d2-w-$i"
  [ "$rs" -eq 0 ] && SOK=$((SOK+1)); record "$rs" "refs/heads/d2-s-$i"
done
[ "$WOK" -eq "$ROUNDS" ] \
  && ok "the holder acked all $ROUNDS of its pushes with a challenger present" \
  || bad "the holder acked only $WOK/$ROUNDS — a standby's presence cost it pushes"
[ "$SOK" -eq 0 ] \
  && ok "the standby acked NONE of its $ROUNDS pushes" \
  || bad "the standby acked $SOK/$ROUNDS pushes it cannot publish"
alive A && alive B && ok "both servers are still running after the concurrent rounds" \
  || inconc "a server died during D2; its refusals prove nothing"

# ── D3: the control — two writers that genuinely both write ──────────
head_ "D3 — CONTROL: the same generator on TWO prefixes must ack twice"
# Without this leg, "the standby acked nothing" is indistinguishable
# from a rig that cannot drive two servers at once. Here both servers
# hold their own cell, so both MUST ack — and if they do not, D2's
# asymmetry is the harness, not the lease.
forge_down A; forge_down B
rig_purge c8/two-
rm -rf "$WORK/ca.git" "$WORK/cb.git" "$WORK/clone-ca" "$WORK/clone-cb"
new_bare_repo "$WORK/ca.git"; new_bare_repo "$WORK/cb.git"
forge_up A "$WORK/ca.git" "$PA" FLINT_FORGE_STATUS_ADDR=127.0.0.1:$PORT_A
forge_up B "$WORK/cb.git" "$PB" FLINT_FORGE_STATUS_ADDR=127.0.0.1:$PORT_B
wait_serving $PORT_A 90 && wait_serving $PORT_B 90 \
  && ok "on separate prefixes both servers hold a lease and serve" \
  || inconc "one of them never claimed its own prefix — the control cannot run"
new_clone "$WORK/ca.git" "$WORK/clone-ca"; new_clone "$WORK/cb.git" "$WORK/clone-cb"
mk_commit "$WORK/clone-ca" "d3-a"; mk_commit "$WORK/clone-cb" "d3-b"
push_bg A "$WORK/clone-ca" refs/heads/d3-a "$WORK/d3-a"; p1=$BG_PID
push_bg B "$WORK/clone-cb" refs/heads/d3-b "$WORK/d3-b"; p2=$BG_PID
wait $p1 $p2
ra=$(cat "$WORK/d3-a.rc" 2>/dev/null || echo 99); rb=$(cat "$WORK/d3-b.rc" 2>/dev/null || echo 99)
if [ "$ra" -eq 0 ] && [ "$rb" -eq 0 ]; then
  ok "both acked — this rig CAN observe two servers writing at once"
else
  inconc "the control could not get two simultaneous pushes acked (a=$ra b=$rb) — D2 proves nothing"
  note "a: $(head -3 "$WORK/d3-a.out" 2>/dev/null | tr '\n' ' ')"
  note "b: $(head -3 "$WORK/d3-b.out" 2>/dev/null | tr '\n' ' ')"
fi
forge_down A; forge_down B

# ── D4: the holder freezes ───────────────────────────────────────────
head_ "D4 — the holder freezes; the standby supersedes; acked pushes survive"
forge_up A "$WORK/a.git" "$P" FLINT_FORGE_STATUS_ADDR=127.0.0.1:$PORT_A
forge_up B "$WORK/b.git" "$P" FLINT_FORGE_STATUS_ADDR=127.0.0.1:$PORT_B
sleep 14
HOLDER=$(cell "$P" .holder_id)
IDA=$(sid $PORT_A); IDB=$(sid $PORT_B)
if   [ "$HOLDER" = "$IDA" ]; then W=A; S=B; WP=$PORT_A; SP=$PORT_B
elif [ "$HOLDER" = "$IDB" ]; then W=B; S=A; WP=$PORT_B; SP=$PORT_A
else inconc "no identifiable holder after the restart"; W=A; S=B; WP=$PORT_A; SP=$PORT_B; fi
note "after the restart the holder is $W"
E_BEFORE=$(cell "$P" .epoch)
wait_serving $WP 90 || inconc "$W never reached rpoClean after the restart"
CW="$WORK/clone2-$W"; new_clone "$WORK/$(echo $W | tr 'AB' 'ab').git" "$CW"
mk_commit "$CW" "d4-acked"
push_at $W "$CW" refs/heads/d4-acked
if [ "$PUSH_RC" -eq 0 ]; then
  record 0 refs/heads/d4-acked
  ok "a push at $W is acknowledged just before the freeze"
else
  record 1 refs/heads/d4-acked
  inconc "the pre-freeze push was refused; D4 has nothing acked to protect"
fi

# SIGSTOP, not SIGKILL. The process must still exist for D5 — a
# partition that heals is the shape that produces a zombie, and a dead
# process cannot try to write.
WPID=$(cat "$WORK/forge-$W.pid"); kill -STOP "$WPID" 2>/dev/null \
  && ok "$W is frozen (SIGSTOP) — its token stops moving, the process does not die" \
  || { inconc "could not freeze $W"; }
TOOK=no
for _ in $(seq 1 60); do
  if [ "$(cell "$P" .holder_id)" != "$HOLDER" ]; then TOOK=yes; break; fi
  sleep 1
done
E_AFTER=$(cell "$P" .epoch)
if [ "$TOOK" = yes ]; then
  ok "$S superseded the frozen holder (epoch $E_BEFORE -> $E_AFTER)"
  [ "${E_AFTER:-0}" -gt "${E_BEFORE:-0}" ] && ok "and the epoch advanced" \
    || bad "the holder changed without the epoch advancing"
  wait_writer $SP 60 && ok "and $S now reports itself the writer" \
    || bad "$S took the cell but does not report holding it"
else
  bad "the standby never superseded a holder frozen for 60s"
fi

# ── D5: the zombie returns ───────────────────────────────────────────
head_ "D5 — the frozen holder wakes with the lease gone and must write nothing"
kill -CONT "$WPID" 2>/dev/null && ok "$W is running again (SIGCONT)" \
  || inconc "could not resume $W"
sleep 8
# MODEL THE OBSERVATION, NOT ONLY THE STATE. A deposed syncer does not
# sit there wearing a `fenced` flag: the renewer's fence IS the serving
# loop's exit (`server.rs:318`), main prints the error and exits 1
# (`flint_forge_syncer.rs:385`), and the pod restarts into a restore
# from the snapshot that deposed it. So the two admissible observations
# are a live process reporting a fence and a process that is GONE — and
# the first shape of this leg failed a correct server for the second.
FENCED=$(sraw $WP '.fenced')
if [ -n "$FENCED" ] && [ "$FENCED" != null ]; then
  ok "$W is alive and reports its fence: $(printf '%s' "$FENCED" | cut -c1-90)"
elif ! alive $W; then
  ok "$W exited rather than serve on — the fence IS the loop's exit"
else
  bad "$W is alive, unfenced and not serving (fenced=$FENCED) — neither admissible state"
fi
# A process that vanished for an unrelated reason would pass the line
# above. What makes it a DEPOSAL is that it said so.
forge_log $W | grep -qi "deposed" \
  && ok "and it named the deposal: $(forge_log $W | grep -i deposed | tail -1 | cut -c1-110)" \
  || bad "$W stopped without ever naming a deposal — the cause is not established"
is_writer $WP && bad "$W still claims to be the writer — SPLIT BRAIN" \
  || ok "and it does not claim the lease"

# The zombie must TRY. A leg that watched an idle process not write
# would pass against a server with no fencing at all.
mk_commit "$CW" "d5-zombie"
push_at $W "$CW" refs/heads/d5-zombie
note "the client at the zombie said: $(printf '%s' "$PUSH_OUT" | tr '\n' ' ' | cut -c1-200)"
if [ "$PUSH_RC" -ne 0 ]; then
  ok "the deposed server REFUSED the push (rc=$PUSH_RC)"; record 1 refs/heads/d5-zombie
else
  bad "the deposed server ACCEPTED a push"; record 0 refs/heads/d5-zombie
fi
[ "$(cell "$P" .holder_id)" != "$HOLDER" ] \
  && ok "and the cell still names the successor, not the zombie" \
  || bad "the zombie took the cell back"

# ── D6: the bucket is the oracle ─────────────────────────────────────
head_ "D6 — a third server restores from S3 alone; acked == present, exactly"
forge_down A; forge_down B
rm -rf "$WORK/c.git"; new_bare_repo "$WORK/c.git"
forge_up C "$WORK/c.git" "$P" FLINT_FORGE_STATUS_ADDR=127.0.0.1:$PORT_C
wait_serving $PORT_C 150 && ok "a fresh server claimed the prefix and restored" \
  || { inconc "the third server never came up; the bucket cannot be read back"
       forge_log C | tail -8; }
sleep 3
GOT=$(git -C "$WORK/c.git" for-each-ref --format='%(refname)' 2>/dev/null | sort)
note "the bucket holds: $(printf '%s' "$GOT" | tr '\n' ' ')"
MISSING=""; PHANTOM=""
for r in $ACKED;   do printf '%s\n' "$GOT" | grep -qx "$r" || MISSING="$MISSING $r"; done
for r in $REFUSED; do printf '%s\n' "$GOT" | grep -qx "$r" && PHANTOM="$PHANTOM $r"; done
[ -z "$MISSING" ] \
  && ok "every ref the client was told landed is in the bucket ($(echo $ACKED | wc -w | tr -d ' ') of them)" \
  || bad "ACKED BUT LOST:$MISSING"
[ -z "$PHANTOM" ] \
  && ok "and no ref the client was told was refused is in the bucket ($(echo $REFUSED | wc -w | tr -d ' ') of them)" \
  || bad "REFUSED BUT PRESENT:$PHANTOM"
forge_down C

# ── D7: the repository exists before the claim ───────────────────────
head_ "D7 — a standby prepares its repository BEFORE it claims (PREWARM=0)"
# `receive.procReceiveRefs` and `core.hooksPath` — the two settings that
# route a push through forge instead of through plain git — are written
# by `init_bare`. It used to run only from the restore and from the
# PREWARM pass, so a standby with prewarm off sat beside a repository
# forge had never configured, and plain git accepted pushes forge never
# saw. `server.rs` calls it before the claim now, and this leg is the
# reason to believe that: prewarm is OFF in both arms below.
rig_purge c8/three
rm -rf "$WORK/pa.git" "$WORK/pb.git" "$WORK/pc.git" "$WORK/clone-pa"
new_bare_repo "$WORK/pa.git"
forge_up A "$WORK/pa.git" c8/three FLINT_FORGE_STATUS_ADDR=127.0.0.1:$PORT_A FLINT_FORGE_PREWARM=0
wait_serving $PORT_A 90 && ok "the holder is up on c8/three" || inconc "no holder for D7"
new_clone "$WORK/pa.git" "$WORK/clone-pa"
mk_commit "$WORK/clone-pa" "d7-seed"
push_at A "$WORK/clone-pa" refs/heads/d7-seed
[ "$PUSH_RC" -eq 0 ] && ok "and it accepts a push — D7's refusals will mean something" \
  || inconc "the D7 holder refused the control push; the leg is not armed"

# The standby, with PREWARM=0 and an EMPTY directory where its
# repository would be — exactly what a second pod starts with, since
# the pod's repository is an emptyDir (`forge_operator/render.rs:7`).
#
# The hooks are staged first and the repository is NOT. In the pod they
# live in the image (`FLINT_FORGE_HOOKS_PATH=/usr/local/share/flint-forge
# /hooks`, `render.rs:50`), so they are present whoever created the
# repository. Without this the leg measured a missing hook binary — git
# refuses with `cannot find hook 'proc-receive'`, which is safe but
# names nothing, and is a state the shipped topology cannot reach.
mkdir -p "$WORK/pc.git/hooks-flint"
ln -sf "$HOOK_BIN" "$WORK/pc.git/hooks-flint/proc-receive"
ln -sf "$HOOK_BIN" "$WORK/pc.git/hooks-flint/pre-receive"
forge_up B "$WORK/pc.git" c8/three FLINT_FORGE_STATUS_ADDR=127.0.0.1:$PORT_B FLINT_FORGE_PREWARM=0
sleep 10
if alive B && [ "$(sraw $PORT_B .phase)" = claimingEpoch ]; then
  ok "the no-prewarm standby is parked in claimingEpoch"
else
  inconc "the no-prewarm standby is $(sraw $PORT_B .phase) — D7 cannot ask its question"
fi
# The direct assertion of the fix, before any push: the repository is
# there and it carries forge's posture.
git -C "$WORK/pc.git" rev-parse --git-dir >/dev/null 2>&1 \
  && ok "the parked standby CREATED its repository without claiming" \
  || bad "no repository beside the parked standby — init_bare did not run before the claim"
[ "$(git -C "$WORK/pc.git" config --get receive.procReceiveRefs 2>/dev/null)" = "refs/" ] \
  && ok "and it carries receive.procReceiveRefs=refs/ — every push routes through forge" \
  || bad "the standby's repository has no procReceiveRefs — plain git would decide its pushes"
D7OUT=$(FLINT_FORGE_SOCKET=/tmp/fc-B.sock REMOTE_USER=driller \
        git -C "$WORK/clone-pa" push "$WORK/pc.git" HEAD:refs/heads/d7 2>&1); D7RC=$?
note "the client said: $(printf '%s' "$D7OUT" | tr '\n' ' ' | cut -c1-180)"
[ "$D7RC" -ne 0 ] \
  && ok "a push at the no-prewarm standby is REFUSED (rc=$D7RC)" \
  || bad "the no-prewarm standby ACCEPTED a push that reaches no bucket"
case "$D7OUT" in
  *standby*) ok "and the refusal names the posture, with prewarm off" ;;
  *)         bad "with prewarm off the refusal does not name the posture" ;;
esac

# ── D7a: a repository forge did not create ───────────────────────────
head_ "D7a — a bare repository that already existed, which forge must still claim"
# The shape that found the defect: a bare repository sitting beside a
# parked syncer that forge never configured — a restored cache volume, a
# hand-run `git init --bare`, a repo directory left by something else.
# Before the fix, plain git ACCEPTED the push here and the client was
# told its commit had landed when nothing reached the bucket.
new_bare_repo "$WORK/pb.git"
git -C "$WORK/pb.git" config --unset receive.procReceiveRefs 2>/dev/null
[ -z "$(git -C "$WORK/pb.git" config --get receive.procReceiveRefs 2>/dev/null)" ] \
  && ok "precondition: the repository starts with NO procReceiveRefs" \
  || inconc "the fixture is already configured; D7a cannot pose its question"
forge_down B
forge_up B "$WORK/pb.git" c8/three FLINT_FORGE_STATUS_ADDR=127.0.0.1:$PORT_B FLINT_FORGE_PREWARM=0
sleep 10
alive B && [ "$(sraw $PORT_B .phase)" = claimingEpoch ] \
  && ok "the standby is parked beside a repository it inherited" \
  || inconc "the standby is $(sraw $PORT_B .phase) — D7a cannot ask its question"
[ "$(git -C "$WORK/pb.git" config --get receive.procReceiveRefs 2>/dev/null)" = "refs/" ] \
  && ok "and it has taken the repository over: procReceiveRefs=refs/ is set" \
  || bad "forge left an inherited repository unconfigured — plain git still owns its pushes"
D7AOUT=$(FLINT_FORGE_SOCKET=/tmp/fc-B.sock REMOTE_USER=driller \
         git -C "$WORK/clone-pa" push "$WORK/pb.git" HEAD:refs/heads/d7a 2>&1); D7ARC=$?
note "the client said: $(printf '%s' "$D7AOUT" | tr '\n' ' ' | cut -c1-180)"
[ "$D7ARC" -ne 0 ] \
  && ok "the push is REFUSED (rc=$D7ARC) — plain git no longer decides it" \
  || bad "plain git ACCEPTED the push: the client is told a commit landed that reached no bucket"
git -C "$WORK/pb.git" rev-parse -q --verify refs/heads/d7a >/dev/null 2>&1 \
  && bad "and the ref moved on that disk — the commit exists there and nowhere else" \
  || ok "and no ref moved on that disk"
forge_down A; forge_down B

verdict "C8"
