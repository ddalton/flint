#!/usr/bin/env bash
# chart render pass — every chart templates, and the peer lists that gate
# the hub's two doors produce VALID NetworkPolicyPeer objects.
#
# WHY THIS EXISTS AS ITS OWN TIER
#
# `helm lint` passes on all three charts and always did. It does not
# render the optional blocks: NetworkPolicy is `enabled: false` by
# default in both charts, so every peer-list template below is dead text
# under stock values and nothing exercised it. That is how
# `apiClientSelectors` shipped with `nindent 8` where the NFS list next
# to it correctly used 10 — a two-key peer made `helm template` fail
# outright with a YAML parse error, and the ONE-key example in the docs
# rendered `podSelector: null` with `matchLabels` hoisted to peer level,
# which is not a valid peer and silently admits nothing like it means.
#
# So the assertion is not "does it render" but "does the rendered peer
# have the keys the operator put in it". A test that only checked the
# exit status of `helm template` would have passed on the one-key form
# while the policy did the wrong thing.
#
# No cluster, no images, no kubectl — helm and python3 only. Runs in CI.
#
# Usage:  tests/regression/chart-render-pass.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TMP="$(mktemp -d -t flint-chart-render.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

fails=0
pass() { echo "✓ $*"; }
fail() { echo "✗ $*"; fails=$((fails + 1)); }

need() { command -v "$1" >/dev/null 2>&1 || { echo "MISSING TOOL: $1 — this test cannot run and is NOT passing"; exit 2; }; }
need helm
need python3

CHARTS="flint-csi-driver-chart flint-lite-chart flint-lite-operator-chart"

# ── 1. every chart lints and templates under stock values ────────────
for c in $CHARTS; do
  if helm lint "$ROOT/$c" >"$TMP/lint-$c.txt" 2>&1; then
    pass "helm lint $c"
  else
    fail "helm lint $c"; tail -5 "$TMP/lint-$c.txt"
  fi
  if helm template t "$ROOT/$c" >"$TMP/tpl-$c.yaml" 2>"$TMP/tpl-$c.err"; then
    pass "helm template $c (stock values)"
  else
    fail "helm template $c (stock values)"; head -3 "$TMP/tpl-$c.err"
  fi
done

# ── 2. the peer lists, in BOTH the one-key and two-key shapes ────────
#
# One key is the shape the docs show; two keys is the shape a real front
# door needs (a namespace AND a pod selector). Both must produce a peer
# whose keys are exactly what was written.
cat >"$TMP/np.yaml" <<'EOF'
networkPolicy:
  enabled: true
  hubNamespaces: ["workspaces"]
  nfsClientSelectors:
    - namespaceSelector:
        matchLabels:
          kubernetes.io/metadata.name: consumers
      podSelector:
        matchLabels:
          app: consumer
  apiClientSelectors:
    - namespaceSelector:
        matchLabels:
          kubernetes.io/metadata.name: frontdoor
      podSelector:
        matchLabels:
          app: fd
EOF

cat >"$TMP/np-onekey.yaml" <<'EOF'
networkPolicy:
  enabled: true
  hubNamespaces: ["workspaces"]
  nfsClientSelectors:
    - podSelector:
        matchLabels:
          app: consumer
  apiClientSelectors:
    - podSelector:
        matchLabels:
          app: fd
EOF

check_peers() {
  local values="$1" label="$2"
  if ! helm template t "$ROOT/flint-lite-operator-chart" -f "$values" \
        -s templates/networkpolicy.yaml >"$TMP/np-out.yaml" 2>"$TMP/np-err.txt"; then
    fail "$label: helm template failed"; head -3 "$TMP/np-err.txt"; return
  fi
  python3 - "$TMP/np-out.yaml" "$label" <<'PY'
import sys, yaml
path, label = sys.argv[1], sys.argv[2]
docs = [d for d in yaml.safe_load_all(open(path)) if d]
want = {2049: "nfs", 8080: "api"}
seen = {}
for d in docs:
    for rule in (d.get("spec", {}).get("ingress") or []):
        ports = [p.get("port") for p in (rule.get("ports") or [])]
        for port, door in want.items():
            if port not in ports:
                continue
            for peer in (rule.get("from") or []):
                # A peer whose value is None is the hoisting bug: the
                # key is present and empty while its matchLabels became
                # a sibling. That is a peer that matches nothing.
                if any(v is None for v in peer.values()):
                    print(f"FAIL {label}: {door} peer has a null value: {peer}")
                    sys.exit(1)
                if "matchLabels" in peer:
                    print(f"FAIL {label}: {door} peer has matchLabels hoisted to peer level: {peer}")
                    sys.exit(1)
                seen.setdefault(door, []).append(sorted(peer.keys()))
for door in ("nfs", "api"):
    if door not in seen:
        print(f"FAIL {label}: no {door} rule rendered")
        sys.exit(1)
print(f"OK {label}: nfs peers {seen['nfs']} | api peers {seen['api']}")
PY
  if [ $? -eq 0 ]; then pass "$label"; else fail "$label"; fi
}

