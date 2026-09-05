#!/usr/bin/env bash
# Per-pod rights on ONE project: two pods, one FlintPassthroughMount,
# one of them declaring its volume `readOnly`. The deck's security
# plate (row "Can two sessions differ?") and the radar's Rights axis
# say "yes, per pod" for passthrough on the strength of node.rs:425
# (`req.readonly || spec.read_only`) — but the suite only ever tested
# the CR-level flag (S5, a second CR with `readOnly: true`). This is
# the pod-level path, on the wire, with the anti-vacuity shape the
# suite uses everywhere else: the two pods differ in exactly one field,
# the read-write twin proves the oracle can see a write, and a third
# pod that is the read-only spec MINUS the flag proves the flag is
# what refuses.
#
#   CTX=kind-flint-s3csi ./run-s3csi.sh setup     # first
#   CTX=kind-flint-s3csi ./run-rights.sh          # then this
set -u
cd "$(dirname "$0")"
CTX=${CTX:-kind-flint-s3csi}
K="kubectl --context $CTX"
NS=s3-tenants; WNS=flint-workers; SYS=flint-system
BUCKET=${BUCKET:-s3bucket}
CR=datasets                     # tenants.yaml: rw, consumers [trainer], prefix datasets/imagenet
PREFIX=datasets/imagenet
RUN=$(date +%s)
PASS=0; FAILED=0; RAN_LEGS=""
bad()  { echo "  BAD: $1"; FAILED=$((FAILED + 1)); }
ok()   { PASS=$((PASS + 1)); echo "  ok: $1"; }
note() { echo "  NOTE: $1"; }
leg()  { RAN_LEGS="$RAN_LEGS $1"; echo; echo "── $1 — $2"; }

inpod()     { local p=$1 try out rc; shift; for try in 1 2 3; do out=$($K -n $NS exec "$p" -c agent -- /bin/sh -c "$*" 2>/dev/null); rc=$?; [ $rc -eq 0 ] && break; sleep 2; done; [ -n "$out" ] && printf '%s\n' "$out"; return $rc; }
# Combined output AND the exit status, once: the refusal legs need the
# error text (EROFS reads "Read-only file system").
# The subshell matters: for `echo x > f`, the failure is the SHELL's
# redirection, and its message goes to the shell's own stderr — which a
# `2>&1` on the command does not cover. Without the parentheses the
# first run reported "rc=1" and no text.
inpod_err() { local p=$1; shift; $K -n $NS exec "$p" -c agent -- /bin/sh -c "( $* ) 2>&1; echo rc=\$?" 2>/dev/null; }
mcx()       { $K -n $SYS exec mc-s3 -- "$@" 2>/dev/null; }
wait_phase() { local i=0; while [ $i -lt "$3" ]; do [ "$($K -n $NS get pod "$1" -o jsonpath='{.status.phase}' 2>/dev/null)" = "$2" ] && return 0; sleep 2; i=$((i + 2)); done; return 1; }
worker_of() {
    $K -n $WNS get pods -o json 2>/dev/null | python3 -c "
import json,sys
want='$NS/$1'
for p in json.load(sys.stdin)['items']:
    if p['metadata'].get('annotations',{}).get('chert.us/tenant-pod')==want and p.get('status',{}).get('phase')=='Running' and not p['metadata'].get('deletionTimestamp'):
        print(p['metadata']['name']); break"
}
# The mount-s3 command line inside a worker, from /proc: the flag the
# plugin appended (node.rs:522) is visible there or nowhere.
mount_args() { $K -n $WNS exec "$1" -- /bin/sh -c 'for p in /proc/[0-9]*; do tr "\0" " " < $p/cmdline 2>/dev/null; echo; done' 2>/dev/null | grep -m1 -E '(^|/| )mount-s3( |$)'; }
worker_key() { $K -n $WNS exec "$1" -- /bin/sh -c 'cat /comm/creds.json' 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)['AccessKeyId'])" 2>/dev/null; }

pod() { # name readonly(true|false)
    local ro=""; [ "$2" = true ] && ro="        readOnly: true"
    cat <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $1
  namespace: $NS
  labels: { drill: rights }
spec:
  serviceAccountName: trainer
  securityContext:
    runAsNonRoot: true
    runAsUser: 1001
    seccompProfile: { type: RuntimeDefault }
  volumes:
    - name: data
      csi:
        driver: s3.csi.chert.us
${ro:+$ro
}        volumeAttributes:
          chert.us/mount: $CR
  containers:
    - name: agent
      image: busybox:1.36
      command: ["/bin/sh", "-c"]
      args: ["trap 'exit 0' TERM INT; sleep 86400 & wait"]
      securityContext:
        allowPrivilegeEscalation: false
        capabilities: { drop: [ALL] }
      volumeMounts:
        - { name: data, mountPath: /mnt/s3 }
EOF
}

