#!/usr/bin/env bash
# flint-s3-csi image pull secrets (issue #85). The chart could not give its
# pods a pull secret, so on a registry that needs a login every worker sat
# in ImagePullBackOff and every S3 mount hung. Asserted here, on the
# rendered objects:
#
#   - stock values render no imagePullSecrets anywhere and no worker env;
#   - global.imagePullSecrets reaches the node DaemonSet's pod (the prepull
#     init containers included: they share it), the broker's pod, and the
#     plugin's FLINT_S3CSI_WORKER_IMAGE_PULL_SECRETS, in order;
#   - a component's own list wins over global, and its empty list means
#     none, whatever global says;
#   - an entry may be a name or a {name: ...} map;
#   - a name no Secret can have, or a list that is not a list, fails the
#     render and names the key;
#   - workers.serviceAccountName reaches the plugin's env and the admission
#     policy's one admitted account; a bad name fails the render;
#   - NOTES names the Secrets to check, per namespace.
#
# helm and python3 (PyYAML) only, no cluster. Runs in CI.
#
# Usage:  tests/system/chart-s3csi-pull-secrets.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CHART="$ROOT/flint-s3-csi-chart"
TMP="$(mktemp -d -t flint-s3csi-pull.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT
for t in helm python3; do
  command -v "$t" >/dev/null || { echo "MISSING TOOL: $t — this test cannot run and is NOT passing"; exit 2; }
done
python3 -c 'import yaml' 2>/dev/null || { echo "MISSING TOOL: python3 PyYAML — this test cannot run and is NOT passing"; exit 2; }

fails=0
BASE=(-n flint-system --set broker.static.secretRef=s3keys)

render() { helm template t "$CHART" "${BASE[@]}" "$@" >"$TMP/out.yaml" 2>"$TMP/err.txt"; }

# What the render says, one line per fact, for exact comparison:
#   node=<names>  broker=<names>  initc=<n>  env=<value|->  sa=<value|->  policy-sa=<value>
facts() {
  python3 - "$TMP/out.yaml" <<'PY'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
def one(kind, name):
    for d in docs:
        if d.get("kind") == kind and d["metadata"]["name"] == name:
            return d
    return None
def names(d):
    if d is None:
        return "absent"
    l = d["spec"]["template"]["spec"].get("imagePullSecrets")
    return "unset" if l is None else ",".join(e["name"] for e in l)
ds = one("DaemonSet", "flint-s3-csi-node")
br = one("Deployment", "flint-s3-broker")
env = {e["name"]: e.get("value") for e in ds["spec"]["template"]["spec"]["containers"][0]["env"]}
pol = one("ValidatingAdmissionPolicy", "flint-s3-csi-workers")
psa = "absent" if pol is None else next(
    (v["expression"] for v in pol["spec"]["variables"] if v["name"] == "workerServiceAccount"), "absent")
print("node=" + names(ds))
print("broker=" + names(br))
print("initc=%d" % len(ds["spec"]["template"]["spec"].get("initContainers") or []))
print("env=" + (env.get("FLINT_S3CSI_WORKER_IMAGE_PULL_SECRETS") or "-"))
print("sa=" + (env.get("FLINT_S3CSI_WORKER_SERVICE_ACCOUNT") or "-"))
print("policy-sa=" + psa)
PY
}

# <case> <expected facts, space-separated> <helm args...>
expect() {
  local c=$1 want=$2; shift 2
  if ! render "$@"; then
    echo "✗ $c: the render failed: $(head -c 300 "$TMP/err.txt")"; fails=$((fails + 1)); return
  fi
  local got; got=$(facts | tr '\n' ' ' | sed 's/ $//')
  local w
  for w in $want; do
    case " $got " in
      *" $w "*) ;;
      *) echo "✗ $c: want $w, got: $got"; fails=$((fails + 1)); return ;;
    esac
  done
  echo "✓ $c: $want"
}

# <case> <text the error must carry> <helm args...>
refuses() {
  local c=$1 says=$2; shift 2
  if render "$@"; then
    echo "✗ $c: rendered anyway"; fails=$((fails + 1))
  elif grep -qF -- "$says" "$TMP/err.txt"; then
    echo "✓ $c: refused, naming $says"
  else
    echo "✗ $c: refused for another reason: $(head -c 300 "$TMP/err.txt")"; fails=$((fails + 1))
  fi
}

values() { cat >"$TMP/$1.yaml"; echo "$TMP/$1.yaml"; }

