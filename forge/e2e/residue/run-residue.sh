#!/usr/bin/env bash
# THE RESIDUE DRILL — do the two pack rules do on the wire what they
# were modelled and measured to do?
#
# Two repositories on ONE cluster, from ONE chart install, with ONE
# agent image and ONE workload script. `ctl` asks for neither rule;
# `treated` asks for both. Everything else is identical, so a difference
# in what the snapshot NAMES is the rules and nothing else.
#
# WHY THE REFUSALS ARE NON-FAST-FORWARDS ON AGENT BRANCHES and not
# pushes to protected `main`. Residue exists because git migrates a
# push's pack out of quarantine when `pre-receive` passes, BEFORE
# `proc-receive` carries forge's verdict. A push `pre-receive` refuses —
# which is what a direct push to a protected branch is — has its whole
# quarantine discarded and leaves NOTHING. Using it here would drill a
# repository with no residue in it and both arms would agree at zero:
# a green measuring nothing. A non-fast-forward to an `agent/*` branch
# passes `pre-receive` (the pattern allows it) and is refused by the
# syncer at `proc-receive`, which is exactly the class that leaves a
# pack behind.
#
#   CTX=kind-forge-residue TAG=drill-<sha8> ./run-residue.sh
#   KEEP=1 ... to leave the cluster up
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
CHART="$ROOT/flint-forge-chart"
CTX=${CTX:-$(kubectl config current-context 2>/dev/null)}
TAG=${TAG:?set TAG to the drill image tag}
K="kubectl --context $CTX"
NS_SYS=forge-system
NS_AGENTS=agents
DOOR=${DOOR:-http://flint-forge-door.$NS_SYS.svc}
RUN=$(date +%s)

PASS=0; FAILED=0; SKIPPED=0
ok()  { PASS=$((PASS+1));     echo "  ok: $1"; }
bad() { FAILED=$((FAILED+1)); echo "  BAD: $1"; }
note(){ echo "  ..  $1"; }
leg() { echo; echo "── $1 — $2"; }
mcx() { $K -n "$NS_SYS" exec mc-s3 -- "$@" 2>/dev/null; }

# THE TWO DIRECTIONS NEED OPPOSITE RIG CONDITIONS, and one run cannot
# show both.
#
#   COMPACT=0 (DEFAULT) leaves the shipped thresholds alone, and BOTH
#   legs work in it: the seed push already leaves a pack that covers the
#   history, so direction 4 has a coverer, and no base rebuild runs to
#   erase direction 5's difference before it is measured. The green run
#   of record is this one — R3 and R4 both pass, 12/0.
#
#   COMPACT=1 drives a base rebuild every cycle. Direction 4 still shows
#   (larger, because more is superseded), but direction 5 CANNOT: `--all`
#   is the only thing that drops dead objects, so a frequent base
#   collects the residue in BOTH arms and there is no pinning left to
#   observe. R3 is SKIPPED there rather than failed — the condition is
#   wrong for that claim, which is not the same as the claim being
#   false.
render_rig() {
  # The knobs go through a FILE, not `awk -v`: awk rejects a newline in
  # a -v assignment, and the multi-line block silently became an error
  # on every line of the template.
  local kf; kf=$(mktemp)
  if [ "${COMPACT:-0}" = 1 ]; then
    cat > "$kf" <<'KNOBS'
  syncerEnv:
    FLINT_FORGE_FOLD_MIN_MIB: "0"
    FLINT_FORGE_BASE_MIN_MIB: "0"
    FLINT_FORGE_BASE_REBUILD_MIN_SECS: "0"
    FLINT_FORGE_FOLD_FACTOR: "2"
KNOBS
  fi
  awk -v kf="$kf" -v tag="$TAG" '
    /^__KNOBS__$/ { while ((getline line < kf) > 0) print line; close(kf); next }
    { gsub(/__TAG__/, tag); print }' "$HERE/rig.yaml.tpl"
  rm -f "$kf"
}
inpod() { $K -n "$NS_AGENTS" exec writer -c agent -- sh -c "$*" 2>&1; }

door_pre() { # door_pre <repo>
  printf 'T=$(cat /var/run/secrets/forge/token); A="Authorization: Basic $(printf "x:%%s" "$T" | base64 -w0)"; G() { git -c http.extraHeader="$A" "$@"; }; U=%s/git/%s/%s.git' \
         "$DOOR" "$NS_AGENTS" "$1"
}

# The snapshot a repository has published, straight out of the bucket.
snapshot_of() { mcx mc cat "m/s3bucket/residue/$1/git/snapshot" 2>/dev/null; }

# Bytes of the packs the snapshot NAMES — the number the whole finding
# is about. Read from the BUCKET, not from a pod's disk: what forge
# keeps is what it names durably.
named_bytes() { # named_bytes <repo>
  local snap; snap=$(snapshot_of "$1")
  [ -n "$snap" ] || { echo "0 0"; return; }
  local packs; packs=$(printf '%s' "$snap" | python3 -c 'import sys,json; print(" ".join(json.load(sys.stdin).get("packs",[])))' 2>/dev/null)
  local total=0 n=0 sz
  for p in $packs; do
    sz=$(mcx mc stat --json "m/s3bucket/residue/$1/git/objects/pack/$p" 2>/dev/null \
         | python3 -c 'import sys,json;
try: print(json.load(sys.stdin).get("size",0))
except Exception: print(0)' 2>/dev/null)
    total=$((total + ${sz:-0})); n=$((n+1))
  done
  echo "$n $total"
}

# The identical workload, run against whichever repository it is given.
workload() { # workload <repo>
  local r=$1 pre; pre=$(door_pre "$r")
  # A corpus that deltifies, so pack sizes mean something.
  inpod "$pre; rm -rf /tmp/$r && G clone -q \$U /tmp/$r 2>&1 | tail -2" >/dev/null
  inpod "cd /tmp/$r && git config user.email t@example.invalid && git config user.name t" >/dev/null
  inpod "cd /tmp/$r && i=0; while [ \$i -lt 4000 ]; do echo \"line \$i\"; i=\$((i+1)); done > big.txt && git add big.txt && git commit -qm base" >/dev/null
  inpod "$pre; cd /tmp/$r && G push -q origin HEAD:refs/for/main 2>&1 | tail -2" >/dev/null
  local refused=0
  for i in 1 2 3; do
    # An accepted agent-branch push: this one lands, and its pack must
    # be named by BOTH arms.
    inpod "cd /tmp/$r && git checkout -q -B agent/b$i && sed -i \"s/^line 100\$/line 100 a$i/\" big.txt && git commit -qam a$i" >/dev/null
    inpod "$pre; cd /tmp/$r && G push -q origin agent/b$i 2>&1 | tail -1" >/dev/null
    # Now a DIVERGENT commit on the same branch, force-pushed: passes
    # pre-receive (agent/* is allowed) and is refused by the syncer as
    # a non-fast-forward — the class that leaves a pack on disk.
    inpod "cd /tmp/$r && git reset -q --hard HEAD~1 && sed -i \"s/^line 200\$/line 200 nff$i/\" big.txt && git commit -qam nff$i" >/dev/null
    local out; out=$(inpod "$pre; cd /tmp/$r && G push --force origin agent/b$i 2>&1 | tail -3")
    # WHICH HOOK REFUSED IT IS THE WHOLE QUESTION, not whether one did.
    # `rejected` is git's word for both, and a `pre-receive` refusal
    # discards the quarantine and leaves NO pack — so counting it as a
    # refusal would mean drilling a repository with no residue in it
    # and calling the resulting agreement a result.
    # TO STDERR, because this function's STDOUT is its return value.
    # Writing a note here polluted the count with the note text, and
    # the leg then compared a paragraph against "3".
    echo "  ..  $r push $i: $(printf '%s' "$out" | tr '\n' '|' | cut -c1-140)" >&2
    case "$out" in
      *non-fast-forward*) refused=$((refused+1)) ;;
      *) echo "  ..  $r: push $i was not refused as a non-fast-forward" >&2 ;;
    esac
    inpod "cd /tmp/$r && git reset -q --hard origin/agent/b$i" >/dev/null
  done
  echo "$refused"
}

