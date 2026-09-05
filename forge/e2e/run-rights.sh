#!/usr/bin/env bash
# Per-principal rights on ONE forge repository, and the boundary that
# makes them mean anything. Verifies the deck's security plate row
# ("Yes, per ServiceAccount") and the radar's Rights axis score for
# forge, both scored from policy.rs:judge and render.rs:network_policy
# with no wire behind them until now.
#
# The claim has two halves and a red hazard, and each is a leg:
#   1. consumers grants read to both principals; the branch policy
#      decides writes per principal. A reader (consumers only) is
#      refused main; the writer (named in mergeInto) merges into it.
#   2. THE BOUNDARY. X-Remote-User is trustworthy only because a
#      NetworkPolicy admits only the door to the repo pod's git port.
#      Reach 8080 directly and you set the header yourself. This is why
#      the drill needs a real CNI: on kind's default CNI a NetworkPolicy
#      is inert and the block would pass vacuously. This cluster runs
#      Cilium.
#   3. THE HAZARD (plate, in red): "until a branches block exists, every
#      consumer may push." The `open` repo has no policy.
#
# Prereqs: `s3csi/e2e/run-s3csi.sh setup` (for the in-cluster MinIO),
# the forge chart deployed with door.deploy=true and door.namespace set,
# and `kubectl apply -f forge/e2e/rig-kind.yaml`.
set -uo pipefail
CTX=${CTX:-kind-flint-s3csi}
K="kubectl --context $CTX"
NS=${NS:-agents}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
WRITER_PRINCIPAL=system:serviceaccount:agents:forge-writer
RUN=$(date +%s)
PASS=0; FAILED=0; RAN=""
bad()  { echo "  BAD: $1"; FAILED=$((FAILED+1)); }
ok()   { PASS=$((PASS+1)); echo "  ok: $1"; }
note() { echo "  ..  $1"; }
leg()  { RAN="$RAN $1"; echo; echo "── $1 — $2"; }

# The door auth preamble, emitted for a pod to run: the pod's projected
# token is the basic password, `G` carries it, `$U` is the door URL.
# Command substitution output is NOT re-expanded by the calling shell,
# so `$T`/`$A`/`$U` here stay literal for the pod; in the appended
# scripts, pod-side vars are written `\$U` and my own vars ($RUN) bare.
door_pre() { printf 'T=$(cat /var/run/secrets/forge/token); A="Authorization: Basic $(printf "x:%%s" "$T" | base64 -w0)"; G() { git -c http.extraHeader="$A" "$@"; }; U=%s/git/%s/%s.git' "$DOOR" "$NS" "$1"; }
inpod() { local p=$1; shift; $K -n "$NS" exec "$p" -c agent -- sh -c "$*" 2>&1; }
expect() { local pod=$1 exp=$2 label=$3 script=$4 out rc; out=$(inpod "$pod" "$script"); rc=$?
  case $exp in
    ok)     [ $rc -eq 0 ] && ok "$label" || { bad "$label — expected success, rc=$rc"; echo "$out" | tail -3 | sed 's/^/        /'; } ;;
    refuse) if [ $rc -ne 0 ]; then ok "$label (refused)"; echo "$out" | grep -iE 'protected|may (be pushed|propose)|only .* may|not under|denied|remote:' | head -1 | sed 's/^/        · /'
            else bad "$label — SUCCEEDED but the policy should have refused it"; fi ;;
  esac
}

echo "forge per-principal rights — context $CTX, repo $NS/proj"
$K -n "$NS" get flintrepo proj open >/dev/null 2>&1 || { echo "rig absent; apply forge/e2e/rig-kind.yaml"; exit 2; }

