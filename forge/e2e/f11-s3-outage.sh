#!/usr/bin/env bash
# FALSIFIER 11 — S3 outage.
#
# Pushes fail with a clear message; clones and fetches keep succeeding.
#
# This header used to go on "until the lease can no longer be renewed;
# the server then EXITS". The syncer has never had that mechanism (X13
# in docs/plans/flint-forge-simplification-2026-09-05.md): a renewal
# that errors without a 412 is "keep serving reads, keep trying". The
# stand-down leg below passed on 2026-09-04 because the push leg before
# it crashed the process (any batch error exits the serving loop) and
# the restart's claim failed against the dead S3. Run the stand-down
# leg BEFORE the push leg and it fails today; that order is X13's
# acceptance. Why the stand-down is still worth having: a server that
# keeps serving through an outage a challenger does not share is a
# stale reader — a second writer it is not, the rotation fences a
# straggler's CAS — and stale reads are what X13 closes.
set -uo pipefail
NS=${NS:-agents}; REPO=${REPO:-proj}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
pass=0; fail=0
ok(){ pass=$((pass+1)); echo "  PASS  $1"; }
bad(){ fail=$((fail+1)); echo "  FAIL  $1"; }

agent(){ kubectl exec -n "$NS" agent1 -- sh -c "
  T=\$(cat /var/run/secrets/forge/token); H=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 -w0)\"
  G(){ git -c http.$DOOR/.extraHeader=\"\$H\" \"\$@\"; }
  $1" 2>&1; }

echo "══ F11: S3 goes away ══"
R0=$(kubectl get pods -n "$NS" -l chert.us/repo=proj -o jsonpath='{.items[0].status.containerStatuses[?(@.name=="syncer")].restartCount}')
echo "── cutting the server off from S3 ──"
cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: { name: s3-outage, namespace: $NS }
spec:
  podSelector: { matchLabels: { chert.us/repo: $REPO } }
  policyTypes: [Egress]
  egress:
    # Everything inside the cluster still works; only the outside world
    # (which is where S3 is) is gone. Without this the pod loses DNS too
    # and the failure stops being an S3 outage.
    - to: [ { ipBlock: { cidr: "10.0.0.0/8" } } ]
    - ports: [ { protocol: UDP, port: 53 }, { protocol: TCP, port: 53 } ]
YAML
sleep 5

echo "── a CLONE during the outage (should still work: it is a read) ──"
OUT=$(agent 'rm -rf /tmp/o1 && G clone -q '"$DOOR"'/git/'"$NS"'/'"$REPO"'.git /tmp/o1 && echo CLONE-OK')
case "$OUT" in *CLONE-OK*) ok "clones keep serving from local packs during the outage" ;;
               *) bad "clone failed during the outage: $(echo "$OUT" | tail -1)" ;; esac

echo "── a PUSH during the outage (must fail, and say why) ──"
OUT=$(agent 'cd /tmp/o1 && git config user.email o@x.y && git config user.name out && echo x > outage.txt && git add -A && git commit -qm outage && G push origin HEAD:refs/heads/agent/outage 2>&1 | tail -3')
echo "$OUT" | sed 's/^/      /'
case "$OUT" in
  *"remote rejected"*|*"error"*|*"fatal"*) ok "the push was refused rather than silently accepted" ;;
  *) bad "the push appears to have SUCCEEDED with no S3 behind it" ;;
esac

echo "── and does the server eventually stand down? ──"
# VACUOUS on its own: what stands the server down today is the push
# leg above, not a lease term. See the header and X13.
GONE=no
for i in $(seq 1 30); do
  R=$(kubectl get pods -n "$NS" -l chert.us/repo=proj -o jsonpath='{.items[0].status.containerStatuses[?(@.name=="syncer")].restartCount}' 2>/dev/null)
  RD=$(kubectl get pods -n "$NS" -l chert.us/repo=proj -o jsonpath='{.items[0].status.containerStatuses[?(@.name=="syncer")].ready}' 2>/dev/null)
  if [ "${R:-0}" != "$R0" ] || [ "$RD" = "false" ]; then GONE=yes; echo "      stood down after ~$((i*10))s (restarts $R0 -> $R, ready=$RD)"; break; fi
  sleep 10
done
[ "$GONE" = yes ] && ok "the server stopped serving rather than holding a lease it cannot renew" \
  || bad "after 300s the server was still serving with no S3 — that is a second writer waiting to happen"

echo "── healing ──"; kubectl delete networkpolicy -n "$NS" s3-outage >/dev/null 2>&1
kubectl wait -n "$NS" --for=condition=Available deploy/forge-proj --timeout=300s >/dev/null 2>&1 && ok "recovers once S3 returns" || bad "did not recover"
echo ""; echo "══ $pass passed, $fail failed ══"