# The packs a repository has on disk, by name.
ondisk_packs() { # ondisk_packs <repo>
  local pod; pod=$($K -n "$NS_AGENTS" get pod -o name 2>/dev/null | grep "forge-$1" | head -1)
  [ -n "$pod" ] || return 0
  $K -n "$NS_AGENTS" exec "${pod#pod/}" -c git-http -- sh -c \
    "cd /repo/$NS_AGENTS/$1.git && ls objects/pack/*.pack 2>/dev/null | sed 's#.*/##'" 2>/dev/null | tr -d '\r'
}

# The packs the published snapshot NAMES.
named_packs() { # named_packs <repo>
  snapshot_of "$1" | python3 -c 'import sys,json
try: print("\n".join(json.load(sys.stdin).get("packs", [])))
except Exception: pass' 2>/dev/null
}

verdict() {
  echo
  echo "══ residue drill: $PASS passed, $FAILED failed, $SKIPPED skipped ══"
  [ "$FAILED" -gt 0 ] && { echo "   FAILED"; return 1; }
  # A leg that could not run is not a leg that passed, and the verdict
  # says so rather than folding it into the green.
  [ "$SKIPPED" -gt 0 ] && { echo "   GREEN, with $SKIPPED leg(s) skipped for the rig condition"; return 2; }
  echo "   GREEN"; return 0
}