# ── F1 both read; the writer seeds main via its merge right ──────────
leg F1 "consumers grants read to BOTH; the writer creates main via refs/for/main and both clone and read it"
expect writer ok "writer seeds main" "$(door_pre proj); rm -rf /tmp/w && G clone -q \$U /tmp/w && cd /tmp/w && git config user.email w@x && git config user.name writer && { git checkout -q main 2>/dev/null || git checkout -q -b main; } && echo 'seed-$RUN' > README.md && git add -A && git commit -qm seed && G push origin HEAD:refs/for/main"
expect writer ok "main exists after the seed" "$(door_pre proj); cd /tmp/w && G fetch -q origin && git rev-parse --verify origin/main >/dev/null"
expect reader ok "reader clones proj and reads main (read is granted by consumers)" "$(door_pre proj); rm -rf /tmp/r && G clone -q \$U /tmp/r && cd /tmp/r && git rev-parse --verify origin/main >/dev/null && grep -q 'seed-$RUN' README.md"

# ── F2 the reader may push an agent branch but not main ──────────────
leg F2 "the reader pushes agent/* (agentPattern) but is REFUSED a direct push to protected main"
expect reader ok "reader pushes agent/reader-$RUN" "$(door_pre proj); cd /tmp/r && git config user.email r@x && git config user.name reader && git checkout -q -b agent/reader-$RUN && echo r > r-$RUN.txt && git add -A && git commit -qm r && G push -q origin agent/reader-$RUN"
expect reader refuse "reader's direct push to main" "$(door_pre proj); cd /tmp/r && git checkout -q -B main origin/main && echo x >> README.md && git commit -qam touch && G push origin main"

# ── F3 refs/for/main is per-principal ────────────────────────────────
leg F3 "refs/for/main: the reader is refused (not in mergeInto), the writer merges"
expect reader refuse "reader's merge request to refs/for/main" "$(door_pre proj); cd /tmp/r && git checkout -q -B prop-r-$RUN origin/main && echo rr >> README.md && git commit -qam rr && G push origin HEAD:refs/for/main"
expect writer ok "writer's merge request to refs/for/main lands" "$(door_pre proj); cd /tmp/w && git checkout -q -B prop-w-$RUN origin/main && echo 'by writer $RUN' >> README.md && git commit -qam ww && G push origin HEAD:refs/for/main"
expect writer ok "main now carries the writer's change" "$(door_pre proj); cd /tmp/w && G fetch -q origin && git show origin/main:README.md | grep -q 'by writer $RUN'"
expect reader ok "the reader can pull the writer's merged result (read is shared)" "$(door_pre proj); cd /tmp/r && G fetch -q origin && git show origin/main:README.md | grep -q 'by writer $RUN'"

# ── F4 THE BOUNDARY: the door overrides a forged header ──────────────
leg F4 "the door OVERRIDES a forged X-Remote-User: the reader claiming to be the writer is still refused"
expect reader refuse "reader sends X-Remote-User: writer to the DOOR, pushes refs/for/main" "$(door_pre proj); cd /tmp/r && git checkout -q -B door-forge-$RUN origin/main && echo fd >> README.md && git commit -qam fd && git -c http.extraHeader=\"\$A\" -c http.extraHeader='X-Remote-User: $WRITER_PRINCIPAL' push \$U HEAD:refs/for/main"

# ── F5 THE BOUNDARY: the git port is not reachable directly ──────────
leg F5 "the NetworkPolicy refuses a direct TCP connection to the repo pod's git port; the door is reachable"
RAN="$RAN F5"
inpod reader "nc -w 8 -z forge-proj.$NS.svc 8080"; rc=$?
[ $rc -ne 0 ] && ok "reader cannot open forge-proj:8080 directly (nc rc=$rc)" || bad "reader OPENED the git port directly — the NetworkPolicy did not block L4"
inpod reader "nc -w 8 -z flint-forge-door.forge-system.svc 80"; rc=$?
[ $rc -eq 0 ] && ok "CONTROL: reader CAN open the door:80 — the block is peer-scoped, not a dead cluster" || bad "CONTROL: reader cannot reach the door either (nc rc=$rc) — the F5 block may be vacuous"