# The control: nothing set renders what it rendered before the knob.
expect "stock" "node=unset broker=unset env=- sa=- policy-sa='default'"

# The prepull init containers live in the node pod and pull under its list:
# assert they are rendered, or "node=" would not cover them.
expect "global list reaches every pod and the plugin, in order" \
  "node=regcred,mirror.pull broker=regcred,mirror.pull initc=1 env=regcred,mirror.pull" \
  --set 'global.imagePullSecrets={regcred,mirror.pull}'

expect "a component's own list wins over global" \
  "node=nodecred broker=brokercred env=wcred,wcred2" \
  -f "$(values own <<'EOF'
global: { imagePullSecrets: [regcred] }
node: { imagePullSecrets: [nodecred] }
broker: { imagePullSecrets: [brokercred] }
workers: { imagePullSecrets: [wcred, wcred2] }
EOF
)"

expect "an empty own list is none, whatever global says" \
  "node=regcred broker=unset env=-" \
  -f "$(values empty <<'EOF'
global: { imagePullSecrets: [regcred] }
broker: { imagePullSecrets: [] }
workers: { imagePullSecrets: [] }
EOF
)"

expect "a {name: ...} entry is a name, and a repeat is folded" \
  "node=regcred,mirror broker=regcred,mirror env=regcred,mirror" \
  -f "$(values maps <<'EOF'
global: { imagePullSecrets: [{ name: regcred }, mirror, { name: regcred }] }
EOF
)"

refuses "a name no Secret can have" "global.imagePullSecrets: \"Regcred\"" \
  --set 'global.imagePullSecrets={Regcred}'
refuses "a worker name with a comma smuggled in a map" "workers.imagePullSecrets: \"a,b\"" \
  -f "$(values comma <<'EOF'
workers: { imagePullSecrets: [{ name: "a,b" }] }
EOF
)"
refuses "a map entry with no name" "node.imagePullSecrets: \"\"" \
  -f "$(values noname <<'EOF'
node: { imagePullSecrets: [{ secret: regcred }] }
EOF
)"
refuses "a string where a list belongs" "workers.imagePullSecrets must be a list" \
  --set workers.imagePullSecrets=regcred

expect "workers.serviceAccountName reaches the plugin and the policy" \
  "sa=flint-s3-worker policy-sa='flint-s3-worker'" \
  --set workers.serviceAccountName=flint-s3-worker
refuses "a ServiceAccount name no account can have" "workers.serviceAccountName \"Flint_SA\"" \
  --set workers.serviceAccountName=Flint_SA

# NOTES: helm template does not render it, so render it through tpl in a
# copy of the chart.
cp -R "$CHART" "$TMP/chart"
cp "$CHART/templates/NOTES.txt" "$TMP/chart/notes-src.txt"
cat >"$TMP/chart/templates/zz-notes.yaml" <<'EOF'
apiVersion: v1
kind: ConfigMap
metadata: { name: notes }
data:
  notes: {{ tpl (.Files.Get "notes-src.txt") . | toJson }}
EOF
notes() {
  helm template t "$TMP/chart" "${BASE[@]}" "$@" -s templates/zz-notes.yaml 2>"$TMP/err.txt" |
    python3 -c 'import sys, yaml; print(yaml.safe_load(sys.stdin)["data"]["notes"])' >"$TMP/notes.txt"
}
check_notes() { # <case> <grep -F text>...
  local c=$1; shift
  local s
  for s in "$@"; do
    if ! grep -qF -- "$s" "$TMP/notes.txt"; then
      echo "✗ NOTES $c: no line with: $s"; fails=$((fails + 1)); return
    fi
  done
  echo "✓ NOTES $c"
}
notes -f "$TMP/own.yaml" --set workers.serviceAccountName=flint-s3-worker
check_notes "names every Secret in its namespace" \
  "kubectl -n flint-system get secret nodecred brokercred" \
  "kubectl -n flint-workers get secret wcred wcred2" \
  "kubectl -n flint-workers get serviceaccount flint-s3-worker"
notes
if grep -q "PULL SECRETS" "$TMP/notes.txt"; then
  echo "✗ NOTES stock: talks about pull secrets with none configured"; fails=$((fails + 1))
else
  echo "✓ NOTES stock: silent about pull secrets"
fi

echo
if [ "$fails" -eq 0 ]; then echo "chart s3csi pull secrets: ALL GREEN"; exit 0; fi
echo "chart s3csi pull secrets: $fails FAILED"; exit 1
