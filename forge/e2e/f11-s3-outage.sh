#!/usr/bin/env bash
# FALSIFIER 11 — S3 outage.
#
# Pushes fail with a clear message; clones and fetches keep succeeding
# within the holder's term; past six heartbeats without a landed
# renewal the syncer withdraws readiness (X13) — the lease is kept, the
# process stays up, and the next renewal that lands restores it.
#
# HISTORY. This header used to promise "the server then EXITS", and the
# stand-down leg passed on 2026-09-04 for the wrong reason: it ran AFTER
# the push leg, the push crashed the process (any batch error exits the
# serving loop), the restart's claim failed against the dead S3, and the
# crash loop read as standing down. The syncer had no term of its own
# (X13 in docs/plans/flint-forge-simplification-2026-09-05.md, found by
# reading Continuity's every-read-verified rule against lease.rs). X13
# was built 2026-09-05; the stand-down leg now runs BEFORE any push and
# is judged by X13's signature — ready=false with the restart count
# UNCHANGED. A restart here is the old vacuous pass and is reported as
# a failure of the leg. NOT YET RE-RUN ON THE WIRE after the change.
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

echo "── does the server stand down on its own, with NO push since the cut? (X13) ──"
# Judged by X13's signature: ready=false while the restart count is
# UNCHANGED. Six heartbeats of 10 s is the term; the probe adds ~5 s.
GONE=no; WHY=""
for i in $(seq 1 12); do
  R=$(kubectl get pods -n "$NS" -l chert.us/repo=proj -o jsonpath='{.items[0].status.containerStatuses[?(@.name=="syncer")].restartCount}' 2>/dev/null)
  RD=$(kubectl get pods -n "$NS" -l chert.us/repo=proj -o jsonpath='{.items[0].status.containerStatuses[?(@.name=="syncer")].ready}' 2>/dev/null)
  if [ "${R:-0}" != "$R0" ]; then WHY="restarted ($R0 -> $R) — a crash, not the term"; break; fi
  if [ "$RD" = "false" ]; then GONE=yes; echo "      readiness withdrawn after ~$((i*10))s, restarts unchanged ($R0)"; break; fi
  sleep 10
done
if [ "$GONE" = yes ]; then ok "the server withdrew readiness on its own within the term, without exiting (X13)"
elif [ -n "$WHY" ]; then bad "the server stood down by $WHY"
else bad "after 120s the server was still ready with no S3 — reads a challenger could not share (X13 not in effect)"; fi

echo "── a PUSH during the outage (must fail, and say why) ──"
OUT=$(agent 'cd /tmp/o1 && git config user.email o@x.y && git config user.name out && echo x > outage.txt && git add -A && git commit -qm outage && G push origin HEAD:refs/heads/agent/outage 2>&1 | tail -3')
echo "$OUT" | sed 's/^/      /'
case "$OUT" in
  *"remote rejected"*|*"error"*|*"fatal"*) ok "the push was refused rather than silently accepted" ;;
  *) bad "the push appears to have SUCCEEDED with no S3 behind it" ;;
esac

echo "── healing ──"; kubectl delete networkpolicy -n "$NS" s3-outage >/dev/null 2>&1
kubectl wait -n "$NS" --for=condition=Available deploy/forge-proj --timeout=300s >/dev/null 2>&1 && ok "recovers once S3 returns: a renewal lands and readiness comes back" || bad "did not recover"
echo ""; echo "══ $pass passed, $fail failed ══"
