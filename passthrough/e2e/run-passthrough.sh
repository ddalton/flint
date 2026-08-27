#!/usr/bin/env bash
# flint-passthrough, end to end on kind.
#
# The question this rig answers: does an S3 prefix actually appear as a
# directory INSIDE THE APP CONTAINER — not inside the sidecar, which is
# the easy half and the one a careless rig would test. A FUSE mount made
# in a sidecar reaches its siblings only through `mountPropagation:
# Bidirectional`, and every assertion here that matters is made from the
# unprivileged reader, over bytes that were written to the bucket before
# the pod existed.
#
# House rules inherited from lean/e2e: every leg observes its own
# PRECONDITION or FAILS, every refusal has an accepted control, and NO
# LEG MAY PASS BY NOT LOOKING. The structural guards are:
#   1. Readers run as uid 1001 and never as root, so a mount only root
#      can traverse fails rather than passing.
#   2. A2's falsifiability fixture (leg A2b) mounts the SAME bucket at a
#      DIFFERENT prefix. A reader that were somehow seeing a local
#      directory, the bucket root, or another pod's mount would show the
#      same eleven files in both; it must show one, with other bytes.
#   3. Every read leg asserts CONTENT, never a file count alone.
#
#   CTX=kind-<cluster> ./run-passthrough.sh setup    # MinIO + seed + CRD
#   CTX=kind-<cluster> ./run-passthrough.sh          # the legs
#   CTX=kind-<cluster> ./run-passthrough.sh teardown
set -u
cd "$(dirname "$0")"

CTX=${CTX:-kind-lean-release-test}
K="kubectl --context $CTX"
NS=pt-agents
BUCKET=ptbucket
PREFIX=datasets/imagenet

PASS=0
FAILED=0
RAN_LEGS=""
bad()  { echo "  BAD: $1"; FAILED=$((FAILED + 1)); }
ok()   { PASS=$((PASS + 1)); echo "  ok: $1"; }
note() { echo "  NOTE: $1"; }
leg()  { RAN_LEGS="$RAN_LEGS $1"; echo; echo "── $1 — $2"; }

# Every leg that observes a pod must first prove the pod is running.
# Otherwise `kubectl exec` returns the empty string and a leg that
# compares against "" passes by not looking.
require_pod() {
    local ph
    ph=$($K -n $NS get pod "$1" -o jsonpath='{.status.phase}' 2>/dev/null)
    [ "$ph" = "Running" ] && return 0
    bad "pod $1 is '${ph:-absent}', not Running — every observation this leg makes would be an empty string"
    return 1
}
inpod() { local p=$1; shift; $K -n $NS exec "$p" -c agent -- /bin/sh -c "$*" 2>/dev/null; }
mcx()   { $K -n flint-system exec mc-pt -- "$@" 2>/dev/null; }

# ── setup / teardown ─────────────────────────────────────────────────
if [ "${1:-}" = "setup" ]; then
    $K apply -f rig.yaml
    $K apply -f ../../flint-passthrough-chart/crds/flintpassthroughmounts.yaml
    echo "waiting for MinIO + seed…"
    $K -n flint-system rollout status deploy/minio --timeout=180s
    $K -n flint-system wait --for=condition=complete job/seed-bucket --timeout=180s
    $K -n flint-system wait --for=condition=ready pod/mc-pt --timeout=120s
    echo "seeded:"; mcx mc ls --recursive "m/$BUCKET/"
    exit $?
fi
if [ "${1:-}" = "teardown" ]; then
    $K delete -f mounts.yaml --ignore-not-found --wait=false
    $K delete -f rig.yaml --ignore-not-found --wait=false
    exit 0
fi

echo "flint-passthrough e2e — context $CTX"

# ── A1 preconditions ─────────────────────────────────────────────────
leg A1 "the webhook is registered and the bucket is seeded"
fp=$($K get mutatingwebhookconfiguration flint-passthrough-inject \
        -o jsonpath='{.webhooks[0].failurePolicy}' 2>/dev/null)
[ "$fp" = "Fail" ] && ok "registration exists with failurePolicy=Fail" \
                   || bad "failurePolicy is '${fp:-absent}', want Fail"
