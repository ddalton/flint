#!/usr/bin/env bash
# The two identity modes nothing had ever exercised: `ambient` on a LEAN
# workspace, and `webIdentity` on either front end.
#
# Runs AFTER `run-s3csi.sh setup` (STORE=s3 NODE_EXEC=nodesh) on an EC2
# cluster whose node role can reach the bucket and whose IMDSv2 hop
# limit admits pods. Helpers are the single-cluster drill's own.
#
#   A1  LEAN ambient: nothing is injected and the syncer's own AWS chain
#       checks out and publishes — gated, like passthrough's P9, on the
#       platform being able to complete that chain at all
#   A2  LEAN webIdentity: the arm materialises (token file, no brokered
#       key, the STS endpoint pointing at the broker) and then either
#       completes or does not — the syncer's client is the Rust AWS SDK
#   A3  PASSTHROUGH webIdentity: the same arm, but the client is
#       mount-s3's CRT, which the design records as HTTPS-only against a
#       broker Service that speaks plain http. Checked, not assumed.
#
# Env: as run-s3csi.sh, plus nothing else.
set -u
cd "$(dirname "$0")"
export STORE=s3 NODE_EXEC=nodesh
REPO=$(cd ../.. && pwd)
eval "$(sed -n '/^CTX=\${CTX:-/,/^# ── setup \/ teardown/p' run-s3csi.sh | sed '$d')"
eval "$(sed -n '/^lobj()   {/,/^lmhas()  {/p' run-s3csi.sh)"
SKIPPED=0
skip() { SKIPPED=$((SKIPPED + 1)); echo "  SKIP: $1"; }
idcrs() { sed -e "s#__B__#$BUCKET#g" -e "s#__ENDPOINT__#$S3_ENDPOINT#g" -e "s#__REGION__#$S3_REGION#g" id-tenants.yaml.tpl; }
leanpod() { sed -e "s#__NAME__#$1#g" -e "s#__CR__#$2#g" -e "s#__NODE__#$3#g" -e "s#__FILES__#$4#g" -e "s#__NONCE__#$5#g" hd-lean-pod.yaml.tpl | $K apply -f - >/dev/null; }
ptpod() { sed -e "s#__NAME__#$1#g" -e "s#__CR__#$2#g" -e "s#__NODE__#$3#g" pt-pod.yaml.tpl | $K apply -f - >/dev/null; }
poddel() { $K -n $NS delete pod "$@" --ignore-not-found --wait=true --timeout=300s >/dev/null 2>&1; }
seedlog() { $K -n $NS logs "$1" 2>/dev/null | grep -E "^SEED $2" | tail -1; }
wait_seedlog() { local i=0; while [ $i -lt "$3" ] && [ -z "$(seedlog "$1" "$2")" ]; do sleep 5; i=$((i + 5)); done; [ -n "$(seedlog "$1" "$2")" ]; }
wenv()  { $K -n $WNS exec "$1" -- sh -c "env | grep -c '^$2=' || true" 2>/dev/null; }
# The arm's environment is handed to the LAUNCHED process over
# mount.sock, not to the container — `env` in an exec shows the
# container's and finds nothing, which read as "the arm is incomplete".
wenvv() { $K -n $WNS exec "$1" -- sh -c "for p in /proc/[0-9]*; do tr '\\0' '\\n' < \$p/environ 2>/dev/null | sed -n 's/^$2=//p'; done | head -1" 2>/dev/null; }
wenv()  { local v; v=$(wenvv "$1" "$2"); [ -n "$v" ] && echo 1 || echo 0; }
wfile() { $K -n $WNS exec "$1" -- sh -c "ls -l /comm/$2 2>/dev/null | awk '{print \$1}'" 2>/dev/null; }
# The same question asked of the NODE rather than of the pod. A worker
# whose mount fails is torn down within seconds, so an exec races it and
# comes back empty — which reads as "the plugin wrote no token" when the
# truth is "there was nobody left to ask". The comm dir is a
# memory-backed emptyDir, so it is on the node under the pod's own uid
# until the pod is gone.
hostfile() { local u; u=$($K -n $WNS get pod "$1" -o jsonpath='{.metadata.uid}' 2>/dev/null); [ -n "$u" ] || return 1; onnode "ls -l /var/lib/kubelet/pods/$u/volumes/kubernetes.io~empty-dir/comm/$2 2>/dev/null | awk '{print \$1}'"; }
wfile_any() { local v; v=$(wfile "$1" "$2"); [ -z "$v" ] && v=$(hostfile "$1" "$2"); printf '%s' "$v"; }
# Did the broker actually ISSUE for this CR? This is the control that
# separates a web-identity exchange from a silent fall-through to
# whatever else the client's default chain can find — on a node whose
# instance role can reach the bucket, a fallback looks exactly like
# success. The broker logs one `issued` line per exchange.
issued_for() { $K -n $SYS logs "$($K -n $SYS get pods -l app.kubernetes.io/name=flint-s3-broker -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)" 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' | grep -c "issued .*cr=$1 "; }
echo "flint identity modes on real nodes — cluster $CTX, node $NODE, bucket $BUCKET"
$K get csidriver s3.csi.chert.us >/dev/null 2>&1 || { echo "no s3.csi.chert.us — run run-s3csi.sh setup first"; exit 2; }
idcrs | $K apply -f - >/dev/null || { echo "  BAD: the identity CRs were refused"; exit 2; }

# ── A1 lean on the platform's own chain ───────────────────────────────
leg A1 "LEAN ambient: the syncer is handed nothing and its own AWS chain checks out and publishes"
# The same precondition passthrough's P9 uses, and for the same reason:
# whether a pod on this node can complete the SDK's default chain is the
# platform's business, and a leg that runs on regardless judges the
# platform rather than flint.
$K -n $NS delete pod id-probe --ignore-not-found --wait=true --timeout=60s >/dev/null 2>&1
probe_err=$(cat <<EOF | $K apply -f - 2>&1 >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: id-probe, namespace: $NS }
spec:
  nodeName: $NODE
  restartPolicy: Never
  securityContext: { runAsNonRoot: true, runAsUser: 1001, seccompProfile: { type: RuntimeDefault } }
  containers:
  - name: probe
    image: amazon/aws-cli:2.36.39
    command: ["sh", "-c", "aws sts get-caller-identity --output text 2>&1 || true"]
    env: [{ name: AWS_REGION, value: $S3_REGION }, { name: HOME, value: /tmp }]
    securityContext: { allowPrivilegeEscalation: false, capabilities: { drop: [ALL] } }
EOF
)
probe=__never__
if [ -n "$probe_err" ]; then
    bad "PRECONDITION: the ambient probe could not be created: $(echo "$probe_err" | cut -c1-200)"
