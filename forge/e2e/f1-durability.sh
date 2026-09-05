#!/usr/bin/env bash
# FALSIFIER 1 — acknowledged means durable.
#
# The design kills the syncer between step 4 (upload) and step 5 (CAS).
# There is no fault-injection hook in the shipped binary, so this does
# the black-box version: push something big enough to have a window,
# kill the pod at a random moment inside it, and check the INVARIANT
# rather than the timing —
#
#     the client was told `ok`  ==>  the bucket holds the commit
#
# NOTE THE ARROW. It is an implication, not an equivalence, and the
# first run of this drill asserted the equivalence and "failed" twice.
# The converse cannot hold for ANY system whose acknowledgement can be
# lost in flight: kill the pod after the CAS but before the response
# reaches the client and the client sees a broken connection while the
# bucket holds the commit. git push carries no idempotency token, so
# there is nothing to reconcile against.
#
# What must be true instead is that the indeterminate outcome is
# BENIGN: the object is complete, fsck passes, and the agent's natural
# retry is a clean no-op rather than a conflict or a duplicate. That is
# checked here, so "indeterminate" is a measured result and not an
# excuse.
#
# Told-ok-but-absent stays a hard failure. That one silently loses an
# agent's work.
#
# Repeat, because one iteration only samples one moment in the window.
set -uo pipefail
NS=${NS:-agents}; REPO=${REPO:-proj}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
: "${BUCKET:?set BUCKET}"; PREFIX=${PREFIX:-drill}
ITER=${ITER:-6}
pass=0; fail=0
ok(){ pass=$((pass+1)); echo "  PASS  $1"; }
bad(){ fail=$((fail+1)); echo "  FAIL  $1"; }
snap_ref(){ aws s3 cp "s3://$BUCKET/$PREFIX/git/git/snapshot" - 2>/dev/null | jq -r ".refs[\"$1\"] // \"<absent>\""; }
gitq(){ kubectl exec -n "$NS" deploy/forge-proj -c syncer -- git --git-dir=/repo/$NS/$REPO.git "$@" 2>/dev/null; }

echo "══ F1: acknowledged <=> durable, across $ITER kills mid-push ══"
acked=0; refused=0
for i in $(seq 1 "$ITER"); do
  kubectl wait -n "$NS" --for=condition=Available deploy/forge-proj --timeout=300s >/dev/null 2>&1
  REF="refs/heads/agent/dur-$i"
  BEFORE=$(snap_ref "$REF")

  # A payload big enough that the push is not instantaneous.
  kubectl exec -n "$NS" agent1 -- sh -c "
    T=\$(cat /var/run/secrets/forge/token); H=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 -w0)\"
    rm -rf /tmp/d$i && git -c http.$DOOR/.extraHeader=\"\$H\" clone -q --depth 1 $DOOR/git/$NS/$REPO.git /tmp/d$i 2>/dev/null
    cd /tmp/d$i && git config user.email d@x.y && git config user.name dur
    mkdir -p big && j=0; while [ \$j -lt 48 ]; do dd if=/dev/urandom of=big/f\$j bs=256k count=1 status=none; j=\$((j+1)); done
    git add -A && git commit -qm dur$i && git rev-parse HEAD > /tmp/oid$i" >/dev/null 2>&1
  OID=$(kubectl exec -n "$NS" agent1 -- cat /tmp/oid$i 2>/dev/null)

  ( kubectl exec -n "$NS" agent1 -- sh -c "
      T=\$(cat /var/run/secrets/forge/token); H=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 -w0)\"
      cd /tmp/d$i && git -c http.$DOOR/.extraHeader=\"\$H\" push origin HEAD:$REF >/dev/null 2>&1 \
        && echo ACK > /tmp/res$i || echo NAK > /tmp/res$i" >/dev/null 2>&1 ) &
  PUSHER=$!
  # Land inside the push rather than before or after it.
  awk "BEGIN{srand($i); printf \"%.2f\", 0.4 + rand()*2.2}" | xargs sleep
  POD=$(kubectl get pods -n "$NS" -l chert.us/repo=proj -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)
  [ -n "$POD" ] && kubectl delete pod -n "$NS" "$POD" --force --grace-period=0 >/dev/null 2>&1
  wait $PUSHER 2>/dev/null

  RES=$(kubectl exec -n "$NS" agent1 -- cat /tmp/res$i 2>/dev/null || echo NAK)
  kubectl wait -n "$NS" --for=condition=Available deploy/forge-proj --timeout=300s >/dev/null 2>&1
  AFTER=$(snap_ref "$REF")

  case "$RES" in
    ACK) acked=$((acked+1))
         [ "$AFTER" = "$OID" ] && ok "iter $i: told ok, and the bucket holds it" \
           || bad "iter $i: TOLD OK BUT THE BUCKET LACKS IT (want $OID, bucket $AFTER)" ;;
    *)   refused=$((refused+1))
         if [ "$AFTER" = "$BEFORE" ]; then
           ok "iter $i: told failed, and the bucket is unchanged"
         elif [ "$AFTER" = "$OID" ]; then
           # The acknowledgement was lost after the CAS. Benign only if
           # the agent's retry resolves cleanly — so check that, do not
           # assume it.
           RETRY=$(kubectl exec -n "$NS" agent1 -- sh -c "
             T=\$(cat /var/run/secrets/forge/token); H=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 -w0)\"
             cd /tmp/d$i && git -c http.$DOOR/.extraHeader=\"\$H\" push origin HEAD:$REF 2>&1 | tail -1" 2>/dev/null)
           case "$RETRY" in
             *"up-to-date"*|*"$REF"*) ok "iter $i: indeterminate (ack lost after the CAS); the retry is a clean no-op" ;;
             *) bad "iter $i: ack lost AND the retry did not resolve: $RETRY" ;;
           esac
         else
           bad "iter $i: TOLD FAILED AND THE BUCKET HOLDS SOMETHING ELSE ($BEFORE -> $AFTER)"
         fi ;;
  esac
done

gitq fsck --strict >/dev/null 2>&1 && ok "fsck --strict clean after $ITER crashes" || bad "fsck failed after the crash series"
echo ""
echo "  ($acked acknowledged, $refused refused — both arms need to occur for this to mean much)"
[ "$acked" -gt 0 ] && [ "$refused" -gt 0 ] && ok "the kill actually landed on both sides of the window" \
  || bad "every iteration landed the same way; the timing did not sample the window"
echo ""; echo "══ $pass passed, $fail failed ══"