sel=$($K get mutatingwebhookconfiguration flint-passthrough-inject \
        -o jsonpath='{.webhooks[0].objectSelector.matchExpressions[0].key}' 2>/dev/null)
[ "$sel" = "flint.io/passthrough-mount" ] && ok "objectSelector keys on the opt-in label" \
                   || bad "objectSelector key is '${sel:-absent}'"
rdy=$($K -n flint-system get deploy flint-passthrough -o jsonpath='{.status.readyReplicas}' 2>/dev/null)
[ "${rdy:-0}" -ge 1 ] && ok "webhook has $rdy ready replica(s)" || bad "no ready webhook replica"
seeded=$(mcx mc ls --recursive "m/$BUCKET/$PREFIX/" | grep -c .)
[ "$seeded" -eq 11 ] && ok "bucket carries 11 seeded objects under $PREFIX" \
                     || bad "seed is $seeded objects, want 11 — reseed before trusting any read leg"

# ── A2 the headline ──────────────────────────────────────────────────
leg A2 "an unprivileged app container reads bucket bytes it did not write"
if require_pod reader; then
    fstype=$(inpod reader "awk '\$2==\"/mnt/s3\"{print \$3}' /proc/mounts")
    case "$fstype" in
        fuse*) ok "the app container's /mnt/s3 is a $fstype mount, not a directory" ;;
        *)     bad "/mnt/s3 fstype is '${fstype:-nothing}' — propagation did not reach the app container" ;;
    esac
    n=$(inpod reader "ls /mnt/s3 | wc -l")
    [ "${n:-0}" -eq 11 ] && ok "sees 11 entries (10 shards + sub/)" || bad "sees ${n:-0} entries, want 11"
    body=$(inpod reader "cat /mnt/s3/shard-05.txt")
    [ "$body" = "seeded-object-05" ] && ok "shard-05.txt reads its seeded bytes" \
                                     || bad "shard-05.txt read '${body:-nothing}'"
    deep=$(inpod reader "cat /mnt/s3/sub/deep.txt")
    [ "$deep" = "deep-seeded" ] && ok "a nested key reads through as a subdirectory" \
                                || bad "sub/deep.txt read '${deep:-nothing}'"
    else_seen=$(inpod reader "ls /mnt/s3 | grep -c elsewhere")
    [ "${else_seen:-1}" -eq 0 ] && ok "keyPrefix scopes the mount — 'elsewhere/' is not visible" \
                               || bad "the mount shows keys outside its prefix"
fi

# ── A2b falsifiability ───────────────────────────────────────────────
leg A2b "the same bucket at another prefix shows other bytes"
if require_pod reader-else; then
    n=$(inpod reader-else "ls /mnt/s3 | wc -l")
    body=$(inpod reader-else "cat /mnt/s3/secret.txt")
    if [ "${n:-0}" -eq 1 ] && [ "$body" = "must-not-be-visible" ]; then
        ok "reads 1 file with its own bytes — A2 was reading its CR's prefix, not a directory"
    else
        bad "saw ${n:-0} files / '${body:-nothing}' — A2's result is not attributable to its prefix"
    fi
fi

# ── A3 the injected shape ────────────────────────────────────────────
leg A3 "the mutation is the shape the design requires"
$K -n $NS get pod reader -o json > /tmp/pt-reader.json 2>/dev/null
python3 - <<'PY' > /tmp/pt-a3.txt 2>&1
import json
p = json.load(open("/tmp/pt-reader.json"))
s = p["spec"]
out = []
ic = s.get("initContainers") or []
side = next((c for c in ic if c["name"] == "flint-passthrough"), None)
if not side:
    out.append("BAD no flint-passthrough initContainer")
else:
    out.append(("ok " if ic[0]["name"] == "flint-passthrough" else "BAD ") + "sidecar is initContainers[0]")
    out.append(("ok " if side.get("restartPolicy") == "Always" else "BAD ") + "restartPolicy=Always (native sidecar)")
    out.append(("ok " if side.get("securityContext", {}).get("privileged") else "BAD ") + "sidecar is privileged")
    m = side["volumeMounts"][0]
    out.append(("ok " if m.get("mountPropagation") == "Bidirectional" else "BAD ") + "sidecar mount is Bidirectional")
    script = side["command"][2]
    leaked = [t for t in ("ptbucket", "datasets/imagenet", "minio.flint-system") if t in script]
    out.append(("ok " if not leaked else "BAD ") + f"no CR data in the command script (leaked={leaked})")
    out.append(("ok " if any("ptbucket" in a for a in side["args"]) else "BAD ") + "CR data arrives as argv")