# ── F6 THE BOUNDARY, vacuity-breaker: no policy ⇒ the forge lands ────
leg F6 "with the NetworkPolicy removed, the forged direct push LANDS — proving F5's block is real, not a dead port"
$K -n forge-system scale deploy/flint-forge --replicas=0 >/dev/null 2>&1
for i in $(seq 1 30); do [ "$($K -n forge-system get deploy flint-forge -o jsonpath='{.status.availableReplicas}' 2>/dev/null)" != "1" ] && break; sleep 1; done
$K -n "$NS" delete networkpolicy forge-proj --ignore-not-found >/dev/null 2>&1
sleep 4
inpod reader "nc -w 8 -z forge-proj.$NS.svc 8080"; rc=$?
[ $rc -eq 0 ] && ok "with no policy the port is now reachable (nc rc=$rc)" || note "port still not reachable yet (nc rc=$rc) — the push below is the real test"
out=$(inpod reader "$(door_pre proj); cd /tmp/r && git checkout -q -B bypass-$RUN origin/main && echo 'forged by reader as writer $RUN' >> README.md && git config user.email b@x && git config user.name bypass && git commit -qam bypass && git -c http.extraHeader='X-Remote-User: $WRITER_PRINCIPAL' push http://forge-proj.$NS.svc:8080/$NS/proj.git HEAD:refs/for/main"); rc=$?
RAN="$RAN F6"
landed=""; for i in $(seq 1 12); do inpod writer "$(door_pre proj); cd /tmp/w && G fetch -q origin && git show origin/main:README.md | grep -q 'forged by reader as writer $RUN'" >/dev/null 2>&1 && { landed=1; break; }; sleep 2; done
[ -n "$landed" ] && ok "forged X-Remote-User=writer, pushed straight to 8080, MERGED into main — the header is trusted with no NetworkPolicy" \
                  || bad "the forged direct push did not land (rc=$rc): $(echo "$out" | grep -iE 'remote:|denied|refused|protected|fatal|could not' | head -1 | cut -c1-140)"
# restore the operator and let it re-render the policy
$K -n forge-system scale deploy/flint-forge --replicas=1 >/dev/null 2>&1
$K -n forge-system rollout status deploy/flint-forge --timeout=120s >/dev/null 2>&1
for i in $(seq 1 40); do $K -n "$NS" get networkpolicy forge-proj >/dev/null 2>&1 && break; sleep 2; done
if $K -n "$NS" get networkpolicy forge-proj >/dev/null 2>&1; then
  sleep 3; inpod reader "nc -w 8 -z forge-proj.$NS.svc 8080"; rc=$?
  [ $rc -ne 0 ] && ok "the operator recreated the NetworkPolicy and the port is blocked again (nc rc=$rc)" || bad "policy back but the port is still reachable"
else bad "the NetworkPolicy was not restored — recreate it before leaving the rig"; fi

# ── F7 THE HAZARD: no branches block ⇒ every consumer may push main ──
leg F7 "the red hazard: with NO branches block, the reader pushes straight to main on 'open'"
expect reader ok "reader seeds+pushes main on 'open' directly (no policy exists to refuse it)" "$(door_pre open); rm -rf /tmp/o && G clone -q \$U /tmp/o && cd /tmp/o && git config user.email r@x && git config user.name reader && { git checkout -q main 2>/dev/null || git checkout -q -b main; } && echo 'open push $RUN' >> README.md && git add -A && git commit -qm open && G push origin main"
expect reader ok "and it moved main on 'open'" "$(door_pre open); cd /tmp/o && G fetch -q origin && git show origin/main:README.md | grep -q 'open push $RUN'"

# ── roster ───────────────────────────────────────────────────────────
echo
for l in F1 F2 F3 F4 F5 F6 F7; do case " $RAN " in *" $l "*) ;; *) bad "leg $l never ran";; esac; done
echo
echo "══ $PASS ok, $FAILED bad ══"
[ "$FAILED" = 0 ]