echo "per-pod rights on one project — context $CTX, CR $NS/$CR"
$K -n $NS delete pod -l drill=rights --ignore-not-found --wait=true --timeout=120s >/dev/null 2>&1
mcx mc rm --force "m/$BUCKET/$PREFIX/from-rw-$RUN.txt" >/dev/null 2>&1
{ pod rights-rw false; echo ---; pod rights-ro true; } | $K apply -f - >/dev/null

# ── R1 ───────────────────────────────────────────────────────────────
leg R1 "two pods on ONE CR, the second declaring its volume readOnly: both Running, a worker each"
wait_phase rights-rw Running 180 && ok "rights-rw is Running" || bad "rights-rw: $($K -n $NS get pod rights-rw -o jsonpath='{.status.phase}') — $($K -n $NS get events --field-selector involvedObject.name=rights-rw -o jsonpath='{.items[-1].message}' 2>/dev/null | cut -c1-160)"
wait_phase rights-ro Running 180 && ok "rights-ro is Running" || bad "rights-ro: $($K -n $NS get pod rights-ro -o jsonpath='{.status.phase}') — $($K -n $NS get events --field-selector involvedObject.name=rights-ro -o jsonpath='{.items[-1].message}' 2>/dev/null | cut -c1-160)"
WRW=$(worker_of rights-rw); WRO=$(worker_of rights-ro)
[ -n "$WRW" ] && [ -n "$WRO" ] && [ "$WRW" != "$WRO" ] && ok "one worker per pod: $WRW / $WRO" || bad "workers: rw='$WRW' ro='$WRO'"

# ── R2 ───────────────────────────────────────────────────────────────
leg R2 "the two pod specs differ in exactly one field: the volume's readOnly"
a=$($K -n $NS get pod rights-rw -o json | jq -c '.spec | {sa: .serviceAccountName, sc: .securityContext, csi: (.volumes[0].csi | del(.readOnly))}')
b=$($K -n $NS get pod rights-ro -o json | jq -c '.spec | {sa: .serviceAccountName, sc: .securityContext, csi: (.volumes[0].csi | del(.readOnly))}')
[ "$a" = "$b" ] && ok "same ServiceAccount, same securityContext, same CSI driver and CR: $b" || bad "specs differ beyond readOnly: $a vs $b"
ro_rw=$($K -n $NS get pod rights-rw -o jsonpath='{.spec.volumes[0].csi.readOnly}'); ro_ro=$($K -n $NS get pod rights-ro -o jsonpath='{.spec.volumes[0].csi.readOnly}')
[ -z "$ro_rw" ] && [ "$ro_ro" = true ] && ok "readOnly: rw='' ro=true" || bad "readOnly: rw='$ro_rw' ro='$ro_ro'"

# ── R3 ───────────────────────────────────────────────────────────────
leg R3 "the read-only pod's worker runs mount-s3 with --read-only; the read-write pod's does not"
if [ -n "$WRW" ] && [ -n "$WRO" ]; then
    arw=$(mount_args "$WRW"); aro=$(mount_args "$WRO")
    [ -n "$arw" ] && ok "rw worker's mount-s3 found: $(echo "$arw" | cut -c1-120)…" || bad "no mount-s3 in the rw worker's /proc"
    echo "$aro" | grep -q -- '--read-only' && ok "ro worker's mount-s3 carries --read-only" || bad "ro worker's mount-s3 lacks --read-only: $(echo "$aro" | cut -c1-160)"
    echo "$arw" | grep -q -- '--read-only' && bad "rw worker's mount-s3 ALSO carries --read-only" || ok "rw worker's mount-s3 does not"
fi

# ── R4 ───────────────────────────────────────────────────────────────
leg R4 "CONTROL: the read-write pod's write lands in the bucket — the oracle can see a write"
inpod rights-rw "echo written-by-rw-$RUN > /mnt/s3/from-rw-$RUN.txt" && ok "rw write accepted" || bad "rw write refused"
sleep 2
got=$(mcx mc cat "m/$BUCKET/$PREFIX/from-rw-$RUN.txt")
[ "$got" = "written-by-rw-$RUN" ] && ok "the object is in the bucket with the written bytes" || bad "bucket object: '$got'"

# ── R5 ───────────────────────────────────────────────────────────────
leg R5 "the read-only pod reads the read-write pod's file: one project, not two mounts"
got=""; for i in 1 2 3 4 5 6 7 8 9 10; do got=$(inpod rights-ro "cat /mnt/s3/from-rw-$RUN.txt"); [ "$got" = "written-by-rw-$RUN" ] && break; sleep 3; done
[ "$got" = "written-by-rw-$RUN" ] && ok "ro pod read the rw pod's bytes (after $i tries)" || bad "ro pod read: '$got'"
got=$(inpod rights-ro "cat /mnt/s3/shard-03.txt")
[ "$got" = "seeded-object-03" ] && ok "and the seeded content, as uid 1001" || bad "seeded read: '$got'"