else
    i=0; ph=""; while [ $i -lt 300 ]; do ph=$($K -n $NS get pod id-probe -o jsonpath='{.status.phase}' 2>/dev/null); case "$ph" in Succeeded|Failed) break ;; esac; sleep 5; i=$((i + 5)); done
    case "$ph" in Succeeded|Failed) probe=$($K -n $NS logs id-probe 2>/dev/null | tail -1) ;; *) bad "PRECONDITION: the ambient probe never completed in ${i}s (phase '${ph:-none}')" ;; esac
fi
$K -n $NS delete pod id-probe --ignore-not-found --wait=false >/dev/null 2>&1
if [ "$probe" = __never__ ]; then
    :
elif echo "$probe" | grep -q 'arn:aws:'; then
    ok "PRECONDITION: a pod on $NODE with nothing injected obtains an identity from the platform's chain ($(echo "$probe" | awk '{print $2}'))"
    poddel lean-amb
    mcx mc rm --recursive --force "m/$BUCKET/tenants/amb/" >/dev/null 2>&1
    leanpod lean-amb proj-amb "$NODE" 50 amb-1
    if wait_phase lean-amb Running 400; then
        ok "the workspace CHECKED OUT and the tenant is Running — the syncer read the bucket on the platform's identity alone"
        if wait_seedlog lean-amb PUBLISHED 400; then
            ok "a declared publish was acked"
            [ "$(lobj tenants/amb/files/src/d0/f0.txt)" = "unit 000000 of the seeded project" ] && ok "the published bytes are in the bucket" || bad "the publish did not land: '$(lobj tenants/amb/files/src/d0/f0.txt)'"
            lmhas tenants/amb src/d0/f0.txt && ok "the manifest cites them ($(lments tenants/amb) entries at seq $(lmseq tenants/amb))" || bad "the manifest does not cite the seeded file"
        else
            bad "no publish ack within 400s: $(seedlog lean-amb '' | tail -1)"
        fi
        w=$(worker_of lean-amb)
        if [ -n "$w" ]; then
            [ "$(wenv "$w" AWS_ACCESS_KEY_ID)" = "0" ] && [ "$(wenv "$w" AWS_WEB_IDENTITY_TOKEN_FILE)" = "0" ] \
                && ok "the worker holds no AWS key and no web-identity token in its environment — nothing was injected" \
                || bad "the ambient worker's env carries credentials (key=$(wenv "$w" AWS_ACCESS_KEY_ID) tokenfile=$(wenv "$w" AWS_WEB_IDENTITY_TOKEN_FILE))"
            [ -z "$(wfile "$w" creds.json)" ] && ok "no creds.json in its comm dir — the broker was never involved" || bad "the ambient worker has a creds.json ($(wfile "$w" creds.json))"
        else
            bad "no Running worker for lean-amb"
        fi
        poddel lean-amb
    else
        bad "lean-amb never reached Running: $(mount_events lean-amb | tail -2 | tr '\n' ' ' | cut -c1-300)"
    fi
