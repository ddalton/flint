#!/usr/bin/env bash
# THE PUBLISHED ARTIFACT — the path a user takes, which no other forge
# drill takes.
#
# Every other leg in forge/e2e runs images built from the checkout, and
# `build-forge-images.sh` says why in its own header: "a drill that
# verifies a claim scored from the current code must run the current
# code, not a release." That is right, and it leaves a hole nothing
# else covers. Twelve falsifiers are green against `drill-<sha7>`
# images. A user runs `helm install ./flint-forge-chart` and gets
# whatever tag `values.yaml` names, pulled from Docker Hub. Those are
# not the same artifact, and only one of them has ever been drilled.
#
# So this leg overrides NO image. It installs the chart as shipped,
# lets the chart's own defaults choose the images, forces them to be
# pulled rather than found, and then asks the released thing to do the
# job forge exists for: clone, push durably, move a protected branch,
# lose its pod, and serve a clone restored from the bucket alone.
#
#   ./run-published.sh                 # against the current kind context
#   CTX=kind-forge-pub ./run-published.sh
#   KEEP=1 ./run-published.sh          # leave the cluster up afterwards
#   OVERRIDE_TAG=1.46.0-forge.6 ./run-published.sh    # DIAGNOSTIC, see below
#
# `OVERRIDE_TAG` exists for one question and answers no other: when the
# chart's own default is broken, is bumping the tag ENOUGH, or is there
# a second failure hiding behind the first? It runs every leg against a
# named tag instead of the chart's. It also turns P1 PENDING, because a
# run told which images to use is not evidence about which images the
# chart chooses — and that is the entire claim of this drill.
#
# It needs a kind cluster and nothing else — no AWS, no spend. The
# store is an in-cluster MinIO this rig stands up itself, so the leg
# does not depend on the s3csi rig being present.
#
# WHAT THIS LEG DELIBERATELY DOES NOT COVER. kind's default CNI does
# not enforce a NetworkPolicy, so the `X-Remote-User` trust boundary is
# inert here; that claim belongs to `run-rights.sh`, which runs on
# Cilium. Nothing below should be read as evidence about it.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
CHART="$ROOT/flint-forge-chart"

CTX=${CTX:-$(kubectl config current-context 2>/dev/null)}
K="kubectl --context $CTX"
NS_SYS=${NS_SYS:-forge-system}
NS_AGENTS=${NS_AGENTS:-agents}
DOOR=${DOOR:-http://flint-forge-door.$NS_SYS.svc}
REPO_NS_NAME="$NS_AGENTS/proj"
RUN=$(date +%s)

PASS=0; FAILED=0; PENDING=0
ok()   { PASS=$((PASS+1));     echo "  ok: $1"; }
bad()  { FAILED=$((FAILED+1)); echo "  BAD: $1"; }
# Tracked apart from both, because a leg that could not run is not a
# leg that passed. The verdict prints it separately and the exit code
# distinguishes it.
pend() { PENDING=$((PENDING+1)); echo "  PENDING: $1"; }
note() { echo "  ..  $1"; }
leg()  { echo; echo "── $1 — $2"; }

verdict() {
  echo
  echo "══ published artifact: $PASS passed, $FAILED failed, $PENDING pending ══"
  if [ "$FAILED" -gt 0 ]; then echo "   FAILED"; return 1; fi
  if [ "$PENDING" -gt 0 ]; then echo "   GREEN, with $PENDING leg(s) awaiting a release"; return 2; fi
  echo "   GREEN"; return 0
}

# ── the tags the CHART names, read out of the chart ───────────────────
# Read, never passed in. A drill that took the tag as an argument would
# be testing whatever the operator typed, and the claim here is about
# what the chart hands a user who types nothing.
chart_tag() { # chart_tag <yaml path fragment>
  case $1 in
    operator) sed -n '/^image:/,/^[a-z]/p' "$CHART/values.yaml" | sed -n 's/^  tag: *"\{0,1\}\([^"]*\)"\{0,1\} *$/\1/p' | head -1 ;;
    git)      sed -n 's#^  gitImage: *\(.*\)$#\1#p'    "$CHART/values.yaml" | head -1 ;;
    syncer)   sed -n 's#^  syncerImage: *\(.*\)$#\1#p' "$CHART/values.yaml" | head -1 ;;
  esac
}

# Does a tag exist on Docker Hub? Public repos need no auth.
tag_exists() { # tag_exists <repo> <tag>
  local code
  code=$(curl -s -o /dev/null -w '%{http_code}' \
         "https://hub.docker.com/v2/repositories/$1/tags/$2" 2>/dev/null)
  [ "$code" = 200 ]
}

