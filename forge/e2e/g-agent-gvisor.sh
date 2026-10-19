#!/usr/bin/env bash
# gVisor agents: the AGENT pod runs under runsc (RuntimeClass gvisor).
#
#   BUCKET=... PREFIX=... TAG=... ./forge/e2e/g-agent-gvisor.sh
#   (after export/run.sh: the rig, and export/gvisor-setup.sh on the workers)
#
#   GA4 PRECONDITION: the runsc pod really runs on gVisor's kernel and the
#       runc control on the host's (dmesg / /proc/version), so a pass is not
#       a runc pass by accident. Records the runsc version and platform.
#   GA1 a runsc agent — credential helper + projected token (audience
#       forge.chert.us) — clones `--single-branch -b main` into an emptyDir,
#       commits, `git push -u origin agent/$POD_NAME`: exit 0, and the ref is
#       in the bucket's snapshot (durable on return; read in-cluster)
#   GA2 from that pod, `git propose` merges into protected main, and the
#       PLAIN export publishes it: CURRENT = the new main, files byte-identical
#   GA3 controls: (a) GA1 from a runc pod passes; (b) a runsc pod presenting
#       its DEFAULT ServiceAccount token (API-server audience) is refused
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
. "$here/export/lib.sh"
current(){ s3cat "$PREFIX/exp-export/CURRENT" | tr -d '\n'; }
# Pod names carry a run id: the branch is agent/$POD_NAME, and a rerun from
# a newer main must not collide with the branch an earlier run left.
R=${RUN_ID:-$(date +%s | tail -c 6)}
RS=ga-runsc-$R; RC_=ga-runc-$R; DT=ga-deftok-$R

echo "══ G: forge agents under gVisor ══"
kubectl get runtimeclass gvisor >/dev/null 2>&1 || { bad "no RuntimeClass gvisor (run export/gvisor-setup.sh)"; summary; exit $?; }
agent_pod $RS gvisor forge  || bad "$RS never became Ready"
agent_pod $RC_ runc   forge  || bad "$RC_ never became Ready"

echo "── GA4: which kernel ──"
kern(){ kubectl exec -n "$NS" "$1" -- sh -c 'dmesg 2>/dev/null | head -3; cat /proc/version' 2>/dev/null; }
KS=$(kern $RS); KC=$(kern $RC_)
NODE=$(kubectl -n "$NS" get pod $RS -o jsonpath='{.spec.nodeName}')
RV=$("$here/../../scripts/nodesh.sh" "$NODE" 'runsc --version 2>/dev/null | head -1; grep platform /etc/containerd/runsc.toml' 2>/dev/null | tr '\n' ' ')
echo "   runsc on $NODE: $RV"
if echo "$KS" | grep -qi gvisor && ! echo "$KC" | grep -qi gvisor; then
  ok "GA4 $RS runs on gVisor ('$(echo "$KS" | grep -i -m1 gvisor)'); $RC_ on the host kernel"
else bad "GA4 runsc pod kernel: '$(echo "$KS" | head -2 | tr '\n' ' ')' / runc pod: '$(echo "$KC" | tail -1)'"; fi

# ga1 <pod>: clone, commit, push agent/<pod>; prints "<rc> <head>"
ga1(){ asg "$1" "cd /work && rm -rf exp && git clone -q --single-branch -b main $DOOR/git/$NS/exp.git exp && cd exp \
  && git switch -q -c agent/\$POD_NAME && echo \"from \$POD_NAME \$(date +%s)\" > agent-\$POD_NAME.txt \
  && git add -A && git commit -qm \"work from \$POD_NAME\" && git push -q -u origin agent/\$POD_NAME 2>/tmp/push.err; echo \"\$? \$(git rev-parse HEAD)\"" | tail -1; }

echo "── GA1: runsc agent pushes its branch ──"
read -r RC H <<< "$(ga1 $RS)"
S=$(snap_ref exp "refs/heads/agent/$RS")
[ "$RC" = 0 ] && [ "$S" = "$H" ] && ok "GA1 push exited 0 and the bucket's snapshot names agent/$RS = $H" \
  || bad "GA1 rc=$RC head=$H snapshot=$S ($(kubectl exec -n "$NS" $RS -- cat /tmp/push.err 2>/dev/null | tail -2 | tr '\n' ' '))"

echo "── GA2: runsc agent proposes into protected main; the plain export follows ──"
P=$(asg $RS "cd /work/exp && git propose 2>&1; echo rc=\$?")
M=$(gitq exp rev-parse refs/heads/main)
if echo "$P" | grep -q 'rc=0' && [ -n "$M" ] && gitq exp merge-base --is-ancestor "$H" "$M"; then
  if wait_for "$M" 180 current; then
    N=0; D=0
    while read -r p; do [ -z "$p" ] && continue; N=$((N+1))
      [ "$(gitq exp show "$M:$p" | sha256sum | cut -d' ' -f1)" = "$(s3cat "$PREFIX/exp-export/commits/$M/$p" | sha256sum | cut -d' ' -f1)" ] || D=$((D+1))
    done <<< "$(gitq exp ls-tree -r --name-only "$M")"
    [ "$D" = 0 ] && [ "$N" -gt 0 ] && ok "GA2 propose merged $H into main ($M); CURRENT = $M and all $N files byte-identical" \
      || bad "GA2 CURRENT = $M but $D of $N files differ"
  else bad "GA2 main = $M but CURRENT is '$(current)'"; fi
else bad "GA2 propose: $(echo "$P" | tail -3 | tr '\n' ' ') main=$M"; fi

echo "── GA3: controls ──"
read -r RC H2 <<< "$(ga1 $RC_)"
S2=$(snap_ref exp "refs/heads/agent/$RC_")
[ "$RC" = 0 ] && [ "$S2" = "$H2" ] && ok "GA3a the same push from a runc pod: exit 0, snapshot names $H2" || bad "GA3a runc rc=$RC snapshot=$S2"
agent_pod $DT gvisor default || bad "$DT never became Ready"
R=$(asg $DT "cd /work && git clone -q --single-branch -b main $DOOR/git/$NS/exp.git x 2>&1; echo rc=\$?")
if echo "$R" | grep -q 'rc=0'; then bad "GA3b a DEFAULT-audience token was ACCEPTED by the door"
elif echo "$R" | grep -qiE '401|403|authentication failed|denied|forbidden'; then ok "GA3b a default-audience token is refused: $(echo "$R" | grep -iE -m1 '401|403|authentication|denied|forbidden')"
else bad "GA3b the clone failed, but not as a refusal: $(echo "$R" | tail -2 | tr '\n' ' ')"; fi

kubectl -n "$NS" delete pod $RS $RC_ $DT --wait=false >/dev/null 2>&1
summary