app = next(c for c in s["containers"] if c["name"] == "agent")
# By mountPath, NOT by index: the ServiceAccount admission plugin runs
# BEFORE mutating webhooks, so volumeMounts[0] is the projected token
# and an index-based check reads the wrong mount.
am = [m for m in app.get("volumeMounts", []) if m["mountPath"] == "/mnt/s3"]
out.append(("ok " if am and am[0].get("mountPropagation") == "HostToContainer" else "BAD ") + "app mount is HostToContainer")
# The mount lives one level below the volume root — mount-s3 refuses
# kubelet's bind as a target, and more importantly a dead mount AT the
# root wedges every later container creation in the runtime. The
# consumer reaches it with subPath, and a mount that ever moved back
# to the root would show up here rather than as a 3am wedge.
out.append(("ok " if am and am[0].get("subPath") == "root" else "BAD ") + f"consumer reaches it through subPath=root (got {am[0].get('subPath') if am else None})")
out.append(("ok " if not app.get("securityContext", {}).get("privileged") else "BAD ") + "app container gains no privilege")
print("\n".join(out))
PY
while IFS= read -r line; do
    case "$line" in ok*) ok "${line#ok }" ;; *) bad "${line#BAD }" ;; esac
done < /tmp/pt-a3.txt

# ── A4 a pod naming no CR is refused ─────────────────────────────────
leg A4 "a pod opting into a mount that does not exist is refused"
ghost=$($K -n $NS run ghost --image=busybox:1.36 \
          --labels=flint.io/passthrough-mount=nosuchmount \
          --restart=Never --command -- sleep 60 2>&1)
case "$ghost" in
    *"no such FlintPassthroughMount"*) ok "refused, and the message names the missing CR" ;;
    *) bad "expected a refusal naming the CR, got: $(echo "$ghost" | head -1)" ;;
esac
$K -n $NS delete pod ghost --ignore-not-found --wait=false >/dev/null 2>&1
# The accepted control: identical pod, a label that resolves.
ctl=$($K -n $NS run ctl --image=busybox:1.36 \
        --labels=flint.io/passthrough-mount=datasets \
        --restart=Never --command -- sleep 60 2>&1)
case "$ctl" in
    *created*) ok "control: the same pod with a resolvable label IS admitted" ;;
    *) bad "control pod was refused too — A4 proves nothing: $(echo "$ctl" | head -1)" ;;
esac
$K -n $NS delete pod ctl --ignore-not-found --wait=false >/dev/null 2>&1

# ── A5 a mountPath collision is refused ──────────────────────────────
leg A5 "a pod that already uses the mount path is refused by name"
col=$($K -n $NS apply -f - 2>&1 <<'EOF'
apiVersion: v1
kind: Pod
metadata:
  name: collide
  namespace: pt-agents
  labels: { "flint.io/passthrough-mount": datasets }
spec:
  containers:
    - name: agent
      image: busybox:1.36
      command: ["sleep", "60"]
      volumeMounts: [{ name: scratch, mountPath: /mnt/s3 }]
  volumes: [{ name: scratch, emptyDir: {} }]
EOF
)
case "$col" in
    *"already mounts"*) ok "refused for the collision" ;;
    *) bad "expected a collision refusal, got: $(echo "$col" | head -1)" ;;
esac
case "$col" in
    *scratch*) ok "the message names the offending volume" ;;
    *) bad "the message does not name the volume the author has to move" ;;
esac
case "$col" in
    *mountPath*) ok "the message names the knob that fixes it" ;;
    *) bad "the message does not name the knob" ;;
esac