# The highest published tag sharing a base with the chart's, so the
# drift check is against what exists rather than against a constant
# that would itself go stale.
highest_tag() { # highest_tag <repo> <base>   e.g. dilipdalton/x 1.46.0-forge
  curl -s "https://hub.docker.com/v2/repositories/$1/tags?page_size=100" 2>/dev/null \
    | python3 -c '
import sys, json, re
base = sys.argv[1]
try: rows = json.load(sys.stdin).get("results", [])
except Exception: sys.exit(0)
pat = re.compile(r"^" + re.escape(base) + r"\.(\d+)$")
ns = [int(m.group(1)) for r in rows if (m := pat.match(r["name"]))]
print(f"{base}.{max(ns)}" if ns else "")' "$2"
}

# ── in-pod helpers, the shape run-rights.sh established ───────────────
# Command substitution output is NOT re-expanded by the calling shell,
# so $T/$A/$U here stay literal for the pod.
door_pre() {
  printf 'T=$(cat /var/run/secrets/forge/token); A="Authorization: Basic $(printf "x:%%s" "$T" | base64 -w0)"; G() { git -c http.extraHeader="$A" "$@"; }; U=%s/git/%s/%s.git' \
         "$DOOR" "$NS_AGENTS" "$1"
}
inpod() { local p=$1; shift; $K -n "$NS_AGENTS" exec "$p" -c agent -- sh -c "$*" 2>&1; }
expect() { # expect <pod> ok|refuse <label> <script>
  local pod=$1 exp=$2 label=$3 script=$4 out rc
  out=$(inpod "$pod" "$script"); rc=$?
  case $exp in
    ok) if [ $rc -eq 0 ]; then ok "$label"; else
          bad "$label — expected success, rc=$rc"; echo "$out" | tail -4 | sed 's/^/        /'; fi ;;
    refuse) if [ $rc -ne 0 ]; then
              ok "$label (refused)"
              echo "$out" | grep -iE 'protected|may (be pushed|propose)|only .* may|denied|remote:' | head -1 | sed 's/^/        · /'
            else bad "$label — SUCCEEDED but the policy should have refused it"; fi ;;
  esac
}

mcx() { $K -n "$NS_SYS" exec mc-s3 -- "$@" 2>/dev/null; }