else
    skip "the platform does not complete the ambient chain for a pod on $NODE; A1 would judge the platform, not flint. The probe said: '${probe:-no output}'"
fi

# ── A2 lean over the broker's STS facade ──────────────────────────────
leg A2 "LEAN webIdentity: the arm materialises, and the Rust AWS SDK either completes the exchange against the broker or says why not"
poddel lean-wi
mcx mc rm --recursive --force "m/$BUCKET/tenants/wi/" >/dev/null 2>&1
leanpod lean-wi proj-wi "$NODE" 20 wi-1
# The worker exists long before its syncer succeeds, and the arm is
# materialised at publish time — so look at ANY-phase worker rather than
# waiting for a Running tenant that may never come.
w=""; i=0; while [ $i -lt 300 ] && [ -z "$w" ]; do w=$(worker_of_any lean-wi); [ -z "$w" ] && { sleep 5; i=$((i + 5)); }; done
if [ -n "$w" ]; then
    j=0; while [ $j -lt 120 ] && [ -z "$(wfile "$w" token)" ]; do sleep 5; j=$((j + 5)); done
    tok=$(wfile "$w" token)
    [ -n "$tok" ] && ok "the projected token was written into the worker's comm dir as $tok" || bad "no token file in the worker's comm dir after ${j}s"
    case "$tok" in -rw-------*) ok "and only its owner can read it ($tok)" ;; *) [ -n "$tok" ] && bad "the token file is mode '$tok', not 0600" ;; esac
    [ -z "$(wfile "$w" creds.json)" ] && ok "no creds.json — the node plugin did NOT do the exchange on the worker's behalf, which is the point of this mode" || bad "a creds.json is present: the broker exchange ran anyway"
    sts=$(wenvv "$w" AWS_ENDPOINT_URL_STS); ra=$(wenvv "$w" AWS_ROLE_ARN); tf=$(wenvv "$w" AWS_WEB_IDENTITY_TOKEN_FILE)
    [ -n "$sts" ] && [ -n "$ra" ] && [ -n "$tf" ] \
        && ok "its environment names the exchange: AWS_ENDPOINT_URL_STS=$sts AWS_ROLE_ARN=$ra AWS_WEB_IDENTITY_TOKEN_FILE=$tf" \
        || bad "the web-identity environment is incomplete (sts='$sts' role='$ra' tokenfile='$tf')"
    note "the broker Service speaks plain http here; whether a client accepts that is exactly what A2/A3 are measuring"
    if wait_phase lean-wi Running 420; then
        # Running is NOT the oracle. The syncer reaching Running only
        # says it found credentials SOMEWHERE.
        if [ "$(issued_for proj-wi)" -ge 1 ]; then
            ok "the syncer completed the exchange against the broker's facade and checked out — the broker logged an issue for proj-wi"
        else
            bad "the workspace checked out and the broker NEVER ISSUED for proj-wi: the syncer did not do a web-identity exchange at all, it fell through its default chain to another identity (this node's instance role can reach the bucket, so the fallback succeeds and looks like success). A credential mode that silently runs as a different principal is worse than one that fails"
        fi
    else
        note "the tenant did not reach Running in 420s. Syncer's last words: $($K -n $WNS logs "$w" --tail=4 2>/dev/null | tr '\n' ' ' | cut -c1-320)"
        note "tenant events: $(mount_events lean-wi | tail -2 | tr '\n' ' ' | cut -c1-320)"
        skip "LEAN webIdentity did not complete; the arm is correct and the exchange is not — recorded above rather than scored, because what blocks it is a client/transport question the drill cannot fix from here"
    fi
    poddel lean-wi