# ── A6 read-only ─────────────────────────────────────────────────────
leg A6 "a readOnly mount still reads, and refuses a write"
if require_pod reader-ro; then
    body=$(inpod reader-ro "cat /mnt/ro/shard-01.txt")
    [ "$body" = "seeded-object-01" ] && ok "readOnly mount propagates and reads" \
                                     || bad "readOnly mount read '${body:-nothing}' — a readOnly consumer mount may be blocking propagation"
    w=$(inpod reader-ro "echo x > /mnt/ro/nope.txt 2>&1; echo rc=\$?")
    case "$w" in
        *rc=0) bad "a write to a readOnly mount SUCCEEDED" ;;
        *)     ok "the write is refused ($(echo "$w" | tr '\n' ' '))" ;;
    esac
    gone=$(mcx mc ls "m/$BUCKET/$PREFIX/nope.txt" | grep -c .)
    [ "${gone:-1}" -eq 0 ] && ok "and nothing reached the bucket" || bad "the refused write still landed in the bucket"
fi

# ── A7 write-through ─────────────────────────────────────────────────
leg A7 "a write in the app container becomes an object in the bucket"
if require_pod reader; then
    inpod reader "echo through-the-mount > /mnt/s3/written.txt" >/dev/null
    sleep 2
    got=$(mcx mc cat "m/$BUCKET/$PREFIX/written.txt")
    if [ "$got" = "through-the-mount" ]; then
        ok "the object is in the bucket with the written bytes"
        # ANTI-VACUITY: the unlink assertion below is "mc ls finds
        # nothing", which is trivially true when the write never
        # happened. Only run it against an object we just proved exists.
        inpod reader "rm -f /mnt/s3/written.txt" >/dev/null
        sleep 2
        left=$(mcx mc ls "m/$BUCKET/$PREFIX/written.txt" | grep -c .)
        [ "${left:-1}" -eq 0 ] && ok "and unlink removes the object" || bad "the object survived unlink"
    else
        bad "bucket has '${got:-nothing}' — this is not passthrough"
        bad "unlink is untested: it would assert an object's ABSENCE, which is already true"
    fi
fi

# ── A8 an unlabelled pod is untouched ────────────────────────────────
leg A8 "a pod with no label is not mutated at all"
if require_pod nolabel; then
    ic=$($K -n $NS get pod nolabel -o jsonpath='{.spec.initContainers}' 2>/dev/null)
    [ -z "$ic" ] && ok "no initContainers were added" || bad "an unlabelled pod was mutated: $ic"
    vol=$($K -n $NS get pod nolabel -o json | grep -c flint-passthrough)
    [ "${vol:-1}" -eq 0 ] && ok "no passthrough volume was added" || bad "a passthrough volume appeared"
fi

# ── A9 the field that no longer exists ───────────────────────────────
leg A9 "a CR asking for the removed s3fs driver is refused, not silently mounted"
# There is one mounter now. The danger in removing a field is not that
# the CR breaks — it is that the API server PRUNES the unknown field
# and mounts something with a different write model than the author
# asked for, silently. Assert the refusal, and assert its control.
r=$($K apply -f - 2>&1 <<'EOF'
apiVersion: flint.io/v1alpha1
kind: FlintPassthroughMount
metadata: { name: legacy, namespace: pt-agents }
spec:
  bucket: ptbucket
  endpoint: http://minio.flint-system.svc:9000
  credentialsSecretRef: minio-creds
  driver: s3fs
EOF
)
case "$r" in
    *created*|*configured*)
        # The failure this leg exists for: the API server PRUNED the
        # unknown field and stored a mount whose write model is not the
        # one the author asked for. The tombstone in the CRD is what
        # stops it, so a green apply here means the tombstone is gone.
        bad "spec.driver was PRUNED and the CR stored — a mount with a write model nobody chose"
        $K -n $NS delete flintpassthroughmount legacy --ignore-not-found >/dev/null 2>&1 ;;
    *driver*) ok "rejected at apply time, naming driver" ;;
    *) bad "expected a rejection naming driver, got: $(echo "$r" | head -2 | tr '\n' ' ')" ;;