main() {
  [ -n "$CTX" ] || { echo "no kube context; set CTX"; return 1; }
  echo "published-artifact drill — context $CTX, chart $CHART"

  local git_ref syncer_ref op_tag git_repo git_tag base newest
  git_ref=$(chart_tag git); syncer_ref=$(chart_tag syncer); op_tag=$(chart_tag operator)
  git_repo=${git_ref%:*}; git_tag=${git_ref##*:}
  [ -n "$git_tag" ] && [ -n "$op_tag" ] || { bad "could not read image tags out of $CHART/values.yaml"; verdict; return 1; }
  if [ -n "${OVERRIDE_TAG:-}" ]; then
    echo
    echo "  ⚠ DIAGNOSTIC RUN — every image forced to $OVERRIDE_TAG, not the chart's $git_tag."
    echo "    This answers 'is a tag bump sufficient?' and is NOT evidence about the chart."
    git_tag=$OVERRIDE_TAG; op_tag=$OVERRIDE_TAG
    git_ref="dilipdalton/flint-forge-git:$OVERRIDE_TAG"
    syncer_ref="dilipdalton/flint-forge-syncer:$OVERRIDE_TAG"
  fi
  note "chart names: $git_ref"
  note "             $syncer_ref"
  note "             dilipdalton/flint-forge-operator:$op_tag"

  # ── P0 — the tags the chart names exist ─────────────────────────────
  leg P0 "every image the chart names is actually published"
  local missing=0 r t
  for pair in "dilipdalton/flint-forge-operator:$op_tag" "$git_ref" "$syncer_ref"; do
    r=${pair%:*}; t=${pair##*:}
    if tag_exists "$r" "$t"; then ok "$r:$t is on the registry"
    else bad "$r:$t is NOT published — a user running 'helm install' gets ImagePullBackOff"; missing=1; fi
  done
  if [ $missing -ne 0 ]; then
    note "nothing below can run against images that do not exist"
    verdict; return 1
  fi

  # ── P1 — and they are the NEWEST published ──────────────────────────
  # The chart naming a tag that exists is not the same as the chart
  # naming the right one. This repository has shipped an image whose tag
  # said one version and whose binaries were another; the cheap guard
  # against the next instance is to notice when the chart stops moving.
  leg P1 "the chart names the newest published tag, not an older one"
  base=${git_tag%.*}
  newest=$(highest_tag "$git_repo" "$base")
  if [ -n "${OVERRIDE_TAG:-}" ]; then
    pend "OVERRIDE_TAG is set, so this run was TOLD its images — it cannot judge the chart's choice"
  elif [ -z "$newest" ]; then
    pend "could not enumerate $git_repo tags matching $base.N — registry unreachable or the scheme changed"
  elif [ "$newest" = "$git_tag" ]; then
    ok "the chart is current at $git_tag"
  else
    bad "the chart pins $git_tag but $newest is published — a 'helm install' installs the older binaries"
    note "everything below therefore measures $git_tag, which is what a user would get"
  fi

  # ── P2 — force a real pull, so this is evidence about a release ─────
  leg P2 "the images are PULLED from the registry, not found in the node cache"
  local nodes n removed=0
  nodes=$($K get nodes -o jsonpath='{.items[*].metadata.name}' 2>/dev/null)
  [ -n "$nodes" ] || { bad "no nodes in context $CTX"; verdict; return 1; }
  for n in $nodes; do
    for pair in "dilipdalton/flint-forge-operator:$op_tag" "$git_ref" "$syncer_ref"; do
      # Absent is the desired state, so a failure here is not an error.
      if docker exec "$n" crictl rmi "docker.io/$pair" >/dev/null 2>&1; then removed=$((removed+1)); fi
    done
  done
  note "cleared $removed cached image(s) from $(echo "$nodes" | wc -w | tr -d ' ') node(s)"
  ok "the node cache was cleared before install (the pull below cannot be satisfied locally)"

  # ── P3 — install the chart with NO image overrides ──────────────────
  leg P3 "helm install the chart as shipped — no --set of any image"
  $K delete ns "$NS_SYS" "$NS_AGENTS" --ignore-not-found --wait=true --timeout=180s >/dev/null 2>&1
  sed "s#__TAG__#$git_tag#g" "$HERE/rig.yaml.tpl" | $K apply -f - >/dev/null 2>&1
  $K -n "$NS_SYS" rollout status deploy/minio --timeout=180s >/dev/null 2>&1
  $K -n "$NS_SYS" wait --for=condition=complete job/seed-bucket --timeout=180s >/dev/null 2>&1
  $K -n "$NS_SYS" wait --for=condition=ready pod/mc-s3 --timeout=120s >/dev/null 2>&1
  if mcx mc ls m/s3bucket/ >/dev/null 2>&1; then ok "the store is up and the bucket exists"
  else bad "the store never seeded; every leg below would read an empty bucket"; verdict; return 1; fi

  local hlog="/tmp/forge-pub-helm-$RUN.log" hrc
  local -a img_sets=()
  if [ -n "${OVERRIDE_TAG:-}" ]; then
    img_sets=(--set "image.tag=$OVERRIDE_TAG"
              --set "server.gitImage=$git_ref"
              --set "server.syncerImage=$syncer_ref")
  fi
  # A pipeline's exit status is the LAST command's, so this is
  # redirected and the status taken from $? directly.
  helm --kube-context "$CTX" upgrade --install flint-forge "$CHART" \
       -n "$NS_SYS" "${img_sets[@]+"${img_sets[@]}"}" \
       --set door.deploy=true --set door.namespace="$NS_SYS" \
       --wait --timeout 6m > "$hlog" 2>&1
  hrc=$?
  if [ $hrc -eq 0 ]; then
    ok "the chart installed with its own image defaults"
  else
    bad "helm install failed (rc=$hrc)"
    tail -6 "$hlog" | sed 's/^/        /'
    # `helm --wait` reports only "timed out waiting for the condition",
    # which names nothing. The reason is always in a pod that never went
    # ready, so print it here rather than making the next reader go and
    # find it by hand — which is what this leg cost the first time.
    local p ready phase
    for p in $($K -n "$NS_SYS" get pods -o name 2>/dev/null); do
      # A COMPLETED Job pod reports ready=false forever, so phase is
      # checked first: without it the seed Job is printed as a failure
      # on every install that timed out for an unrelated reason.
      phase=$($K -n "$NS_SYS" get "$p" -o jsonpath='{.status.phase}' 2>/dev/null)
      [ "$phase" = Succeeded ] && continue
      ready=$($K -n "$NS_SYS" get "$p" -o jsonpath='{.status.containerStatuses[*].ready}' 2>/dev/null)
      case "$ready" in
        *false*|"")
          echo "        ── $p is not ready:"
          # 20, not 6: clap prints the argument it rejected FIRST and
          # the usage block after it, so a short tail keeps the usage
          # and drops the one line that names the defect.
          $K -n "$NS_SYS" logs "$p" --all-containers --tail=20 2>&1 \
            | grep -v '^[[:space:]]*$' | head -8 | sed 's/^/           /' ;;
      esac
    done
    verdict; return 1
  fi

  # ── P4 — the repository serves ──────────────────────────────────────
  leg P4 "a FlintRepo becomes a serving repository"
  local waited=0 phase=""
  while [ $waited -lt 300 ]; do
    phase=$($K -n "$NS_AGENTS" get flintrepo proj -o jsonpath='{.status.phase}' 2>/dev/null)
    [ "$phase" = Ready ] && break
    sleep 5; waited=$((waited+5))
  done
  if [ "$phase" = Ready ]; then ok "$REPO_NS_NAME reached Ready in ${waited}s"
  else
    bad "$REPO_NS_NAME never reached Ready (phase=${phase:-<none>}) after ${waited}s"
    $K -n "$NS_AGENTS" get pods -o wide 2>/dev/null | sed 's/^/        /'
    $K -n "$NS_AGENTS" describe flintrepo proj 2>/dev/null | tail -15 | sed 's/^/        /'
  fi
  $K -n "$NS_AGENTS" wait --for=condition=ready pod/writer --timeout=180s >/dev/null 2>&1 \
    && ok "the agent pod is running the published forge-git image" \
    || bad "the agent pod never became ready"

  # The pull is the claim; assert it landed as a REGISTRY reference. A
  # `kind load`ed image reports a bare `sha256:…` config digest with no
  # repository in it, a pulled one reports `docker.io/<repo>@sha256:…`.
  # That difference is the whole discriminator.
  #
  # BOTH namespaces, and only here. Checked right after the install this
  # found two operator digests and called it "all images": the syncer
  # and git images run in the repository's namespace and the repository
  # pod does not exist until the operator has reconciled. A provenance
  # check that runs before the pods it is about is a check of nothing.
  local ids pulled=0 total=0 id seen_git=0 seen_syncer=0
  ids=$($K get pods -A -o jsonpath='{range .items[*]}{range .status.containerStatuses[*]}{.imageID}{"\n"}{end}{end}' 2>/dev/null | grep dilipdalton | sort -u)
  for id in $ids; do
    total=$((total+1))
    case "$id" in *dilipdalton/*@sha256:*) pulled=$((pulled+1)) ;; esac
    case "$id" in *flint-forge-git*)    seen_git=1 ;; esac
    case "$id" in *flint-forge-syncer*) seen_syncer=1 ;; esac
  done
  # Count what SHOULD be there rather than trusting what turned up: all
  # three images must appear, or the leg is judging a subset.
  if [ "$total" -eq 0 ]; then
    bad "no flint containers found in any namespace to check provenance on"
  elif [ "$seen_git" -eq 0 ] || [ "$seen_syncer" -eq 0 ]; then
    bad "the forge-git and/or forge-syncer image never appeared — provenance was judged on a subset"
    echo "$ids" | sed 's/^/        · /'
  elif [ "$pulled" -eq "$total" ]; then
    ok "all $total distinct flint image(s) carry a registry digest — operator, git and syncer all came from Docker Hub"
    echo "$ids" | sed 's/^/        · /'
  else
    bad "$((total - pulled)) of $total image(s) have no registry digest — loaded locally, not pulled"
    echo "$ids" | sed 's/^/        · /'
  fi

  # ── P5 — the durability claim, through the door ─────────────────────
  leg P5 "clone, commit and push — and the BUCKET names the ref before the client is told ok"
  expect writer ok "the writer seeds main via refs/for/main" \
    "$(door_pre proj); rm -rf /tmp/w && G clone -q \$U /tmp/w && cd /tmp/w && git config user.email w@x && git config user.name writer && { git checkout -q main 2>/dev/null || git checkout -q -b main; } && echo 'published-$RUN' > README.md && git add -A && git commit -qm seed && G push origin HEAD:refs/for/main"
  expect writer ok "an agent branch push is accepted" \
    "$(door_pre proj); cd /tmp/w && git checkout -q -b agent/pub-$RUN && echo body-$RUN > f-$RUN.txt && git add -A && git commit -qm work && G push -q origin agent/pub-$RUN"

  local tip
  tip=$(inpod writer "$(door_pre proj); cd /tmp/w && git rev-parse HEAD" | tr -d '\r\n')
  # The oracle is the bucket, not the server: "acknowledged means
  # durable" is a claim about S3, and asking the pod that just answered
  # ok would be asking the same party twice.
  local snap
  snap=$(mcx mc cat m/s3bucket/published/proj/git/snapshot 2>/dev/null)
  if printf '%s' "$snap" | grep -q "agent/pub-$RUN"; then
    ok "the bucket's snapshot names agent/pub-$RUN"
  else
    bad "the bucket's snapshot does not name the branch the client was told was durable"
    printf '%s' "$snap" | head -c 400 | sed 's/^/        /'
  fi
  [ -n "$tip" ] && note "agent branch tip $tip"

  # ── P6 — the protected branch, on the published binary ──────────────
  leg P6 "main is protected, and the refusal names the remedy"
  expect writer refuse "a direct push to protected main" \
    "$(door_pre proj); cd /tmp/w && git checkout -q -B main origin/main && echo x >> README.md && git commit -qam touch && G push origin main"
  expect writer ok "the same change proposed through refs/for/main merges" \
    "$(door_pre proj); cd /tmp/w && G push origin HEAD:refs/for/main"
  expect writer ok "main now carries it" \
    "$(door_pre proj); cd /tmp/w && G fetch -q origin && git show origin/main:README.md | grep -q 'published-$RUN'"

  # ── P7 — is `git propose` in the release yet? ───────────────────────
  # PENDING, never BAD: the verb is new in the working tree and cannot
  # be in an image published before it. This leg turns green on its own
  # the first time a release carries it, which is the point of writing
  # it now rather than after.
  leg P7 "the published forge-git image carries the 'git propose' verb"
  if inpod writer "command -v git-propose >/dev/null && git propose --help >/dev/null 2>&1"; then
    expect writer ok "git propose proposes into the default branch" \
      "$(door_pre proj); cd /tmp/w && git checkout -q -B prop-$RUN origin/main && echo 'via propose $RUN' >> README.md && git commit -qam propose && GIT_PROPOSE_REMOTE=origin git -c http.extraHeader=\"\$A\" propose"
  else
    pend "git-propose is not in $git_ref — expected until a release after the commit that adds it"
    note "the long form it wraps IS covered by P6: push origin HEAD:refs/for/main"
  fi

  # ── P8 — the headline: lose the pod, serve a clone from the bucket ──
  leg P8 "the pod is destroyed and the published image restores from the bucket alone"
  local before
  before=$(inpod writer "$(door_pre proj); cd /tmp/w && G ls-remote origin refs/heads/main | cut -f1" | tr -d '\r\n' | tail -c 41)
  $K -n "$NS_AGENTS" delete pod -l app.kubernetes.io/name=forge-proj --wait=true --timeout=120s >/dev/null 2>&1 \
    || $K -n "$NS_AGENTS" delete pod -l flintrepo=proj --wait=true --timeout=120s >/dev/null 2>&1
  note "server pod deleted — the emptyDir went with it, so the bucket is all that is left"
  $K -n "$NS_AGENTS" rollout status deploy/forge-proj --timeout=300s >/dev/null 2>&1

  local after=""
  waited=0
  while [ $waited -lt 240 ]; do
    after=$(inpod writer "$(door_pre proj); G ls-remote \$U refs/heads/main 2>/dev/null | cut -f1" | tr -d '\r\n' | tail -c 41)
    [ -n "$after" ] && break
    sleep 5; waited=$((waited+5))
  done
  if [ -n "$before" ] && [ "$after" = "$before" ]; then
    ok "main is at the same commit after the restore ($before)"
  else
    bad "main was $before before the kill and ${after:-<unreachable>} after it"
  fi
  expect writer ok "a FRESH clone from the restored repository passes git fsck --strict" \
    "$(door_pre proj); rm -rf /tmp/fresh && G clone -q \$U /tmp/fresh && cd /tmp/fresh && git fsck --strict --no-progress >/dev/null 2>&1 && grep -q 'published-$RUN' README.md"

  verdict
}

cleanup() {
  local rc=$?
  if [ "${KEEP:-0}" != 1 ]; then
    $K delete ns "$NS_SYS" "$NS_AGENTS" --ignore-not-found --wait=false >/dev/null 2>&1
    helm --kube-context "$CTX" uninstall flint-forge -n "$NS_SYS" >/dev/null 2>&1
  else
    echo "KEEP=1: $NS_SYS and $NS_AGENTS left standing"
  fi
  exit $rc
}
trap cleanup EXIT
main "$@"