main() {
  echo "residue drill — context $CTX, images $TAG"

  leg R0 "the rig stands up and BOTH repositories serve"
  render_rig | $K apply -f - >/dev/null 2>&1
  $K -n "$NS_SYS" rollout status deploy/minio --timeout=180s >/dev/null 2>&1
  $K -n "$NS_SYS" wait --for=condition=complete job/seed-bucket --timeout=180s >/dev/null 2>&1
  $K -n "$NS_SYS" wait --for=condition=ready pod/mc-s3 --timeout=120s >/dev/null 2>&1
  helm --kube-context "$CTX" upgrade --install flint-forge "$CHART" -n "$NS_SYS" \
       --set image.tag="$TAG" --set image.pullPolicy=IfNotPresent \
       --set server.gitImage="dilipdalton/flint-forge-git:$TAG" \
       --set server.syncerImage="dilipdalton/flint-forge-syncer:$TAG" \
       --set door.deploy=true --set door.namespace="$NS_SYS" \
       --wait --timeout 6m > /tmp/residue-helm-$RUN.log 2>&1
  if [ $? -ne 0 ]; then bad "helm install failed"; tail -6 /tmp/residue-helm-$RUN.log | sed 's/^/        /'; verdict; return 1; fi
  render_rig | $K apply -f - >/dev/null 2>&1
  $K -n "$NS_AGENTS" wait --for=condition=ready pod/writer --timeout=180s >/dev/null 2>&1
  local up=0
  for r in ctl treated; do
    for _ in $(seq 1 60); do
      [ "$($K -n "$NS_AGENTS" get flintrepo "$r" -o jsonpath='{.status.phase}' 2>/dev/null)" = Ready ] && { up=$((up+1)); break; }
      sleep 5
    done
  done
  [ "$up" = 2 ] && ok "both repositories reached Ready" || { bad "only $up/2 repositories became Ready"; verdict; return 1; }

  # THE PLUMBING CHECK, and it comes before any measurement. A spec
  # field the CRD does not declare is PRUNED silently, and the syncer
  # would then run with its defaults — both rules off — while the drill
  # believed it was measuring them. Every number below would be a
  # careful comparison of two identical arms.
  leg R1 "the treated repository's syncer actually received the two flags"
  local env_t env_c
  env_t=$($K -n "$NS_AGENTS" get deploy forge-treated -o json 2>/dev/null | tr -d ' \n')
  env_c=$($K -n "$NS_AGENTS" get deploy forge-ctl -o json 2>/dev/null | tr -d ' \n')
  case "$env_t" in
    *FLINT_FORGE_NAME_ACCEPTED_SET*) ok "treated carries FLINT_FORGE_NAME_ACCEPTED_SET" ;;
    *) bad "treated does NOT carry FLINT_FORGE_NAME_ACCEPTED_SET — the field was pruned or not rendered" ;;
  esac
  case "$env_t" in
    *FLINT_FORGE_RECLAIM_AT_REST*) ok "treated carries FLINT_FORGE_RECLAIM_AT_REST" ;;
    *) bad "treated does NOT carry FLINT_FORGE_RECLAIM_AT_REST" ;;
  esac
  # The control must NOT carry them, or the arms do not differ.
  case "$env_c" in
    *FLINT_FORGE_NAME_ACCEPTED_SET*|*FLINT_FORGE_RECLAIM_AT_REST*)
      bad "the CONTROL carries a pack flag — the arms do not differ" ;;
    *) ok "the control carries neither flag" ;;
  esac

  leg R2 "the same workload against both, and the refusals are real"
  local rc rt
  rc=$(workload ctl); rt=$(workload treated)
  note "refusals: ctl $rc/3, treated $rt/3"
  if [ "$rc" = 3 ] && [ "$rt" = 3 ]; then
    ok "every non-fast-forward was refused in both arms"
  else
    bad "refusals differ or did not happen (ctl $rc, treated $rt) — there is no residue to measure"
    verdict; return 1
  fi

  # ── R3 — direction 5, asked precisely ────────────────────────────
  #
  # NOT by comparing total named bytes. An earlier cut did that and it
  # was the wrong oracle: the arms drift for reasons that have nothing
  # to do with the rule (compaction timing, and pack names being
  # MANY-TO-ONE so identical content collapses), and the difference
  # vanished into the noise — the leg passed a `<` on a three-byte gap
  # and then failed a 25% threshold while the rule was working.
  #
  # The claim is about ONE pack: the one a REFUSED push leaves behind.
  # A refused batch never CASes, so the pack sits on disk unnamed until
  # the NEXT ACCEPTED push publishes a snapshot — and that is the
  # moment the directory rule names it and pins it forever. So: take
  # the disk before and after a refusal to identify that exact pack,
  # land an accepted push, and ask whether the snapshot names it.
  # The control must say YES or the leg is not testing the rule.
  leg R3 "direction 5: the pack a REFUSED push leaves is not named by the accepted set"
  if [ "${COMPACT:-0}" = 1 ]; then
    note "SKIPPED at COMPACT=1: a base rebuild runs every cycle and \`--all\` drops dead"
    note "objects, so the residue is collected in BOTH arms and there is no pinning to"
    note "observe. This leg needs COMPACT=0. Skipped, not failed — the condition is"
    note "wrong for the claim, which is not the same as the claim being false."
    SKIPPED=$((SKIPPED+1))
  else
  local r before after residue named verdict_ctl="" verdict_treated=""
  for r in ctl treated; do
    local pre; pre=$(door_pre "$r")
    inpod "cd /tmp/$r && git checkout -q -B agent/probe && echo probe-$RUN >> big.txt && git commit -qam probe" >/dev/null
    inpod "$pre; cd /tmp/$r && G push -q \$U agent/probe 2>&1 | tail -1" >/dev/null
    sleep 3
    # CAPTURED AFTER THE ACCEPTED PUSH, which is the whole point of the
    # diff: taking it before meant `after \ before` held the accepted
    # push's pack AS WELL as the refused one, and `head -1` picked
    # whichever sorted first. It picked the accepted pack, which is of
    # course named — so the leg reported "direction 5 named the refused
    # pack" while direction 5 was working correctly. The control passed
    # on the same mistake by luck: under the directory rule every pack
    # is named, so it answers "NAMES" whichever one you point at.
    before=$(ondisk_packs "$r")
    # The refusal that leaves a pack.
    inpod "cd /tmp/$r && git reset -q --hard HEAD~1 && echo diverge-$RUN >> big.txt && git commit -qam diverge" >/dev/null
    inpod "$pre; cd /tmp/$r && G push --force \$U agent/probe 2>&1 | tail -1" >/dev/null
    sleep 3
    after=$(ondisk_packs "$r")
    local newpacks; newpacks=$(comm -13 <(printf '%s\n' "$before" | sort) <(printf '%s\n' "$after" | sort))
    residue=$(printf '%s\n' "$newpacks" | grep -c . )
    if [ "$residue" != 1 ]; then
      bad "$r: the refused push left $residue new pack(s), expected exactly 1 — the diff does not isolate the residue"
      note "$r: new packs were: $(printf '%s' "$newpacks" | tr '\n' ' ')"
      continue
    fi
    residue=$(printf '%s\n' "$newpacks" | head -1)
    note "$r: the refusal left $residue on disk"
    # The next ACCEPTED push is what publishes a snapshot.
    inpod "cd /tmp/$r && git reset -q --hard origin/agent/probe 2>/dev/null; git checkout -q -B agent/probe2 && echo after-$RUN >> big.txt && git commit -qam after" >/dev/null
    inpod "$pre; cd /tmp/$r && G push -q \$U agent/probe2 2>&1 | tail -1" >/dev/null
    sleep 4
    if named_packs "$r" | grep -qx "$residue"; then
      note "$r: the snapshot NAMES $residue"
      [ "$r" = ctl ] && verdict_ctl=named || verdict_treated=named
    else
      note "$r: the snapshot does NOT name $residue"
      [ "$r" = ctl ] && verdict_ctl=absent || verdict_treated=absent
    fi
  done
  # THE CONTROL FIRST: if the directory rule does not name the residue
  # either, this cluster produced no pinning and the treated arm's
  # "does not name it" would be free.
  if [ "$verdict_ctl" = named ]; then
    ok "the control NAMES the refused push's pack — the pinning is reproduced here"
  else
    bad "the control did not name the refused pack ($verdict_ctl) — nothing below discriminates"
  fi
  if [ "$verdict_treated" = absent ]; then
    ok "direction 5 declines to name it, on the same cluster and the same workload"
  else
    bad "direction 5 NAMED the refused push's pack ($verdict_treated) — the rule is not in effect"
  fi

  # Bytes, reported but not asserted: they move with compaction timing
  # and with pack-name collisions, so they are context, not the claim.
  fi
  local nc bc nt bt
  read -r nc bc <<<"$(named_bytes ctl)"
  read -r nt bt <<<"$(named_bytes treated)"
  note "named totals — ctl: $nc pack(s) $bc B;  treated: $nt pack(s) $bt B"

  leg R4 "direction 4: a RESTART collects what is left, and the control's does not"
  # BOTH are restarted. The reclaim runs at restore, so restarting only
  # the treated arm would confound the rule with the restart itself —
  # a base rebuild on the way back up would move bytes in either arm.
  local before_b=$bt before_c=$bc
  for r in ctl treated; do
    $K -n "$NS_AGENTS" rollout restart deploy/forge-$r >/dev/null 2>&1
  done
  for r in ctl treated; do
    $K -n "$NS_AGENTS" rollout status deploy/forge-$r --timeout=300s >/dev/null 2>&1
  done
  sleep 15
  local na ba nca bca
  read -r na ba <<<"$(named_bytes treated)"
  read -r nca bca <<<"$(named_bytes ctl)"
  note "after restart — ctl: $nca pack(s) $bca B (was $bc B);  treated: $na pack(s) $ba B (was $bt B)"
  if [ "$ba" -gt 0 ] && [ "$bca" -gt 0 ]; then
    ok "both arms still name packs (neither emptied itself)"
  else
    bad "an arm names 0 B after the restart — the reclaim took everything"
  fi
  if [ "$ba" -le "$before_b" ]; then
    ok "the treated named set did not grow across the restart ($before_b B -> $ba B)"
  else
    bad "the treated named set GREW across a restart: $before_b B -> $ba B"
  fi
  # THE COLLECTOR'S OWN CLAIM, with the control as the discriminator:
  # the same restart, the same rebuild, and only one arm reclaims.
  if [ "$((ba * 4))" -lt "$((bca * 3))" ]; then
    ok "after the restart treated names $ba B against the control's $bca B"
  else
    bad "after the restart treated names $ba B and the control $bca B — the collector took nothing the restart did not"
  fi

  leg R5 "and the treated repository still serves a clone that fsck's"
  local pre; pre=$(door_pre treated)
  local out
  out=$(inpod "$pre; rm -rf /tmp/fresh && G clone -q \$U /tmp/fresh && cd /tmp/fresh && git fsck --strict --no-progress >/dev/null 2>&1 && git rev-parse HEAD >/dev/null && echo CLONE_OK")
  case "$out" in
    *CLONE_OK*) ok "a fresh clone of the treated repository passes git fsck --strict" ;;
    *) bad "the treated repository does not clone cleanly: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
  esac
  # The control must still clone too, or R5 says nothing about the rules.
  local pre2; pre2=$(door_pre ctl)
  out=$(inpod "$pre2; rm -rf /tmp/fresh2 && G clone -q \$U /tmp/fresh2 && cd /tmp/fresh2 && git fsck --strict --no-progress >/dev/null 2>&1 && echo CLONE_OK")
  case "$out" in
    *CLONE_OK*) ok "the control clones cleanly too (R5 discriminates)" ;;
    *) bad "the CONTROL does not clone cleanly — the rig is broken, not the rules" ;;
  esac

  verdict
}

cleanup() {
  local rc=$?
  if [ "${KEEP:-0}" != 1 ]; then
    $K delete ns "$NS_SYS" "$NS_AGENTS" --ignore-not-found --wait=false >/dev/null 2>&1
    helm --kube-context "$CTX" uninstall flint-forge -n "$NS_SYS" >/dev/null 2>&1
  else
    echo "KEEP=1: namespaces left standing"
  fi
  exit $rc
}
trap cleanup EXIT
main "$@"