esac
# The control: the same CR WITHOUT the removed field is accepted, so
# the leg above is attributable to `driver` and not to anything else
# in the document.
r2=$($K apply -f - 2>&1 <<'EOF'
apiVersion: flint.io/v1alpha1
kind: FlintPassthroughMount
metadata: { name: legacy, namespace: pt-agents }
spec:
  bucket: ptbucket
  endpoint: http://minio.flint-system.svc:9000
  credentialsSecretRef: minio-creds
EOF
)
case "$r2" in
    *created*|*configured*) ok "control: the same CR without spec.driver IS accepted" ;;
    *) bad "control rejected too — A9 is not attributable to driver: $(echo "$r2" | head -1)" ;;
esac
$K -n $NS delete flintpassthroughmount legacy --ignore-not-found >/dev/null 2>&1

# ── A10 the s3fs-shaped mount option that outlived s3fs ──────────────
leg A10 "an s3fs mountOption is refused at admission, not left crash-looping"
# The migration failure, and it is not hypothetical: a CR written before
# the driver was removed carries `-o something`, mount-s3 has no -o, and
# unrefused the result is a PRIVILEGED sidecar in CrashLoopBackOff whose
# reason exists only in a container log. Observed on this rig 2026-08-27.
$K apply -f - >/dev/null 2>&1 <<'EOF'
apiVersion: flint.io/v1alpha1
kind: FlintPassthroughMount
metadata: { name: oldopts, namespace: pt-agents }
spec:
  bucket: ptbucket
  keyPrefix: datasets/imagenet
  endpoint: http://minio.flint-system.svc:9000
  credentialsSecretRef: minio-creds
  mountOptions: ["-o", "auto_unmount"]
EOF
o=$($K -n $NS run oldoptspod --image=busybox:1.36 \
      --labels=flint.io/passthrough-mount=oldopts \
      --restart=Never --command -- sleep 60 2>&1)
case "$o" in
    *mountOptions*s3fs*) ok "the pod is refused, naming mountOptions and s3fs" ;;
    *created*|*Running*) bad "the pod was ADMITTED with an s3fs option — it will crash-loop privileged" ;;
    *)                   bad "refused for some other reason: $(echo "$o" | head -1)" ;;
esac
$K -n $NS delete pod oldoptspod --ignore-not-found --wait=false >/dev/null 2>&1
# The control: the SAME CR with a mount-s3-shaped option is admitted, so
# the refusal is attributable to the option's shape and not to the
# presence of mountOptions at all.
$K apply -f - >/dev/null 2>&1 <<'EOF'
apiVersion: flint.io/v1alpha1
kind: FlintPassthroughMount
metadata: { name: oldopts, namespace: pt-agents }
spec:
  bucket: ptbucket
  keyPrefix: datasets/imagenet
  endpoint: http://minio.flint-system.svc:9000
  credentialsSecretRef: minio-creds
  mountOptions: ["--metadata-ttl", "60"]
EOF
o2=$($K -n $NS run newoptspod --image=busybox:1.36 \
       --labels=flint.io/passthrough-mount=oldopts \
       --restart=Never --command -- sleep 60 2>&1)
case "$o2" in
    *created*) ok "control: a --long mount-s3 option IS admitted" ;;
    *)         bad "control refused too — A10 is not attributable to the -o shape: $(echo "$o2" | head -1)" ;;
esac
$K -n $NS delete pod newoptspod --ignore-not-found --wait=false >/dev/null 2>&1
$K -n $NS delete flintpassthroughmount oldopts --ignore-not-found >/dev/null 2>&1

# ── A11 the documented cost, proven ──────────────────────────────────
leg A11 "a PodSecurity-baseline namespace rejects the privileged sidecar"
psa=$($K -n pt-restricted run psa --image=busybox:1.36 \
        --labels=flint.io/passthrough-mount=datasets \
        --restart=Never --command -- sleep 60 2>&1)
case "$psa" in
    *privileged*) ok "rejected by PodSecurity, naming privileged — the cost is real and enforced" ;;
    *"no such FlintPassthroughMount"*) note "rejected earlier, by the webhook: the CR is namespaced and pt-restricted has none" ;;
    *) bad "expected a PodSecurity rejection, got: $(echo "$psa" | head -1)" ;;
esac
$K -n pt-restricted delete pod psa --ignore-not-found --wait=false >/dev/null 2>&1