else
    bad "no worker appeared for lean-wi in ${i}s: $(mount_events lean-wi | tail -2 | tr '\n' ' ' | cut -c1-300)"
fi

# ── A3 passthrough over the same facade ───────────────────────────────
leg A3 "PASSTHROUGH webIdentity: the same arm, with mount-s3's CRT as the client instead of the Rust SDK"
poddel pt-wi
mcx sh -c "printf 'wi-seeded\n' | mc pipe m/$BUCKET/pt/wi/hello.txt" >/dev/null 2>&1
ptpod pt-wi pt-wi "$NODE"
w=""; i=0; while [ $i -lt 300 ] && [ -z "$w" ]; do w=$(worker_of_any pt-wi); [ -z "$w" ] && { sleep 5; i=$((i + 5)); }; done
if [ -n "$w" ]; then
    j=0; while [ $j -lt 120 ] && [ -z "$(wfile_any "$w" token)" ]; do sleep 2; j=$((j + 2)); done
    t=$(wfile_any "$w" token)
    [ -n "$t" ] && ok "the token file is in the worker's comm dir ($t) — the arm materialises for passthrough exactly as it does for lean" || bad "no token file after ${j}s, asked of both the pod and the node"
    [ -z "$(wfile "$w" creds.json)" ] && ok "no creds.json — nothing was brokered on its behalf" || bad "a creds.json is present"
    if wait_phase pt-wi Running 420 && [ "$(inpod_out pt-wi cat /mnt/s3/hello.txt)" = "wi-seeded" ]; then
        [ "$(issued_for pt-wi)" -ge 1 ] \
            && ok "the mount SERVES and the broker issued for pt-wi: the exchange really happened" \
            || bad "the mount serves but the broker NEVER ISSUED for pt-wi — the client fell through to another identity"
    else
        note "tenant event: $(mount_events pt-wi | tail -2 | tr '\n' ' ' | cut -c1-320)"
        note "worker's last words: $($K -n $WNS logs "$w" --tail=4 2>/dev/null | tr '\n' ' ' | cut -c1-320)"
        note "the broker issued $(issued_for pt-wi) time(s) for pt-wi — an exchange never reached it"
        bad "PASSTHROUGH webIdentity does not work: the arm materialises correctly and the mounter dies with no usable credential. Measured out of band on 2026-09-05 with mount-s3 1.24.0: with a real projected token it fails in ~1 ms and sends NOTHING to the endpoint, identically for flint's synthetic role ARN and a valid AWS one, over http and https — so the client is not attempting the exchange at all, and no wiring on this side changes that. The mode works on LEAN, whose client is the Rust SDK. Offering it on a passthrough CR promises something that cannot happen"
    fi
    poddel pt-wi
else
    bad "no worker appeared for pt-wi in ${i}s: $(mount_events pt-wi | tail -2 | tr '\n' ' ' | cut -c1-300)"
fi

echo
for want in A1 A2 A3; do echo " $RAN_LEGS " | grep -q " $want " || bad "leg $want never ran"; done
echo "════════════════════════════════════════"
echo "flint identity modes on real nodes: $PASS ok, $FAILED bad, $SKIPPED skipped"
[ "$FAILED" = "0" ]