# ── R6 ───────────────────────────────────────────────────────────────
leg R6 "the read-only pod's write, unlink and mkdir are refused, and the bucket does not change"
out=$(inpod_err rights-ro "echo x > /mnt/s3/ro-attempt-$RUN.txt")
echo "$out" | grep -q 'rc=0' && bad "ro write ACCEPTED: $out" || ok "ro write refused: $(echo "$out" | head -1 | cut -c1-100)"
echo "$out" | grep -qi 'read-only file system' && ok "the refusal is EROFS" || note "refusal text: $(echo "$out" | tr '\n' ' ' | cut -c1-140)"
out=$(inpod_err rights-ro "rm /mnt/s3/from-rw-$RUN.txt")
echo "$out" | grep -q 'rc=0' && bad "ro unlink ACCEPTED" || ok "ro unlink refused"
out=$(inpod_err rights-ro "mkdir /mnt/s3/ro-dir-$RUN")
echo "$out" | grep -q 'rc=0' && bad "ro mkdir ACCEPTED" || ok "ro mkdir refused"
sleep 2
mcx mc stat "m/$BUCKET/$PREFIX/ro-attempt-$RUN.txt" >/dev/null 2>&1 && bad "ro-attempt reached the bucket" || ok "nothing landed from the ro pod"
got=$(mcx mc cat "m/$BUCKET/$PREFIX/from-rw-$RUN.txt")
[ "$got" = "written-by-rw-$RUN" ] && ok "the rw pod's object survived the ro pod's unlink" || bad "rw object after ro unlink: '$got'"

# ── R7 ───────────────────────────────────────────────────────────────
leg R7 "both workers hold the SAME key: the flag is the only thing separating the two sessions"
if [ -n "$WRW" ] && [ -n "$WRO" ]; then
    krw=$(worker_key "$WRW"); kro=$(worker_key "$WRO")
    [ -n "$krw" ] && [ "$krw" = "$kro" ] && ok "same AccessKeyId in both workers' creds.json ($krw): one scope, two pods" || bad "keys: rw='$krw' ro='$kro'"
fi

# ── R8 ───────────────────────────────────────────────────────────────
leg R8 "the read-only pod holds no credential and no token: read-only is not a weaker key, it is no key"
env_hits=$(inpod rights-ro "tr '\0' '\n' < /proc/1/environ | grep -c -E '^AWS_|SECRET|TOKEN'" ); [ "${env_hits:-0}" = 0 ] && ok "no credential-shaped env var in the ro pod" || bad "ro pod env has $env_hits credential-shaped vars"
inpod rights-ro "test -d /var/run/secrets/kubernetes.io/serviceaccount" && note "the default SA token IS mounted (kube API only; audience is the apiserver's, S7 proves the broker refuses even the right audience without a registration)" || ok "no SA token mounted"

# ── R9 ───────────────────────────────────────────────────────────────
leg R9 "CONTROL: the read-only pod's spec with the flag REMOVED writes — the flag is what refuses"
pod rights-flip false | $K apply -f - >/dev/null
wait_phase rights-flip Running 180 && ok "rights-flip (ro spec minus readOnly) is Running" || bad "rights-flip: $($K -n $NS get pod rights-flip -o jsonpath='{.status.phase}')"
c=$($K -n $NS get pod rights-flip -o json | jq -c '.spec | {sa: .serviceAccountName, sc: .securityContext, csi: (.volumes[0].csi | del(.readOnly))}')
[ "$c" = "$b" ] && ok "rights-flip differs from rights-ro only in readOnly" || bad "rights-flip spec: $c"
inpod rights-flip "echo written-by-flip-$RUN > /mnt/s3/from-flip-$RUN.txt" && ok "rights-flip's write accepted" || bad "rights-flip's write refused"
sleep 2
got=$(mcx mc cat "m/$BUCKET/$PREFIX/from-flip-$RUN.txt")
[ "$got" = "written-by-flip-$RUN" ] && ok "and it landed in the bucket" || bad "flip object: '$got'"

# ── roster + cleanup ─────────────────────────────────────────────────
echo
for l in R1 R2 R3 R4 R5 R6 R7 R8 R9; do case " $RAN_LEGS " in *" $l "*) ;; *) bad "leg $l never ran";; esac; done
mcx mc rm --force "m/$BUCKET/$PREFIX/from-rw-$RUN.txt" "m/$BUCKET/$PREFIX/from-flip-$RUN.txt" >/dev/null 2>&1
$K -n $NS delete pod -l drill=rights --ignore-not-found --wait=false >/dev/null 2>&1
echo "══ $PASS ok, $FAILED bad ══"
[ "$FAILED" = 0 ]