# ── A12 the mounter dies ─────────────────────────────────────────────
leg A12 "a mounter that dies strands its consumers — and the pod says so"
NODE=${NODE:-${CTX#kind-}-control-plane}
if require_pod reader; then
    before=$(inpod reader "cat /mnt/s3/shard-02.txt")
    [ "$before" = "seeded-object-02" ] && ok "precondition: the mount serves before the kill" \
                                       || bad "precondition failed — nothing below is attributable"
    # NOT `kill -9 1`: the mounter IS pid 1 of its container and the
    # kernel discards signals sent to a namespace's init from inside it,
    # so the kill would be a silent no-op and this leg would "pass"
    # against a mount that never stopped working. Kill the CONTAINER
    # from the node, which is the crash this leg is about.
    cid=$(docker exec "$NODE" crictl ps --name flint-passthrough -o json 2>/dev/null | python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: sys.exit(0)
for c in d.get('containers', []):
    if c['labels'].get('io.kubernetes.pod.name')=='reader': print(c['id'])
")
    if [ -z "$cid" ]; then
        bad "could not find the mounter container on node $NODE — the kill never happened"
    else
        docker exec "$NODE" crictl stop "$cid" >/dev/null 2>&1
        broke=0
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            r=$(inpod reader "cat /mnt/s3/shard-02.txt 2>&1")
            case "$r" in *"not connected"*) broke=1; break ;; esac
            [ "$r" != "seeded-object-02" ] && { broke=1; break; }
            sleep 2
        done
        [ "$broke" -eq 1 ] && ok "the app container goes ENOTCONN — its view died with the mounter" \
                           || bad "the mount never broke; the kill did not land"

        rs=0
        for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
            rs=$($K -n $NS get pod reader -o jsonpath='{.status.initContainerStatuses[0].restartCount}')
            [ "${rs:-0}" -ge 1 ] && break
            sleep 4
        done
        [ "${rs:-0}" -ge 1 ] && ok "kubelet restarted the mounter in place ($rs)" \
                             || bad "the mounter never restarted — the rest of this leg is untested"

        # THE FINDING, asserted rather than assumed. The consumer's view
        # of a FUSE filesystem is a private copy made when its container
        # started; the replacement's mount does not reach it. This is
        # not repairable from inside the pod, which is why the leg below
        # exists.
        sleep 10
        after=$(inpod reader "cat /mnt/s3/shard-02.txt 2>&1")
        case "$after" in
            *"not connected"*) ok "and the RUNNING app container stays broken — a mounter crash is not recoverable in place" ;;
            "seeded-object-02") bad "the app container recovered: the stranding this design documents did not happen" ;;
            *) bad "the app container reads '${after:-nothing}' — neither stranded nor recovered" ;;
        esac

        # The mitigation: the replacement must not sit there Ready while
        # the workload reads ENOTCONN.
        srdy=""
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            srdy=$($K -n $NS get pod reader -o jsonpath='{.status.initContainerStatuses[0].ready}')
            [ "$srdy" = "false" ] && break
            sleep 3
        done
        [ "$srdy" = "false" ] && ok "the replacement mounter reports NOT READY" \
                              || bad "the replacement mounter is ready='$srdy' while its consumers are stranded"
        pr=$($K -n $NS get pod reader -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')
        [ "$pr" = "False" ] && ok "and the POD is NotReady, so a Service stops sending it work" \
                            || bad "pod Ready='$pr' — the stranding is still invisible to everything outside"
    fi
    # Leave the rig usable: this leg deliberately destroys `reader`.
    $K -n $NS delete pod reader --wait=true >/dev/null 2>&1
    $K apply -f mounts.yaml >/dev/null 2>&1
fi

# ── roster reconciliation ────────────────────────────────────────────
EXPECTED_LEGS="A1 A2 A2b A3 A4 A5 A6 A7 A8 A9 A10 A11 A12"
for l in $EXPECTED_LEGS; do
    case " $RAN_LEGS " in *" $l "*) ;; *) bad "leg $l is declared but never ran" ;; esac
done
for l in $RAN_LEGS; do
    case " $EXPECTED_LEGS " in *" $l "*) ;; *) bad "leg $l ran but is not on the roster" ;; esac
done

echo
echo "═══ passthrough e2e: $PASS ok, $FAILED bad ═══"
[ "$FAILED" -eq 0 ]
