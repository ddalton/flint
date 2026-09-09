#!/usr/bin/env bash
# THE GAPS THE FIRST RESIDUE DRILL LEFT.
#
# That drill pushed SEQUENTIALLY, from one agent, with wholly-refused
# pushes only, on a tiny repository, for twenty minutes. It showed the
# mechanism. It did not show the mechanism under the conditions that
# break mechanisms.
#
# THE ONE THAT MATTERS IS CONCURRENCY. Direction 5's push->pack handoff
# is keyed by the PARENT PID of `git-receive-pack`, and concurrent
# pushes are exactly where a key can cross: if one push reads another's
# record, an ACCEPTED push's pack is left unnamed and its objects are in
# no pack the snapshot names — a repository intact on disk and
# unrecoverable from S3. That is the F14 shape, and F14 was found with
# four agents pushing at once. Pack names are also MANY-TO-ONE, so two
# concurrent pushes of identical content legitimately share a name.
#
# THE SAFETY ORACLE IS THE SAME IN EVERY LEG: after the dust settles,
# a FRESH CLONE of the treated repository must pass `git fsck --strict`
# and every accepted ref must resolve. Bytes are the claim; integrity is
# the thing that must never be traded for it.
#
#   CTX=... TAG=... [S3_BUCKET=... KEYFILE=...] ./run-residue-hard.sh
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
CHART="$ROOT/flint-forge-chart"
CTX=${CTX:-$(kubectl config current-context 2>/dev/null)}
TAG=${TAG:?set TAG}
K="kubectl --context $CTX"
NS_SYS=forge-system
NS_AGENTS=agents
DOOR=${DOOR:-http://flint-forge-door.$NS_SYS.svc}
RUN=$(date +%s)
CONC=${CONC:-6}

PASS=0; FAILED=0
ok()  { PASS=$((PASS+1));     echo "  ok: $1"; }
bad() { FAILED=$((FAILED+1)); echo "  BAD: $1"; }
note(){ echo "  ..  $1"; }
leg() { echo; echo "── $1 — $2"; }
inpod() { $K -n "$NS_AGENTS" exec writer -c agent -- sh -c "$*" 2>&1; }
door_pre() {
  printf 'T=$(cat /var/run/secrets/forge/token); A="Authorization: Basic $(printf "x:%%s" "$T" | base64 -w0)"; G() { git -c http.extraHeader="$A" "$@"; }; U=%s/git/%s/%s.git' \
         "$DOOR" "$NS_AGENTS" "$1"
}
mcx() { $K -n "$NS_SYS" exec mc-s3 -- "$@" 2>/dev/null; }
snapshot_of() {
  if [ -n "${S3_BUCKET:-}" ]; then
    aws s3 cp "s3://$S3_BUCKET/residue/$1/git/snapshot" - --profile "${AWS_PROFILE_DRILL:-trove-admin}" 2>/dev/null
  else
    mcx mc cat "m/s3bucket/residue/$1/git/snapshot" 2>/dev/null
  fi
}
named_packs() {
  snapshot_of "$1" | python3 -c 'import sys,json
try: print("\n".join(json.load(sys.stdin).get("packs", [])))
except Exception: pass' 2>/dev/null
}
named_refs() {
  snapshot_of "$1" | python3 -c 'import sys,json
try:
    d=json.load(sys.stdin)
    print("\n".join(sorted(d.get("refs", {}).keys())))
except Exception: pass' 2>/dev/null
}
ondisk_packs() {
  local pod; pod=$($K -n "$NS_AGENTS" get pod -o name 2>/dev/null | grep "forge-$1" | head -1)
  [ -n "$pod" ] || return 0
  $K -n "$NS_AGENTS" exec "${pod#pod/}" -c git-http -- sh -c \
    "cd /repo/$NS_AGENTS/$1.git && ls objects/pack/*.pack 2>/dev/null | sed 's#.*/##'" 2>/dev/null | tr -d '\r'
}

# THE ORACLE. A fresh clone, `fsck --strict`, and every ref the snapshot
# claims must resolve in it. Run after every leg, on BOTH arms, because
# "the treated arm is intact" says nothing if the control is broken too.
integrity() { # integrity <repo> <label>
  local r=$1 label=$2 pre out
  pre=$(door_pre "$r")
  out=$(inpod "$pre; rm -rf /tmp/ck-$r && G clone -q \$U /tmp/ck-$r 2>&1 | tail -1; cd /tmp/ck-$r 2>/dev/null && git fsck --strict --no-progress >/dev/null 2>&1 && echo FSCK_OK")
  case "$out" in
    *FSCK_OK*) ;;
    *) bad "$label: $r does not clone+fsck — $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-140)"; return 1 ;;
  esac
  # Every ref the SNAPSHOT names must resolve in a clone built from what
  # the snapshot names. This is what an unnamed accepted pack breaks.
  local missing=0 ref
  for ref in $(named_refs "$r"); do
    case "$ref" in refs/heads/*) ;; *) continue ;; esac
    inpod "cd /tmp/ck-$r && git rev-parse --verify -q ${ref#refs/heads/} >/dev/null 2>&1 || git rev-parse --verify -q origin/${ref#refs/heads/} >/dev/null 2>&1" >/dev/null 2>&1 \
      || { missing=$((missing+1)); note "$label: $r is missing $ref in a fresh clone"; }
  done
  [ "$missing" = 0 ] || { bad "$label: $r lost $missing ref(s) — objects unnamed"; return 1; }
  ok "$label: $r clones, fsck --strict passes, and every named ref resolves"
}

seed() { # seed <repo>
  local r=$1 pre; pre=$(door_pre "$r")
  inpod "$pre; rm -rf /tmp/$r && G clone -q \$U /tmp/$r 2>&1|tail -1" >/dev/null
  inpod "cd /tmp/$r && git config user.email t@e.invalid && git config user.name t" >/dev/null
  # A corpus with some bulk, so packs are not all a few hundred bytes.
  inpod "cd /tmp/$r && i=0; while [ \$i -lt 20000 ]; do echo \"line \$i padding padding padding\"; i=\$((i+1)); done > big.txt && git add big.txt && git commit -qm base" >/dev/null
  inpod "$pre; cd /tmp/$r && G push -q \$U HEAD:refs/for/main 2>&1|tail -1" >/dev/null
  sleep 4
}

verdict() {
  echo
  echo "══ residue HARD drill: $PASS passed, $FAILED failed ══"
  [ "$FAILED" -gt 0 ] && { echo "   FAILED"; return 1; }
  echo "   GREEN"; return 0
}

main() {
  echo "residue HARD drill — context $CTX, images $TAG, concurrency $CONC"
  for r in ctl treated; do seed "$r"; done

  # ── H1 — a CONCURRENT burst, all accepted ───────────────────────────
  #
  # The dangerous direction-5 failure is not naming too much, it is
  # naming too LITTLE: an accepted push whose pack goes unnamed leaves
  # its objects in no pack the snapshot names, which is a repository
  # that is intact on disk and unrecoverable from the bucket. With every
  # push in the burst accepted, EVERY new pack must be named — so this
  # leg can state the requirement exactly rather than approximately.
  leg H1 "$CONC concurrent ACCEPTED pushes — every new pack must be named"
  local r pre before after newp namedp unnamed
  for r in ctl treated; do
    pre=$(door_pre "$r")
    before=$(ondisk_packs "$r")
    # Distinct branches, distinct content, launched together.
    # EVERY PUSH'S OUTPUT IS KEPT. The first cut counted landed branches
    # and reported "only 5 of 6" with no way to tell a server refusal
    # from a `git worktree add` that failed because the branch already
    # existed. A count is not a diagnosis.
    inpod "$pre; cd /tmp/$r && for i in \$(seq 1 $CONC); do ( git worktree add -q -f /tmp/$r-w\$i -b agent/c\$i HEAD > /tmp/$r-wt\$i.log 2>&1 || echo WORKTREE_FAILED >> /tmp/$r-wt\$i.log; cd /tmp/$r-w\$i 2>/dev/null && echo burst-$RUN-\$i >> big.txt && git commit -qam c\$i >/dev/null 2>&1 && G push \$U agent/c\$i > /tmp/$r-p\$i.log 2>&1 || echo PUSH_FAILED >> /tmp/$r-p\$i.log ) & done; wait" >/dev/null 2>&1
    sleep 6
    after=$(ondisk_packs "$r")
    newp=$(comm -13 <(printf '%s\n' "$before" | sort) <(printf '%s\n' "$after" | sort) | grep -c . )
    namedp=$(named_packs "$r" | grep -c . )
    unnamed=$(comm -23 <(printf '%s\n' "$after" | sort) <(named_packs "$r" | sort) | grep -c . )
    note "$r: $newp new pack(s) on disk after the burst, $namedp named, $unnamed on disk but unnamed"
    if [ "$newp" -lt 2 ]; then
      bad "$r: the burst produced $newp new pack(s) — it did not actually run concurrently"
    else
      ok "$r: the burst produced $newp new packs"
    fi
    if [ "$unnamed" = 0 ]; then
      ok "$r: NO pack is on disk but unnamed — every accepted push was named"
    else
      bad "$r: $unnamed pack(s) on disk are unnamed after an ALL-ACCEPTED burst — objects are stranded"
    fi
    # Every branch must have landed.
    local landed; landed=$(named_refs "$r" | grep -c "^refs/heads/agent/c")
    if [ "$landed" = "$CONC" ]; then
      ok "$r: all $CONC branches landed"
    else
      bad "$r: only $landed of $CONC branches landed"
      # Say WHICH and WHY, so the next reader is not left with a count.
      inpod "for i in \$(seq 1 $CONC); do if grep -qE 'WORKTREE_FAILED|PUSH_FAILED|rejected|error' /tmp/$r-wt\$i.log /tmp/$r-p\$i.log 2>/dev/null; then echo \"push \$i: \$(cat /tmp/$r-wt\$i.log /tmp/$r-p\$i.log 2>/dev/null | tr '\\n' '|' | cut -c1-160)\"; fi; done" \
        | head -6 | sed 's/^/        · /'
    fi
  done
  for r in ctl treated; do integrity "$r" H1; done

  # ── H2 — a concurrent burst with REFUSALS mixed in ──────────────────
  #
  # Now the keys can genuinely cross: accepted and refused pushes in
  # flight together, each with a record to hand off. A crossed key makes
  # direction 5 decline an ACCEPTED push's pack, which H1's oracle
  # catches only if it happens on an all-accepted burst — here it is the
  # realistic shape.
  leg H2 "$CONC concurrent pushes, half REFUSED — no accepted pack may be declined"
  for r in ctl treated; do
    pre=$(door_pre "$r")
    before=$(ondisk_packs "$r")
    inpod "$pre; cd /tmp/$r && for i in \$(seq 1 $CONC); do ( if [ \$((i % 2)) -eq 0 ]; then
             git worktree add -q -f /tmp/$r-x\$i -b agent/x\$i HEAD >/dev/null 2>&1; cd /tmp/$r-x\$i && echo ok-$RUN-\$i >> big.txt && git commit -qam x\$i && G push -q \$U agent/x\$i;
           else
             git worktree add -q -f /tmp/$r-y\$i --detach agent/c\$i >/dev/null 2>&1; cd /tmp/$r-y\$i && git reset -q --hard HEAD~1 && echo nff-$RUN-\$i >> big.txt && git commit -qam y\$i && G push -q --force \$U HEAD:agent/c\$i;
           fi ) & done; wait" >/dev/null 2>&1
    sleep 6
    after=$(ondisk_packs "$r")
    unnamed=$(comm -23 <(printf '%s\n' "$after" | sort) <(named_packs "$r" | sort) | grep -c . )
    note "$r: $unnamed pack(s) on disk but unnamed after the mixed burst"
    # The CONTROL names everything, so any unnamed pack there is a rig
    # problem rather than a rule one — which is what makes the treated
    # arm's count meaningful.
    if [ "$r" = ctl ]; then
      [ "$unnamed" = 0 ] && ok "ctl: names every pack, as the directory rule must" \
        || bad "ctl: $unnamed unnamed pack(s) — the directory rule should name all of them"
    else
      [ "$unnamed" -gt 0 ] && ok "treated: declines $unnamed refused pack(s)" \
        || note "treated: declined none this round (the refusals may have shared names)"
    fi
    local landed_x; landed_x=$(named_refs "$r" | grep -c "^refs/heads/agent/x")
    [ "$landed_x" -gt 0 ] && ok "$r: the accepted half of the burst landed ($landed_x)" \
      || bad "$r: none of the accepted half landed"
  done
  for r in ctl treated; do integrity "$r" H2; done

  # ── H3 — a MIXED push, which is direction 5's ceiling ───────────────
  #
  # One push, two refs, ONE pack: one ref accepted and one refused. The
  # pack holds accepted objects, so direction 5 MUST NAME IT. This is
  # the case where the rule is required to do the thing it usually
  # avoids, and getting it backwards strands live objects.
  leg H3 "a MIXED push (one ref accepted, one refused) must still be NAMED"
  for r in ctl treated; do
    pre=$(door_pre "$r")
    before=$(ondisk_packs "$r")
    inpod "cd /tmp/$r && git worktree add -q -f /tmp/$r-mx -b agent/mix HEAD >/dev/null 2>&1; cd /tmp/$r-mx && echo mixed-$RUN >> big.txt && git commit -qam mixed" >/dev/null
    # one NEW branch (accepted) and one non-ff on an existing one
    # (refused), in a SINGLE push, so both share one pack.
    inpod "$pre; cd /tmp/$r-mx && G push \$U HEAD:refs/heads/agent/mix HEAD~1:refs/heads/agent/c1 2>&1 | tail -3" >/dev/null 2>&1
    sleep 5
    after=$(ondisk_packs "$r")
    local mixpack; mixpack=$(comm -13 <(printf '%s\n' "$before" | sort) <(printf '%s\n' "$after" | sort) | head -1)
    if [ -z "$mixpack" ]; then
      note "$r: the mixed push produced no new pack (content may have deduplicated)"
    elif named_packs "$r" | grep -qx "$mixpack"; then
      ok "$r: the mixed push's pack is NAMED — accepted objects are not stranded"
    else
      bad "$r: the mixed push's pack is NOT named — the accepted ref's objects are stranded"
    fi
    named_refs "$r" | grep -qx "refs/heads/agent/mix" \
      && ok "$r: the accepted half of the mixed push landed" \
      || bad "$r: the accepted half of the mixed push did not land"
  done
  for r in ctl treated; do integrity "$r" H3; done

  # ── H4 — `git push --atomic` ────────────────────────────────────────
  #
  # An atomic push is all-or-nothing, so a refused command refuses the
  # WHOLE push and its pack holds nothing that landed. The prediction
  # was that direction 5 should be COMPLETE for atomic pushes; this is
  # the first time it is asked on a wire.
  leg H4 "an ATOMIC push refused as a whole leaves a pack the accepted set declines"
  for r in ctl treated; do
    pre=$(door_pre "$r")
    before=$(ondisk_packs "$r")
    inpod "cd /tmp/$r && git worktree add -q -f /tmp/$r-at -b agent/atomic HEAD >/dev/null 2>&1; cd /tmp/$r-at && echo atomic-$RUN >> big.txt && git commit -qam atomic" >/dev/null
    # --force, or GIT'S OWN CLIENT refuses the non-fast-forward before
    # anything is sent: the first cut of this leg pushed without it,
    # nothing reached the server, no pack was created, and the leg
    # reported "no new pack" and moved on having tested nothing.
    local out; out=$(inpod "$pre; cd /tmp/$r-at && G push --atomic --force \$U HEAD:refs/heads/agent/atomic HEAD~1:refs/heads/agent/c2 2>&1 | tail -3")
    note "$r: atomic push said: $(printf '%s' "$out" | tr '\n' '|' | cut -c1-120)"
    sleep 5
    after=$(ondisk_packs "$r")
    local atpack; atpack=$(comm -13 <(printf '%s\n' "$before" | sort) <(printf '%s\n' "$after" | sort) | head -1)
    # AN ACCEPTED PUSH MUST FOLLOW, or nothing publishes a snapshot and
    # neither arm names the pack — the control then says "did not name
    # it either" and the treated arm's decline is free. A refused batch
    # never CASes; the next accepted one is what names what is on disk.
    inpod "cd /tmp/$r && git worktree add -q -f /tmp/$r-ap -b agent/atpub HEAD >/dev/null 2>&1; cd /tmp/$r-ap && echo atpub-$RUN >> big.txt && git commit -qam atpub" >/dev/null
    inpod "$pre; cd /tmp/$r-ap && G push -q \$U agent/atpub 2>&1|tail -1" >/dev/null
    sleep 5
    if named_refs "$r" | grep -qx "refs/heads/agent/atomic"; then
      bad "$r: the atomic push LANDED a ref although one of its commands was refused"
    else
      ok "$r: the atomic push was refused as a whole (no ref landed)"
    fi
    if [ -z "$atpack" ]; then
      bad "$r: the atomic push left NO pack — this leg tested nothing (did the client refuse it locally?)"
    elif named_packs "$r" | grep -qx "$atpack"; then
      [ "$r" = ctl ] && ok "ctl: names the refused atomic push's pack (the pinning)" \
                     || bad "treated: NAMED a wholly-refused atomic push's pack"
    else
      if [ "$r" = treated ]; then
        ok "treated: declines the wholly-refused atomic push's pack"
      else
        bad "ctl did NOT name the refused atomic pack — treated's decline is undiscriminated"
      fi
    fi
  done
  for r in ctl treated; do integrity "$r" H4; done

  # ── H5 — many restarts: the collector must CONVERGE ─────────────────
  #
  # The reclaim runs at EVERY restore. A rule that keeps finding
  # something to drop would shave the repository on each restart until
  # it dropped something it needed. Proven twice in a unit test; this
  # asks it on the wire, three times, with the integrity oracle between.
  # ── H5 — the COLLECTOR: it collects, then converges ─────────────────
  #
  # A THING THIS DRILL TAUGHT ME. With direction 5 ON there is almost
  # nothing for direction 4 to collect, because the residue was never
  # NAMED in the first place — the reducer and the collector overlap,
  # and the collector's remaining job is residue the reducer could not
  # prevent: repositories that accumulated it before the rule existed,
  # and mixed-push packs that direction 5 must name and that later go
  # dead. The first cut of this leg watched three restarts change
  # nothing and called it convergence; converged because it never
  # started is not convergence.
  #
  # So the collector is given something to collect: direction 5 is
  # turned OFF on the treated arm, leaving direction 4 on, and residue
  # is made under the directory rule. That is the real-world shape.
  leg H5 "the collector collects residue direction 5 did not prevent, then converges"
  $K -n "$NS_AGENTS" patch flintrepo treated --type merge \
    -p '{"spec":{"packs":{"nameAcceptedSet":false,"reclaimAtRest":true}}}' >/dev/null 2>&1
  $K -n "$NS_AGENTS" rollout status deploy/forge-treated --timeout=300s >/dev/null 2>&1
  sleep 8
  local pre5; pre5=$(door_pre treated)
  # Refusals that leave packs, then an ACCEPTED push to publish a
  # snapshot that NAMES them — a refused batch never CASes on its own.
  local j
  for j in 1 2 3; do
    inpod "cd /tmp/treated && git worktree add -q -f /tmp/treated-r$j --detach agent/c$j >/dev/null 2>&1; cd /tmp/treated-r$j && git reset -q --hard HEAD~1 && echo dead-$RUN-$j >> big.txt && git commit -qam dead$j" >/dev/null
    inpod "$pre5; cd /tmp/treated-r$j && G push -q --force \$U HEAD:agent/c$j 2>&1|tail -1" >/dev/null
  done
  inpod "cd /tmp/treated && git worktree add -q -f /tmp/treated-pub -b agent/pub HEAD >/dev/null 2>&1; cd /tmp/treated-pub && echo pub-$RUN >> big.txt && git commit -qam pub" >/dev/null
  inpod "$pre5; cd /tmp/treated-pub && G push -q \$U agent/pub 2>&1|tail -1" >/dev/null
  sleep 6
  local prev cur i collected=0
  prev=$(named_packs treated | grep -c . )
  note "with direction 5 off, treated names $prev pack(s) — residue included"
  for i in 1 2 3; do
    $K -n "$NS_AGENTS" rollout restart deploy/forge-treated >/dev/null 2>&1
    $K -n "$NS_AGENTS" rollout status deploy/forge-treated --timeout=300s >/dev/null 2>&1
    sleep 12
    cur=$(named_packs treated | grep -c . )
    note "restart $i: treated names $cur pack(s) (was $prev)"
    [ "$cur" -le "$prev" ] || bad "restart $i: the named set GREW, $prev -> $cur"
    [ "$cur" -lt "$prev" ] && collected=1
    integrity treated "H5/restart$i" || break
    [ "$i" = 3 ] && { [ "$cur" = "$prev" ] && ok "converged — the last restart changed nothing" \
                       || bad "still collecting on restart 3 ($prev -> $cur)"; }
    prev=$cur
  done
  # WITHOUT THIS THE CONVERGENCE CLAIM IS FREE: a collector that never
  # ran also never changes anything on the third restart.
  [ "$collected" = 1 ] && ok "the collector actually collected, so the plateau means something" \
                       || bad "the collector never collected — 'converged' would be vacuous"
  # THE CONTROL: the same refusals and restarts with the reclaim OFF.
  local cb ca
  cb=$(named_packs ctl | grep -c . )
  $K -n "$NS_AGENTS" rollout restart deploy/forge-ctl >/dev/null 2>&1
  $K -n "$NS_AGENTS" rollout status deploy/forge-ctl --timeout=300s >/dev/null 2>&1
  sleep 10
  ca=$(named_packs ctl | grep -c . )
  [ "$ca" = "$cb" ] && ok "ctl held at $ca pack(s) across a restart — the collector is what moved treated" \
                    || bad "ctl moved $cb -> $ca across a restart with the reclaim OFF"
  integrity ctl H5

  verdict
}

cleanup() {
  local rc=$?
  [ "${KEEP:-0}" = 1 ] && echo "KEEP=1: namespaces left standing" || {
    $K delete ns "$NS_SYS" "$NS_AGENTS" --ignore-not-found --wait=false >/dev/null 2>&1
    helm --kube-context "$CTX" uninstall flint-forge -n "$NS_SYS" >/dev/null 2>&1; }
  exit $rc
}
trap cleanup EXIT
main "$@"