check_peers "$TMP/np.yaml"        "two-key peers render valid on BOTH doors"
check_peers "$TMP/np-onekey.yaml" "one-key peers render valid on BOTH doors"

# ── 3. the two doors must not be gated by one port ───────────────────
# The 8080 rule hardcodes its port while spec.monitoring.port is free.
# This does not fix that; it pins the current shape so a change is seen.
if grep -q 'port: 8080' "$ROOT/flint-lite-operator-chart/templates/networkpolicy.yaml"; then
  pass "8080 is still hardcoded in the API rule (known: spec.monitoring.port is not consulted)"
else
  fail "the API rule's port changed — if monitoring.port is now honoured, delete this check"
fi

# -- 4. the gateway: it must refuse to render without credentials -----
#
# `flint-hub-gateway` reaches every project's files. Three shapes must
# be refused at TEMPLATE time rather than discovered as a CrashLoop or,
# far worse, as a running open proxy:
#   - no inbound token     -> anyone who reaches the Service reads everything
#   - no hub credential    -> every upstream call is unauthenticated
#   - both hub credentials -> ambiguous which the fleet was provisioned with
#
# The binary refuses all three too. This asserts the CHART refuses them,
# because a chart that renders a broken Deployment turns a config
# mistake into a pull-image-and-crashloop instead of a helm error.
GW="--set gateway.enabled=true"
refuses() {
  local label="$1"; shift
  if helm template t "$ROOT/flint-lite-operator-chart" "$@" >/dev/null 2>"$TMP/gw-err.txt"; then
    fail "$label: rendered anyway"
  else
    pass "$label"
  fi
}
refuses "gateway without an inbound token is refused" $GW
refuses "gateway without a hub credential is refused" $GW --set gateway.tokenSecretRef=t
refuses "gateway with BOTH hub credentials is refused" $GW --set gateway.tokenSecretRef=t \
        --set gateway.rootKeySecretRef=r --set gateway.hubTokenSecretRef=h

# And the valid shape renders, so the refusals above are the guards and
# not a template that never works.
if helm template t "$ROOT/flint-lite-operator-chart" $GW \
      --set gateway.tokenSecretRef=t --set gateway.rootKeySecretRef=r \
      >"$TMP/gw.yaml" 2>"$TMP/gw-err.txt"; then
  pass "gateway renders with a token and a root key"
else
  fail "gateway does not render even when configured"; head -3 "$TMP/gw-err.txt"
fi

# The numeric args must not reach clap in scientific notation. helm
# renders a bare 5368709120 as 5.36870912e+09, which clap rejects at
# startup -- a CrashLoop that no lint and no `helm template` exit status
# would have caught. This one already happened.
if grep -qE -- '--max-upload-bytes=[0-9]+$' "$TMP/gw.yaml"; then
  pass "maxUploadBytes renders as an integer, not scientific notation"
else
  fail "maxUploadBytes is not a plain integer: $(grep -o -- '--max-upload-bytes=[^ ]*' "$TMP/gw.yaml")"
fi

# The gateway must never be granted Secrets. That grant is what the
# derived-token design exists to avoid: the workspace namespaces hold
# the tenants' S3 credentials next to the API tokens. Nor create/delete
# on shares -- provisioning a project is the front door's decision.
python3 "$ROOT/tests/regression/lib/gateway-role-check.py" "$TMP/gw.yaml"
if [ $? -eq 0 ]; then pass "the gateway role grants no Secrets and cannot create or delete shares"; else fail "the gateway role is too wide"; fi

# With BOTH the policy and the gateway on, the hub's 8080 rule must
# admit the gateway. Nobody would have to remember to repeat its
# selector in apiClientSelectors -- but if the auto-peer ever regresses,
# the symptom is every file request timing out with the policy looking
# fine, so it is pinned here.
if helm template t "$ROOT/flint-lite-operator-chart" $GW \
      --set gateway.tokenSecretRef=t --set gateway.rootKeySecretRef=r \
      --set networkPolicy.enabled=true --set 'networkPolicy.hubNamespaces={workspaces}' \
      -s templates/networkpolicy.yaml >"$TMP/gw-np.yaml" 2>"$TMP/gw-np-err.txt"; then
  if grep -q 'flint-lite-operator-gateway' "$TMP/gw-np.yaml"; then
    pass "the hub's 8080 rule admits the gateway automatically"
  else
    fail "networkPolicy is on and the gateway is NOT admitted to 8080 — every file request would time out"
  fi
else
  fail "networkpolicy.yaml does not render with the gateway on"; head -3 "$TMP/gw-np-err.txt"
fi

# The token mount must stay a WHOLE-DIRECTORY projection. A subPath
# mount is frozen at pod start, which would silently return the inbound
# token to boot-time behaviour with nothing failing.
if grep -A3 'mountPath: /etc/flint/gateway-token' "$TMP/gw.yaml" | grep -q 'subPath'; then
  fail "the gateway token is mounted with subPath -- rotation would silently stop working"
else
  pass "the gateway token is a whole-directory mount (rotation stays live)"
fi

echo
if [ "$fails" -eq 0 ]; then echo "chart render pass: ALL GREEN"; exit 0; fi
echo "chart render pass: $fails FAILED"; exit 1
