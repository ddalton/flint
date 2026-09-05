#!/usr/bin/env bash
# The git-server-to-S3 path at a size where its defects live — on a
# real cluster against real S3, because the things this drill exists
# for cannot be produced on loopback:
#
#   * a RESTORE long enough to outlast the takeover window
#     (QUIET_POLLS × heartbeat = 6 × 10 s), which takes a multi-GiB pack
#     at S3's single-stream rate — MinIO on the Mac restores 96 MiB in
#     about a second (forge/e2e/largerepo);
#   * a MULTIPART compose that a real bucket judges: the CRC64NVME
#     full-object checksum at CompleteMultipartUpload, the part grid,
#     and the orphaned uploads a killed syncer leaves behind, which cost
#     money on S3 and nothing on MinIO.
#
# Legs, each with its oracle stated at the leg:
#   S0  provenance and the rig: the syncer in the pod is the tree under
#       test (a content marker, never a tag), both repositories serve,
#       the disks are big enough
#   S1  calibrate on a PROBE_MB repository: push, multipart, cold
#       restore; the restore RATE sizes the large repository
#   S2  the large repository: composed on real S3, restored ranged (anon
#       RSS flat, not scaling with the pack), and the lease token SILENT
#       for longer than the takeover window during the push and during
#       the restore — the window, observed from outside before any
#       challenger is introduced
#   S3  the takeover, with a control. A challenger syncer is started
#       while the holder restores. Small repository (restore << window):
#       the challenger never claims. Large repository: it claims
#       mid-restore, and what happens next is recorded — epochs, fences,
#       restarts, time to serve — while an agent pushes throughout, so
#       that NO ACKNOWLEDGED PUSH IS LOST is checked, not assumed
#   S4  acknowledged means durable at multipart size: the syncer is
#       killed INSIDE the multipart upload (seen via
#       list-multipart-uploads, not a guessed sleep), the f1 invariant
#       is checked, and the orphaned uploads are counted and sized
#
# INCONCLUSIVE is not PASS: a leg that could not measure what it exists
# for is counted separately and fails the run.
#
# Prereqs: forge/e2e/scale/deploy.sh has run, and the workers' emptyDirs
# are backed by something larger than an 8 GiB root (prep-nodes.sh).
# The drill reads the bucket with the SAME scoped key the syncer uses.
#
#   BUCKET=... PREFIX=... KEYFILE=... ./forge/e2e/scale/run-scale.sh
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)
: "${BUCKET:?set BUCKET}"
: "${PREFIX:?set PREFIX to what deploy.sh printed}"
if [ -n "${KEYFILE:-}" ]; then
  AWS_ACCESS_KEY_ID=$(jq -r .AccessKey.AccessKeyId "$KEYFILE")
  AWS_SECRET_ACCESS_KEY=$(jq -r .AccessKey.SecretAccessKey "$KEYFILE")
  export AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
  unset AWS_PROFILE
fi
REGION=${REGION:-us-west-1}
export AWS_REGION=$REGION AWS_DEFAULT_REGION=$REGION
NS=${NS:-agents}; AGENT=${AGENT:-agent1}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
BIG=${BIG:-big}; SMALL=${SMALL:-small}
PROBE_MB=${PROBE_MB:-1024}
MIN_BIG_MB=${MIN_BIG_MB:-2048}; MAX_MB=${MAX_MB:-40960}   # 40960: with the restore fan-out (7557d3c1) an i4i.xlarge restores at ~340 MiB/s, and 10 GiB came back in 30 s — inside the 60 s window (runbx)
TARGET_RESTORE_SECS=${TARGET_RESTORE_SECS:-150}
SMALL_MB=${SMALL_MB:-16}
DUR_MB=${DUR_MB:-2048}; DUR_ITER=${DUR_ITER:-3}   # 2048: a 320 MiB upload finished inside the kill's 0.5-3.5 s jitter on real S3 (runbw), so no kill landed mid-upload
HEARTBEAT=${HEARTBEAT:-10}; QUIET_POLLS=6; WINDOW=$((HEARTBEAT * QUIET_POLLS))
STATUS_PORT=9848
WHOLE_PUT_MAX=$((64 * 1024 * 1024))
LEGS=${LEGS:-S0 S1 S2 S3 S4}
BIG_MB=${BIG_MB:-}          # set by S1; give it explicitly to skip S1
# What the tree under test is EXPECTED to do. The oracles INVERT with
# the fixes — a drill that encodes one behaviour as PASS confirms
# nothing about the other — so the expectation is named, never guessed:
#   EXPECT=window-open    before the renewer: the token is silent through
#                         the push's upload and the whole restore, and a
#                         challenger claims a live pod (design §5)
#   EXPECT=window-closed  a renewer task keeps the token moving: silence
#                         stays under WINDOW/2 during both, and the
#                         challenger never claims against a live restore
#   SWEEP=none            forge leaves an interrupted upload orphaned
#   SWEEP=claim           the successor aborts orphans after its claim:
#                         S4 must see them at the kill and none once it serves
EXPECT=${EXPECT:-window-open}; SWEEP=${SWEEP:-none}
# The digest the build pushed (the `docker push` line of
# build-forge-images.sh, with or without the repository in front).
# The chart pins 1.46.0-forge.1 and deploy.sh defaults to forge.2, both
# older than the fixes the inverted oracles test, so a deploy that fell
# back to a default would fail S2/S3/S4 looking like a broken fix.
DIGEST=${DIGEST:-}; DIGEST=${DIGEST##*@}
case "$EXPECT" in window-open|window-closed) ;; *) echo "EXPECT must be window-open or window-closed (got '$EXPECT')" >&2; exit 2;; esac
case "$SWEEP" in none|claim) ;; *) echo "SWEEP must be none or claim (got '$SWEEP')" >&2; exit 2;; esac

RUN=$(date +%Y%m%d-%H%M%S)
RESULTS=${RESULTS:-$HERE/results}
WORK=$RESULTS/work-$RUN; mkdir -p "$WORK"
LOG=$RESULTS/scale-$RUN.log
exec > >(tee -a "$LOG") 2>&1

PASS=0; FAIL=0; INCONC=0; RAN=""
ok()     { PASS=$((PASS+1));     printf '  PASS  %s\n' "$*"; }
bad()    { FAIL=$((FAIL+1));     printf '  FAIL  %s\n' "$*"; }
inconc() { INCONC=$((INCONC+1)); printf '  INCONCLUSIVE  %s\n' "$*"; }
note()   { printf '  ....  %s\n' "$*"; }
leg()    { RAN="$RAN $1"; printf '\n══ %s — %s ══\n' "$1" "$2"; }

# ── helpers ──────────────────────────────────────────────────────────
K() { kubectl "$@"; }
now() { date +%s; }
mib() { python3 -c "print(f'{int(float(\"${1:-0}\"))/1048576:.1f}')"; }
iso2unix() { python3 -c "import sys,datetime as d; s=sys.argv[1].split('.')[0].rstrip('Z'); print(int(d.datetime.strptime(s,'%Y-%m-%dT%H:%M:%S').replace(tzinfo=d.timezone.utc).timestamp()))" "$1"; }
kp()  { printf '%s/%s' "$PREFIX" "$1"; }            # the FlintRepo's keyPrefix, no trailing slash
key() { printf '%s/git/%s' "$(kp "$1")" "$2"; }    # an object under the repository's git/ root
repo_pod() {
  K -n "$NS" get pods -l "chert.us/repo=$1" -o json 2>/dev/null \
    | jq -r '[.items[] | select(.metadata.deletionTimestamp == null)] | sort_by(.metadata.creationTimestamp) | last | .metadata.name // empty'
}
pod_node()       { K -n "$NS" get pod "$1" -o jsonpath='{.spec.nodeName}' 2>/dev/null; }
# From INSIDE the syncer container, not through the API server's pod
# proxy: the repository's NetworkPolicy admits :9848 from the operator
# alone, and Cilium enforces it — the proxy answered on every kind rig
# only because kind's CNI enforces no policy at all.
phase()          { [ -n "${1:-}" ] || { echo "?"; return; }; K -n "$NS" exec "$1" -c syncer -- wget -qO- "http://127.0.0.1:$STATUS_PORT/status" 2>/dev/null | jq -r '.phase // "?"' 2>/dev/null | grep . || echo "?"; }
restarts()       { K -n "$NS" get pod "$1" -o jsonpath='{.status.containerStatuses[?(@.name=="syncer")].restartCount}' 2>/dev/null || echo 0; }
syncer_started() { K -n "$NS" get pod "$1" -o jsonpath='{.status.containerStatuses[?(@.name=="syncer")].state.running.startedAt}' 2>/dev/null; }
gitq()  { local r=$1; shift; K -n "$NS" exec "$(repo_pod "$r")" -c syncer -- git --git-dir="/repo/$NS/$r.git" "$@"; }
inpod() { K -n "$NS" exec "$AGENT" -c agent -- sh -c "$*" 2>&1; }
snap_ref() { aws s3 cp "s3://$BUCKET/$(key "$1" snapshot)" - 2>/dev/null | jq -r --arg r "refs/heads/$2" '.refs[$r] // "<absent>"'; }
snap_refs() { aws s3 cp "s3://$BUCKET/$(key "$1" snapshot)" - 2>/dev/null | jq -r '.refs | to_entries[] | "\(.key) \(.value)"' | sort; }
newest_pack() {
  aws s3api list-objects-v2 --bucket "$BUCKET" --prefix "$(key "$1" objects/pack/)" \
    --query 'sort_by(Contents[?ends_with(Key, `.pack`)], &LastModified)[-1].[Key,Size,ETag]' --output text 2>/dev/null
}
wait_serving() { # repo secs
  local i p; for i in $(seq 1 "$(( $2 / 2 ))"); do p=$(repo_pod "$1"); [ "$(phase "$p")" = serving ] && return 0; sleep 2; done; return 1
}

# Watchers run as background subshells until a .stop file appears.
tok_watch() { # repo outfile
  local k; k=$(key "$1" epoch)
  ( while [ ! -e "$2.stop" ]; do
      # The CLI's own timeouts default to 60 s: one hung HEAD in run 4
      # made a 2 s sampler blind for 64 s and reported the token it
      # had seen at the START of the call, which `longest_silence`
      # then read as 66 s of a quiet renewer. A sample that does not
      # come back in 5 s is recorded as `blind`, and a blind spot is
      # reported as what it is, never as silence.
      printf '%s %s\n' "$(now)" "$(aws s3api head-object --bucket "$BUCKET" --key "$k" --query ETag --output text \
        --cli-connect-timeout 5 --cli-read-timeout 5 2>/dev/null || echo blind)" >> "$2"
      sleep 2; done ) & disown
}
rss_watch() { # pod outfile — cAdvisor's rssBytes is ANON memory; page cache (the packs being written) is excluded
  local node; node=$(pod_node "$1")
  ( while [ ! -e "$2.stop" ]; do
      K get --raw "/api/v1/nodes/$node/proxy/stats/summary" 2>/dev/null \
        | jq -r --arg p "$1" '.pods[] | select(.podRef.name==$p) | .containers[] | select(.name=="syncer") | "\(.memory.rssBytes // 0) \(.memory.workingSetBytes // 0)"' >> "$2" 2>/dev/null
      sleep 3; done ) & disown
}
log_follow() { # pod outfile — re-attaches across container restarts
  ( while [ ! -e "$2.stop" ]; do K -n "$NS" logs -f --timestamps "$1" -c syncer >> "$2" 2>/dev/null; sleep 1; done ) & disown
}
timeline() { # repo challenger outfile
  ( while [ ! -e "$3.stop" ]; do
      local p; p=$(repo_pod "$1")
      printf '%s holder=%s phase=%s restarts=%s challenger=%s crestarts=%s\n' \
        "$(now)" "$p" "$(phase "$p")" "$(restarts "$p")" "$(phase "$2")" "$(restarts "$2")" >> "$3"
      sleep 2; done ) & disown
}
stop_watch() { touch "$1.stop"; pkill -P "$2" 2>/dev/null; kill "$2" 2>/dev/null; wait "$2" 2>/dev/null; }
# Longest run of one unchanged token, in seconds, counted ONLY across
# contiguous observations: a `blind` sample, or a gap of more than 8 s
# between two samples of a 2 s sampler, ends the run without extending
# it. What the watcher did not see is not silence — it is a blind
# spot, which `longest_blind` reports separately and the verdicts
# treat as inconclusive when it is long enough to have hidden one.
longest_silence() {
  awk 'BEGIN{max=0;have=0}
       $2=="blind"{have=0; next}
       !have{t0=$1;last=$2;prev=$1;have=1;next}
       { if($1-prev>8){t0=$1;last=$2;prev=$1;next}
         if($2!=last){g=$1-t0; if(g>max)max=g; t0=$1; last=$2}
         prev=$1 }
       END{ if(have){g=prev-t0; if(g>max)max=g}; print max+0}' "$1" 2>/dev/null
}
longest_blind() { # longest span the watcher could not see, seconds
  awk 'BEGIN{max=0;lastgood=-1}
       $2=="blind"{next}
       { if(lastgood>=0){g=$1-lastgood; if(g>8 && g>max)max=g}; lastgood=$1 }
       END{print max+0}' "$1" 2>/dev/null
}
max_col() { awk -v c="$2" 'BEGIN{m=0}{if($c+0>m)m=$c+0}END{print m}' "$1" 2>/dev/null; }

cleanup() {
  touch "$WORK"/*.stop 2>/dev/null
  inpod "touch /work/chaos.stop" >/dev/null 2>&1
  K -n "$NS" delete pod "challenger-$BIG" "challenger-$SMALL" --ignore-not-found --wait=false >/dev/null 2>&1
}
trap cleanup EXIT

# ── the agent's scripts (quoted heredocs: nothing here expands on the Mac) ──
put_script() { K -n "$NS" exec -i "$AGENT" -c agent -- sh -c "cat > /work/$1 && chmod +x /work/$1"; }
install_agent_scripts() {
  put_script lib.sh <<'EOS'
# sourced by the others; DOOR and NS come from the environment
auth() { T=$(cat /var/run/secrets/forge/token); A="Authorization: Basic $(printf 'x:%s' "$T" | base64 -w0)"; }
G() { auth; git -c http.extraHeader="$A" "$@"; }
url() { echo "$DOOR/git/$NS/$1.git"; }
EOS
  put_script build.sh <<'EOS'
#!/bin/sh
# build.sh <name> <mib>: a repository of <mib> MiB of incompressible
# content under /work/<name>; prints the tip. Incompressible so the
# pack is the size of the content — git would deflate a pattern to
# nothing and the pack would never reach the regime under test.
set -e
name=$1; mb=$2; files=$(( (mb + 31) / 32 ))
d=/work/$name; rm -rf "$d"; mkdir -p "$d/blobs"; cd "$d"
git init -q -b main
git config user.email scale@invalid; git config user.name scale
# No delta search and no deflate: 8 GiB of random bytes would otherwise
# cost minutes of CPU finding deltas that do not exist.
git config pack.window 0; git config pack.depth 0; git config core.compression 0
i=0; while [ $i -lt $files ]; do dd if=/dev/urandom of="blobs/b$i" bs=1M count=32 status=none; i=$((i+1)); done
git add -A >/dev/null; git commit -qm "$name: $mb MiB incompressible" >/dev/null
rm -rf blobs
git rev-parse HEAD
EOS
  put_script push.sh <<'EOS'
#!/bin/sh
# push.sh <name> <repo> <ref>: push /work/<name>'s HEAD to refs/heads/<ref> on <repo> through the door
. /work/lib.sh
cd "/work/$1" && G push -q "$(url "$2")" "HEAD:refs/heads/$3"
EOS
  put_script lsremote.sh <<'EOS'
#!/bin/sh
. /work/lib.sh
G ls-remote "$(url "$1")" "refs/heads/$2"
EOS
  put_script chaos.sh <<'EOS'
#!/bin/sh
# chaos.sh <repo> <ref> <interval>: commit and push until /work/chaos.stop
# exists, logging "<unix> ACK|NAK <oid>" per push. A push that hangs on
# a door holding the request for a dead server is a NAK after 90 s.
. /work/lib.sh
repo=$1; ref=$2; every=$3; d=/work/chaos-$repo; log=/work/chaos-$repo.log
mkdir -p "$d"; cd "$d"
[ -d .git ] || { git init -q -b main; git config user.email chaos@invalid; git config user.name chaos; }
i=0
while [ ! -e /work/chaos.stop ]; do
  echo "$i $(date +%s)" >> f; git add f; git commit -qm "c$i" >/dev/null
  oid=$(git rev-parse HEAD)
  if timeout 90 sh -c '. /work/lib.sh; G push -q "$(url "$1")" "HEAD:refs/heads/$2"' _ "$repo" "$ref" >/dev/null 2>&1; then
    echo "$(date +%s) ACK $oid" >> "$log"
  else
    echo "$(date +%s) NAK $oid" >> "$log"
  fi
  i=$((i+1)); sleep "$every"
done
EOS
}

# ── S0 ───────────────────────────────────────────────────────────────
leg_S0() {
  leg S0 "provenance and the rig: the syncer in the pod is the tree under test, both repositories serve, the disks are big enough"
  local p img m r ph hb need free_agent free_repo
  p=$(repo_pod "$BIG")
  [ -n "$p" ] || { inconc "no pod for $BIG — deploy first"; return; }
  img=$(K -n "$NS" get pod "$p" -o jsonpath='{.status.containerStatuses[?(@.name=="syncer")].imageID}')
  note "syncer image: $img"
  if [ -n "$DIGEST" ]; then
    case "$img" in
      *"@$DIGEST") ok "the syncer pod runs the digest this run pushed ($DIGEST)" ;;
      *) bad "the syncer pod runs $img, not the pushed $DIGEST: the deploy fell back to another tag (the chart pins 1.46.0-forge.1, deploy.sh defaults to forge.2 — both older than the fixes)" ;;
    esac
  else
    note "DIGEST unset: the image is judged by content only (DIGEST=sha256:… from the build's push line pins it)"
  fi
  # The binary is read OUT and split here: busybox grep matches a line
  # as a C string, so everything past the first NUL byte of a line is
  # invisible to it — `grep -a -c` inside the pod said 0 for a string
  # that is there (runbw, 2026-09-05).
  K -n "$NS" exec "$p" -c syncer -- cat /usr/local/bin/flint-forge-syncer 2>/dev/null \
      | LC_ALL=C tr -c '[:print:]' '\n' > "$WORK/syncer-strings"
  m=$(grep -c 'earlier chunks keep their progress' "$WORK/syncer-strings" | tr -d '[:space:]')
  [ "${m:-0}" -ge 1 ] \
    && ok "the syncer carries the ranged restore (marker string ×$m, 827c5f90) — this is the tree under test, judged by content" \
    || bad "the syncer binary lacks the ranged-restore marker: this image is NOT the tree under test"
  # The fixes the knobs name are judged the same way, so a knob that
  # disagrees with the image fails HERE as "wrong image" and never in
  # S2/S3/S4 as "the fix does not work" (or, worse, passes window-open
  # against a tree that closed it).
  m=$(grep -c 'moved nothing since the last renewal' "$WORK/syncer-strings" | tr -d '[:space:]')
  case "$EXPECT:${m:-0}" in
    window-closed:0) bad "EXPECT=window-closed but the syncer lacks the renewer's gate string (2a213b01): this image predates the fix under test" ;;
    window-closed:*) ok "the syncer carries the progress-gated renewer (marker ×$m, 2a213b01): the window-closed oracle has its subject" ;;
    window-open:0)   ok "the syncer has no renewer task (pre-2a213b01), as EXPECT=window-open assumes" ;;
    window-open:*)   bad "EXPECT=window-open but the syncer carries the renewer (marker ×$m, 2a213b01): the oracle would fail for the wrong reason — run with EXPECT=window-closed" ;;
  esac
  m=$(grep -c 'aborted a multipart upload left in flight' "$WORK/syncer-strings" | tr -d '[:space:]')
  case "$SWEEP:${m:-0}" in
    claim:0) bad "SWEEP=claim but the syncer lacks the sweep's abort string (2a213b01): this image predates the fix under test" ;;
    claim:*) ok "the syncer carries the claim-time orphan sweep (marker ×$m, 2a213b01)" ;;
    none:0)  ok "the syncer has no orphan sweep (pre-2a213b01), as SWEEP=none assumes" ;;
    none:*)  bad "SWEEP=none but the syncer carries the orphan sweep (marker ×$m, 2a213b01): S4 would measure a leak this image closes — run with SWEEP=claim" ;;
  esac
  # The dump is the whole binary as text (17 MiB) and is derived from
  # the digest S0 already recorded; it does not belong in the results.
  rm -f "$WORK/syncer-strings"
  for r in "$BIG" "$SMALL"; do
    p=$(repo_pod "$r"); ph=$(phase "$p")
    [ "$ph" = serving ] && ok "$r is serving ($p)" || bad "$r is '$ph' ($p)"
  done
  if aws s3api head-object --bucket "$BUCKET" --key "$(key "$BIG" snapshot)" >/dev/null 2>&1; then
    local n; n=$(aws s3 cp "s3://$BUCKET/$(key "$BIG" snapshot)" - 2>/dev/null | jq '.refs | length')
    [ "$n" = 0 ] && ok "$BIG's snapshot names no refs (fresh prefix)" || bad "$BIG already has $n refs — use a fresh PREFIX"
  else
    ok "$BIG has no snapshot yet (fresh prefix)"
  fi
  hb=$(K -n "$NS" get deploy "forge-$BIG" -o json | jq -r '.spec.template.spec.containers[] | select(.name=="syncer") | .env[]? | select(.name=="FLINT_FORGE_HEARTBEAT_SECS") | .value // empty')
  if [ -n "$hb" ]; then HEARTBEAT=$hb; WINDOW=$((HEARTBEAT * QUIET_POLLS)); fi
  note "takeover window = QUIET_POLLS $QUIET_POLLS × heartbeat ${HEARTBEAT}s = ${WINDOW}s"
  free_agent=$(inpod "df -Pm /work | awk 'NR==2{print \$4}'" | tr -d '[:space:]')
  free_repo=$(K -n "$NS" exec "$(repo_pod "$BIG")" -c syncer -- sh -c "df -Pm /repo | awk 'NR==2{print \$4}'" 2>/dev/null | tr -d '[:space:]')
  need=$((MAX_MB * 2 + PROBE_MB * 2 + DUR_MB * 8))
  note "free: agent /work ${free_agent:-?} MiB, repository cache ${free_repo:-?} MiB; this run may need ~${need} MiB on each"
  [ "${free_agent:-0}" -ge "$need" ] && ok "the agent's emptyDir has room" \
    || inconc "the agent's /work has ${free_agent:-?} MiB, under ${need}: run prep-nodes.sh (an 8 GiB root cannot hold this drill)"
  [ "${free_repo:-0}" -ge "$need" ] && ok "the repository cache has room" \
    || inconc "the repository cache has ${free_repo:-?} MiB, under ${need}: run prep-nodes.sh"
}

# ── the push-then-restore procedure S1 and S2 share ─────────────────
# Sets TIP, P_SECS, P_SILENCE, PACK_KEY, PACK_SIZE, PACK_ETAG, and cold_restore's R_*.
push_and_restore() { # repo name mb tag
  local repo=$1 name=$2 mb=$3 tag=$4 rc out t0 t1 want attrs ctype ccrc parts sr refs snap lr tw
  note "building $name: $mb MiB of /dev/urandom in the agent's emptyDir"
  t0=$(now); TIP=$(inpod "/work/build.sh $name $mb" | tail -1); t1=$(now)
  case "$TIP" in
    [0-9a-f][0-9a-f][0-9a-f][0-9a-f]*) ok "built $name ($mb MiB) in $((t1 - t0)) s, tip ${TIP:0:12}" ;;
    *) bad "build of $name failed: $TIP"; return 1 ;;
  esac
  tok_watch "$repo" "$WORK/tok-push-$tag"; tw=$!
  t0=$(now); out=$(inpod "DOOR=$DOOR NS=$NS /work/push.sh $name $repo agent/$tag"); rc=$?; t1=$(now)
  sleep $((HEARTBEAT + 5)); stop_watch "$WORK/tok-push-$tag" "$tw"
  P_SECS=$((t1 - t0)); P_SILENCE=$(longest_silence "$WORK/tok-push-$tag"); P_BLIND=$(longest_blind "$WORK/tok-push-$tag")
  K -n "$NS" logs "$(repo_pod "$repo")" -c syncer --timestamps --since="$((P_SECS + HEARTBEAT + 30))s" > "$WORK/log-push-$tag" 2>/dev/null
  if [ "$rc" = 0 ]; then ok "push of $name acknowledged in $P_SECS s"
  else bad "push of $name failed (rc=$rc): $(printf '%s' "$out" | tail -2 | tr '\n' ' ')"; return 1; fi
  note "lease token silent for ${P_SILENCE:-?} s during the push (window ${WINDOW}s); the watcher's longest blind spot ${P_BLIND:-0} s"

  set -- $(newest_pack "$repo"); PACK_KEY=${1:-}; PACK_SIZE=${2:-0}; PACK_ETAG=${3:-}
  [ -n "$PACK_KEY" ] && [ "$PACK_KEY" != None ] || { bad "no pack object under $(key "$repo" objects/pack/)"; return 1; }
  note "pack $(basename "$PACK_KEY"): $(mib "$PACK_SIZE") MiB, ETag $PACK_ETAG"
  [ "$PACK_SIZE" -gt "$WHOLE_PUT_MAX" ] && ok "the pack is above the $(mib "$WHOLE_PUT_MAX") MiB whole-put ceiling" \
    || bad "the pack is under the ceiling — this is not the regime under test"
  want=$(( (PACK_SIZE + WHOLE_PUT_MAX - 1) / WHOLE_PUT_MAX ))
  case "$PACK_ETAG" in
    *-"$want"\"|*-"$want") ok "ETag carries part count $want = ceil(size / 64 MiB): composed by the part grid, not whole-PUT" ;;
    *) bad "ETag $PACK_ETAG does not carry part count $want" ;;
  esac
  attrs=$(aws s3api get-object-attributes --bucket "$BUCKET" --key "$PACK_KEY" --object-attributes Checksum ObjectParts ObjectSize 2>/dev/null)
  ctype=$(printf '%s' "$attrs" | jq -r '.Checksum.ChecksumType // empty')
  ccrc=$(printf '%s' "$attrs" | jq -r '.Checksum.ChecksumCRC64NVME // empty')
  parts=$(printf '%s' "$attrs" | jq -r '.ObjectParts.TotalPartsCount // empty')
  [ "$ctype" = FULL_OBJECT ] && [ -n "$ccrc" ] \
    && ok "S3 holds a FULL_OBJECT CRC64NVME ($ccrc): the checksum forge streamed over the file was accepted at CompleteMultipartUpload" \
    || bad "no full-object CRC64NVME on the composed pack (type '${ctype:-none}')"
  [ "$parts" = "$want" ] && ok "S3 reports $parts parts" || note "S3 reports parts='${parts:-?}'"
  sr=$(snap_ref "$repo" "agent/$tag")
  [ "$sr" = "$TIP" ] && ok "the snapshot names agent/$tag at the tip" || bad "snapshot names agent/$tag = $sr, want $TIP"

  note "cold restore of $repo"
  cold_restore "$repo" "restore-$tag" || return 1
  snap=$(snap_refs "$repo"); refs=$(gitq "$repo" for-each-ref --format='%(refname) %(objectname)' 2>/dev/null | sort)
  [ "$refs" = "$snap" ] && ok "restored refs are exactly the snapshot's" \
    || bad "restored refs differ from the snapshot: $(diff <(echo "$refs") <(echo "$snap") | head -3 | tr '\n' ' ')"
  t0=$(now)
  if gitq "$repo" fsck --strict --no-progress >/dev/null 2>&1; then ok "fsck --strict clean on the restored $(mib "$PACK_SIZE") MiB repository ($(( $(now) - t0 )) s): the composed bytes came back intact"
  else bad "fsck --strict FAILED after the restore"; fi
  lr=$(inpod "DOOR=$DOOR NS=$NS /work/lsremote.sh $repo agent/$tag" | awk '{print $1}' | head -1)
  [ "$lr" = "$TIP" ] && ok "the door advertises agent/$tag at the tip after the restore" || bad "the door advertises '$lr'"
  if [ "${R_RSS:-0}" -gt 0 ]; then
    [ "$R_RSS" -lt "$PACK_SIZE" ] && ok "the restore's anon RSS peaked at $(mib "$R_RSS") MiB for a $(mib "$PACK_SIZE") MiB pack: memory does not scale with the repository" \
      || bad "anon RSS $(mib "$R_RSS") MiB ≥ the pack: memory scales with the repository"
    [ "$R_RSS" -lt $((256 * 1024 * 1024)) ] && note "…and under 256 MiB, consistent with the 38-40 MiB flat loopback figure (working set $(mib "$R_WS") MiB includes page cache)" \
      || note "…but above 256 MiB: higher than the loopback figure; working set $(mib "$R_WS") MiB"
  else
    inconc "no RSS samples for the restore: the node stats proxy answered nothing"
  fi
  RATE=$(python3 -c "print(f'{$PACK_SIZE/1048576/max($R_SECS_CT,1):.1f}')")
  note "restore rate ${RATE} MiB/s from the syncer's start (sequential 8 MiB ranged GETs, one in flight)"
}

# Delete the repository's pod gracefully (SIGTERM releases the lease, so
# the successor claims at once — the restore is what is measured, not a
# lease wait), then watch the successor to `serving`.
# Sets R_POD, R_SECS (from the delete), R_SECS_CT (from the syncer's
# start), R_RSS, R_WS, R_SILENCE.
cold_restore() { # repo tag
  local repo=$1 tag=$2 old new t0 t1 ts ph i tw rw st seen=0
  old=$(repo_pod "$repo")
  note "deleting $old"
  K -n "$NS" delete pod "$old" --wait=false >/dev/null 2>&1
  t0=$(now); new=""
  for i in $(seq 1 180); do new=$(repo_pod "$repo"); [ -n "$new" ] && [ "$new" != "$old" ] && break; new=""; sleep 1; done
  [ -n "$new" ] || { inconc "no successor pod for $repo within 180 s"; return 1; }
  R_POD=$new
  tok_watch "$repo" "$WORK/tok-$tag"; tw=$!
  for i in $(seq 1 120); do [ -n "$(pod_node "$new")" ] && break; sleep 1; done
  rss_watch "$new" "$WORK/rss-$tag"; rw=$!
  while :; do
    ph=$(phase "$new"); ts=$(now)
    echo "$ts $ph" >> "$WORK/phase-$tag"
    [ "$ph" = importing ] && seen=1
    [ "$ph" = serving ] && break
    if [ $((ts - t0)) -gt 3600 ]; then
      inconc "the restore of $repo did not reach serving in 3600 s (last phase '$ph')"
      stop_watch "$WORK/tok-$tag" "$tw"; stop_watch "$WORK/rss-$tag" "$rw"; return 1
    fi
    sleep 1
  done
  t1=$(now)
  sleep $((HEARTBEAT + 5))          # let the first renew after serving land, so the silence run closes
  stop_watch "$WORK/tok-$tag" "$tw"; stop_watch "$WORK/rss-$tag" "$rw"
  R_SECS=$((t1 - t0))
  st=$(syncer_started "$new")
  if [ -n "$st" ]; then R_SECS_CT=$((t1 - $(iso2unix "$st"))); else R_SECS_CT=$R_SECS; fi
  [ "$R_SECS_CT" -gt 0 ] || R_SECS_CT=$R_SECS
  R_RSS=$(max_col "$WORK/rss-$tag" 1); R_RSS=${R_RSS:-0}
  R_WS=$(max_col "$WORK/rss-$tag" 2);  R_WS=${R_WS:-0}
  R_SILENCE=$(longest_silence "$WORK/tok-$tag"); R_SILENCE=${R_SILENCE:-0}; R_BLIND=$(longest_blind "$WORK/tok-$tag"); R_BLIND=${R_BLIND:-0}
  K -n "$NS" logs "$new" -c syncer --timestamps > "$WORK/log-restore-$tag" 2>/dev/null
  note "$repo restored by $new: $R_SECS s from the delete ($R_SECS_CT s from the syncer's start); anon RSS max $(mib "$R_RSS") MiB; lease token silent $R_SILENCE s (watcher blind at most $R_BLIND s)"
  [ "$seen" = 1 ] || note "the poll never caught phase=importing (a fast restore)"
  return 0
}

# ── S1 ───────────────────────────────────────────────────────────────
leg_S1() {
  leg S1 "calibrate: a ${PROBE_MB} MiB repository through the whole path, and its restore rate sizes the large one"
  push_and_restore "$BIG" probe "$PROBE_MB" probe || { inconc "calibration did not complete; BIG_MB cannot be derived"; return; }
  BIG_MB=$(python3 -c "import math; mb=$RATE*$TARGET_RESTORE_SECS; mb=int(math.ceil(mb/256.0)*256); print(max($MIN_BIG_MB, min($MAX_MB, mb)))")
  note "restore rate ${RATE} MiB/s ⇒ a ${TARGET_RESTORE_SECS}s restore needs ~$(python3 -c "print(int($RATE*$TARGET_RESTORE_SECS))") MiB; BIG_MB=$BIG_MB (clamped to [$MIN_BIG_MB, $MAX_MB])"
  [ "$BIG_MB" -gt "$PROBE_MB" ] && ok "the large repository will be ${BIG_MB} MiB, larger than the probe" || bad "sizing produced ${BIG_MB} MiB, not larger than the probe"
}

# ── S2 ───────────────────────────────────────────────────────────────
leg_S2() {
  [ -n "$BIG_MB" ] || { leg S2 "the large repository"; inconc "BIG_MB unknown: run S1 or set BIG_MB"; return; }
  case "$EXPECT" in
    window-open)   leg S2 "the large repository: ${BIG_MB} MiB composed on S3, restored ranged, and the lease silent for longer than the ${WINDOW}s window" ;;
    window-closed) leg S2 "the large repository: ${BIG_MB} MiB composed on S3, restored ranged, and the lease RENEWED throughout (silence under $((WINDOW / 2))s)" ;;
  esac
  push_and_restore "$BIG" big "$BIG_MB" big || return
  BIG_PACK_SIZE=$PACK_SIZE; BIG_R_SECS=$R_SECS; BIG_R_SECS_CT=$R_SECS_CT
  # A healthy token is seen to change about every HEARTBEAT (13 s at a
  # 2 s poll, S1 on runbw); the restore's figure also carries the gap
  # from the old holder's last renewal to the successor's claim
  # (≤ HEARTBEAT + a pod start). WINDOW/2 sits above both and at half
  # of what a challenger needs, so a fixed tree cannot pass by luck and
  # the unfixed one (125 s / 141 s at 10 GiB) cannot pass at all.
  local bound=$((WINDOW / 2))
  case "$EXPECT" in
    window-open)
      if [ "${P_SILENCE:-0}" -gt "$WINDOW" ]; then
        ok "PUSH: the token was silent ${P_SILENCE}s > ${WINDOW}s — the batch renews once, then uploads $(mib "$PACK_SIZE") MiB in serial 64 MiB parts with no heartbeat; a challenger present then would count six quiet polls"
      else
        note "PUSH: the token was silent ${P_SILENCE:-?}s ≤ ${WINDOW}s; the upload of this pack fits inside the window"
      fi
      if [ "${R_SILENCE:-0}" -gt "$WINDOW" ]; then
        ok "RESTORE: the token was silent ${R_SILENCE}s > ${WINDOW}s — the takeover window is OPEN for the whole restore of this repository (design §5 'still open', on the wire)"
      else
        inconc "RESTORE: the token was silent only ${R_SILENCE:-?}s ≤ ${WINDOW}s — the repository is not large enough for S3 to open the window; raise MAX_MB or TARGET_RESTORE_SECS"
      fi ;;
    window-closed)
      # The renewer is progress-gated: a transfer that moves nothing
      # LETS the token go quiet so a wedged server can be taken over,
      # and the syncer says so in its log. A long silence with that
      # line present is the gate's, not a missing renewer — and it
      # means renewal was not exercised across the stall, which is
      # inconclusive, not a pass.
      local gp gr
      gp=$(grep -c 'moved nothing since the last renewal' "$WORK/log-push-big" 2>/dev/null); gp=${gp:-0}
      gr=$(grep -c 'moved nothing since the last renewal' "$WORK/log-restore-big" 2>/dev/null); gr=${gr:-0}
      # Either transfer must itself outlast the window: a token silent for
      # the whole of a 30 s restore is never counted by anyone, so
      # "silence ≤ bound" over it would pass on the unfixed tree too —
      # which is what 10 GiB became once the restore fan-out landed
      # (341 MiB/s on runbx: restored in 30 s).
      if [ "${P_SECS:-0}" -le "$WINDOW" ]; then
        inconc "PUSH: the whole push took ${P_SECS:-?} s ≤ the ${WINDOW}s window — a token silent throughout could not have been counted, so renewal across an upload was not exercised; raise BIG_MB"
      elif [ "${P_SILENCE:-999}" -le "$bound" ] && [ "${P_BLIND:-0}" -gt "$bound" ]; then
        inconc "PUSH: the token was silent at most ${P_SILENCE}s ≤ ${bound}s where the watcher could see, but the watcher was blind for ${P_BLIND}s > ${bound}s (a HEAD hung): a silence that long could have hidden inside it — rerun"
      elif [ "${P_SILENCE:-999}" -le "$bound" ]; then
        ok "PUSH: the token was silent at most ${P_SILENCE}s ≤ ${bound}s while $(mib "$PACK_SIZE") MiB uploaded in $P_SECS s (> ${WINDOW}s window) — renewal covers the upload (progress-gate lines: $gp; watcher blind at most ${P_BLIND:-0}s)"
      elif [ "$gp" -gt 0 ]; then
        inconc "PUSH: the token was silent ${P_SILENCE}s > ${bound}s, and the syncer logged $gp 'moved nothing' line(s): the transfer STALLED and the progress gate held the token quiet by design — renewal across a moving upload was not exercised; rerun"
      else
        bad "PUSH: the token was silent ${P_SILENCE:-?}s > ${bound}s during the upload with no stall reported: the renewer is NOT covering the batch (EXPECT=window-closed)"
      fi
      if [ "${R_SECS_CT:-0}" -le "$WINDOW" ]; then
        inconc "RESTORE: the restore took only ${R_SECS_CT:-?} s from the syncer's start, ≤ the ${WINDOW}s window — a token silent throughout could not have been counted, so renewal across a restore was not exercised and S3's seize arm cannot open the window; raise BIG_MB (rate ${RATE:-?} MiB/s)"
      elif [ "${R_SILENCE:-999}" -le "$bound" ] && [ "${R_BLIND:-0}" -gt "$bound" ]; then
        inconc "RESTORE: the token was silent at most ${R_SILENCE}s ≤ ${bound}s where the watcher could see, but the watcher was blind for ${R_BLIND}s > ${bound}s (a HEAD hung) — rerun"
      elif [ "${R_SILENCE:-999}" -le "$bound" ]; then
        ok "RESTORE: the token was silent at most ${R_SILENCE}s ≤ ${bound}s across a ${R_SECS_CT:-?} s restore (> ${WINDOW}s window) — renewal covers the restore (progress-gate lines: $gr; watcher blind at most ${R_BLIND:-0}s)"
      elif [ "$gr" -gt 0 ]; then
        inconc "RESTORE: the token was silent ${R_SILENCE}s > ${bound}s, and the syncer logged $gr 'moved nothing' line(s): the restore STALLED and the progress gate held the token quiet by design — renewal across a moving restore was not exercised; rerun"
      else
        bad "RESTORE: the token was silent ${R_SILENCE:-?}s > ${bound}s during a ${R_SECS_CT:-?} s restore with no stall reported: the renewer is NOT running before the restore (EXPECT=window-closed)"
      fi ;;
  esac
}

# ── S3 ───────────────────────────────────────────────────────────────
render_challenger() { # repo — the operator's own pod template, minus the labels the ReplicaSet and Service select on
  K -n "$NS" get deploy "forge-$1" -o json \
    | jq --arg n "challenger-$1" '{apiVersion:"v1", kind:"Pod", metadata:{name:$n, namespace:.metadata.namespace, labels:{"drill.chert.us/role":"challenger"}}, spec:(.spec.template.spec + {restartPolicy:"Always"})}' \
    | K apply -f - >/dev/null
}

takeover_arm() { # repo arm(hold|seize) obs_secs
  local repo=$1 arm=$2 obs=$3 ch="challenger-$1" old new t0 t1 tstart ph i p cp tl lc lh
  local ch_claims h_claims deposed first fts fphase epochs acks naks lost gap conv snapref serve_at
  K -n "$NS" delete pod "$ch" --ignore-not-found --wait=true >/dev/null 2>&1
  wait_serving "$repo" 600 || { inconc "$repo is not serving before the $arm arm"; return; }
  old=$(repo_pod "$repo")
  inpod "rm -f /work/chaos.stop /work/chaos-$repo.log" >/dev/null 2>&1
  ( K -n "$NS" exec "$AGENT" -c agent -- env "DOOR=$DOOR" "NS=$NS" /work/chaos.sh "$repo" "agent/chaos-$arm-$RUN" 10 >/dev/null 2>&1 ) & cp=$!
  sleep 15                                     # at least one push lands on the healthy holder
  timeline "$repo" "$ch" "$WORK/tl-$arm"; tl=$!
  log_follow "$ch" "$WORK/log-ch-$arm"; lc=$!
  note "deleting $old; the successor restores and the challenger arrives while it does"
  K -n "$NS" delete pod "$old" --wait=false >/dev/null 2>&1; t0=$(now)
  new=""; for i in $(seq 1 180); do new=$(repo_pod "$repo"); [ -n "$new" ] && [ "$new" != "$old" ] && break; new=""; sleep 1; done
  [ -n "$new" ] || { inconc "no successor for $repo"; stop_watch "$WORK/tl-$arm" "$tl"; stop_watch "$WORK/log-ch-$arm" "$lc"; inpod "touch /work/chaos.stop" >/dev/null; wait "$cp" 2>/dev/null; return; }
  log_follow "$new" "$WORK/log-holder-$arm"; lh=$!
  ph=""; for i in $(seq 1 600); do ph=$(phase "$new"); { [ "$ph" = importing ] || [ "$ph" = serving ]; } && break; sleep 0.5; done
  note "successor $new is '$ph' $(( $(now) - t0 ))s after the delete; starting the challenger ($obs s of observation)"
  render_challenger "$repo"; tstart=$(now)
  sleep "$obs"
  inpod "touch /work/chaos.stop" >/dev/null 2>&1; wait "$cp" 2>/dev/null
  K -n "$NS" delete pod "$ch" --wait=false >/dev/null 2>&1
  conv=0; for i in $(seq 1 600); do p=$(repo_pod "$repo"); [ "$(phase "$p")" = serving ] && { conv=1; break; }; sleep 2; done
  t1=$(now)
  sleep 3
  stop_watch "$WORK/tl-$arm" "$tl"; stop_watch "$WORK/log-ch-$arm" "$lc"; stop_watch "$WORK/log-holder-$arm" "$lh"
  K -n "$NS" logs "$ch" -c syncer --timestamps >> "$WORK/log-ch-$arm" 2>/dev/null   # anything the follower missed
  inpod "cat /work/chaos-$repo.log" > "$WORK/chaos-$arm" 2>/dev/null

  ch_claims=$(grep -c 'holding .* at epoch' "$WORK/log-ch-$arm" 2>/dev/null); ch_claims=${ch_claims:-0}
  h_claims=$(grep -c 'holding .* at epoch' "$WORK/log-holder-$arm" 2>/dev/null); h_claims=${h_claims:-0}
  deposed=$(grep -c 'deposed' "$WORK/log-holder-$arm" 2>/dev/null); deposed=${deposed:-0}
  epochs=$(cat "$WORK/log-ch-$arm" "$WORK/log-holder-$arm" 2>/dev/null | grep 'holding .* at epoch' | sort | sed -E 's/.*at epoch ([0-9]+).*/\1/' | tr '\n' ' ')
  note "$arm arm: challenger claims=$ch_claims, holder claims=$h_claims, holder fenced=$deposed, epoch sequence: ${epochs:-none}; holder restarts=$(restarts "$(repo_pod "$repo")"); time from the delete to a serving operator pod: $((t1 - t0)) s"
  if [ "$ch_claims" -gt 0 ]; then
    first=$(grep -m1 'holding .* at epoch' "$WORK/log-ch-$arm"); fts=$(iso2unix "${first%% *}")
    fphase=$(awk -v t="$fts" '$1<=t{p=$3} END{print p}' "$WORK/tl-$arm" | sed 's/phase=//')
    note "the challenger's first claim came $((fts - tstart)) s after it started, while the holder was '$fphase'"
  fi
  acks=$(grep -c ' ACK ' "$WORK/chaos-$arm" 2>/dev/null); acks=${acks:-0}
  naks=$(grep -c ' NAK ' "$WORK/chaos-$arm" 2>/dev/null); naks=${naks:-0}
  gap=$(awk '$2=="ACK"{if(last){g=$1-last; if(g>m)m=g}; last=$1} END{print m+0}' "$WORK/chaos-$arm" 2>/dev/null)
  note "pushes during the arm: $acks acknowledged, $naks refused or timed out; longest gap between acknowledgements ${gap:-0} s"
  snapref=$(snap_ref "$repo" "agent/chaos-$arm-$RUN")
  lost=0
  while read -r _ res oid; do
    [ "$res" = ACK ] || continue
    gitq "$repo" merge-base --is-ancestor "$oid" "$snapref" >/dev/null 2>&1 || lost=$((lost + 1))
  done < "$WORK/chaos-$arm"
  # when the SUCCESSOR first served, from the timeline (the old pod was serving before the delete)
  serve_at=$(awk -v t="$t0" -v h="holder=$new" '$1>t && $2==h && $3=="phase=serving"{print $1; exit}' "$WORK/tl-$arm" 2>/dev/null)
  [ -n "$serve_at" ] && note "the successor first served $((serve_at - t0)) s after the delete (the challenger arrived at +$((tstart - t0)) s, left at +$((tstart - t0 + obs)) s)" \
    || note "the successor never reached serving while the challenger was present"
  case "$arm" in
    hold)
      [ "$ch_claims" = 0 ] && ok "CONTROL: a challenger beside a HEALTHY holder never claimed in $obs s (the holder's heartbeats kept the token moving)" \
        || bad "CONTROL: the challenger deposed a healthy holder ($ch_claims claims) — the takeover rule itself is broken, and the seize arm proves nothing" ;;
    seize)
      case "$EXPECT" in
        window-open)
          if [ "$ch_claims" -gt 0 ] && [ "$fphase" = importing ]; then
            ok "REPRODUCED: the challenger claimed the repository while its pod was alive and mid-restore ('$fphase'), $((fts - tstart)) s after arriving — the window §5 records, on the wire"
          elif [ "$ch_claims" -gt 0 ]; then
            bad "the challenger claimed while the holder was '$fphase', not importing — a different hazard than the one recorded"
          else
            bad "the recorded hazard did not reproduce: the challenger never claimed in $obs s against a restore of ${BIG_R_SECS:-?} s — either §5 is wrong or this leg is"
          fi
          [ "$deposed" -gt 0 ] && ok "the deposed holder fenced itself at its next heartbeat and exited (no second writer kept serving)" \
            || note "no 'deposed' line from the holder within the observation"
          [ "$h_claims" -gt 1 ] && note "PING-PONG: the operator's pod claimed $h_claims times (epochs ${epochs}) — each side seizes the other mid-restore until a restore finishes inside the window" ;;
        window-closed)
          # The control's oracle, now against a restore longer than the
          # window — and that length is checked on THIS arm's restore,
          # not assumed from S2: inside the window an unfixed tree is
          # never claimed either, and "never claimed" proves nothing.
          local arm_restore=-1; [ -n "$serve_at" ] && arm_restore=$((serve_at - t0))
          if [ -n "$serve_at" ] && [ "$arm_restore" -le "$WINDOW" ]; then
            inconc "the seize arm's successor served ${arm_restore} s after the delete, inside the ${WINDOW}s window: an UNFIXED tree is never claimed here either, so the arm is vacuous — raise BIG_MB (S2's restore took ${BIG_R_SECS_CT:-?} s)"
          elif [ "$ch_claims" = 0 ]; then
            ok "CLOSED: the challenger never claimed in $obs s against a live restore that served ${arm_restore} s after the delete (> ${WINDOW}s window) — the lease was renewed while the holder imported"
          else
            bad "the challenger claimed $((fts - tstart)) s after arriving while the holder was '$fphase': renewal does not cover the restore (EXPECT=window-closed)"
          fi
          [ "$deposed" = 0 ] && ok "the holder was never fenced" \
            || bad "the holder was fenced $deposed time(s) although nothing should have claimed over it"
          [ "$h_claims" -le 1 ] || bad "PING-PONG: the operator's pod claimed $h_claims times (epochs ${epochs}) — the two servers alternated, which a renewed lease forbids"
          [ -n "$serve_at" ] && ok "the successor served $((serve_at - t0)) s after the delete WITH the challenger present — no outage beyond its own restore" \
            || bad "the successor never served during the $obs s observation although the challenger never claimed" ;;
      esac ;;
  esac
  [ "$conv" = 1 ] && ok "the operator's pod was serving again $((t1 - t0)) s after the delete (the challenger removed at +$obs s)" \
    || bad "the operator's pod never returned to serving within 20 min after the challenger was removed"
  [ "$acks" -gt 0 ] || inconc "no push was acknowledged during the $arm arm, so durability under contention was not exercised"
  [ "$lost" = 0 ] && ok "every acknowledged push ($acks) is in the bucket's agent/chaos-$arm-$RUN: told ok ⇒ durable held under contention" \
    || bad "$lost ACKNOWLEDGED push(es) are NOT in the bucket's ref — work lost to the takeover"
}

leg_S3() {
  leg S3 "the takeover during a restore, with its control: the same challenger against a small repository (restore of seconds) and the large one (restore of ${BIG_R_SECS:-?} s)"
  local obs_hold=$((WINDOW * 2 + 30)) obs_seize
  # the control's repository: a few MiB, so its restore is well inside the window
  local tip; tip=$(inpod "/work/build.sh tiny $SMALL_MB" | tail -1)
  inpod "DOOR=$DOOR NS=$NS /work/push.sh tiny $SMALL agent/tiny" >/dev/null 2>&1 \
    && ok "the control repository holds $SMALL_MB MiB at ${tip:0:12}" || bad "seeding $SMALL failed"
  takeover_arm "$SMALL" hold "$obs_hold"
  [ -n "${BIG_R_SECS:-}" ] || { inconc "the large repository's restore time is unknown (S2 did not run); the seize arm needs it"; return; }
  obs_seize=$((BIG_R_SECS * 2 + 120)); [ "$obs_seize" -gt 900 ] && obs_seize=900
  takeover_arm "$BIG" seize "$obs_seize"
}

# ── S4 ───────────────────────────────────────────────────────────────
mpu_list()  { aws s3api list-multipart-uploads --bucket "$BUCKET" --prefix "$1" --query 'Uploads[].[UploadId,Key]' --output text 2>/dev/null | grep -v '^None' | grep . ; }
mpu_count() { mpu_list "$1" | grep -c . ; }
mpu_bytes() { # MiB of parts held by every incomplete upload under a prefix
  local total=0 uid k b
  while read -r uid k; do
    [ -n "$uid" ] || continue
    b=$(aws s3api list-parts --bucket "$BUCKET" --key "$k" --upload-id "$uid" --query 'sum(Parts[].Size)' --output text 2>/dev/null)
    case "$b" in None|"") b=0;; esac
    total=$(python3 -c "print(int($total) + int(float('$b')))")
  done <<EOF
$(mpu_list "$1")
EOF
  mib "$total"
}

leg_S4() {
  leg S4 "acknowledged means durable at multipart size ($DUR_MB MiB pushes to $SMALL): kills INSIDE the upload, seen not timed; and the orphans counted"
  local pp before_n i n name ref tip before after res uid keyu t0 t_seen d pod pusher o acked=0 naked=0 retry
  local o_iter o_kill o_after swept=0 leaked=0
  pp=$(key "$SMALL" objects/pack/)
  before_n=$(mpu_count "$pp")
  note "incomplete multipart uploads under $pp before the leg: $before_n"
  n=$((DUR_ITER + 1))
  for i in $(seq 1 "$n"); do
    K -n "$NS" wait --for=condition=Available "deploy/forge-$SMALL" --timeout=600s >/dev/null 2>&1
    wait_serving "$SMALL" 900 || { inconc "iter $i: $SMALL never came back to serving"; continue; }
    name=dur-$RUN-$i; ref=agent/dur-$RUN-$i     # per run: a re-run on the same prefix must not be a non-fast-forward push onto the last one
    tip=$(inpod "/work/build.sh $name $DUR_MB" | tail -1)
    before=$(snap_ref "$SMALL" "$ref")
    inpod "rm -f /work/res-$i" >/dev/null 2>&1
    ( K -n "$NS" exec "$AGENT" -c agent -- sh -c "DOOR=$DOOR NS=$NS /work/push.sh $name $SMALL $ref >/dev/null 2>&1 && echo ACK > /work/res-$i || echo NAK > /work/res-$i" >/dev/null 2>&1 ) & pusher=$!
    # THIS push's upload, not an orphan an earlier kill left: the ids
    # present before the push began are excluded, or the poll answers
    # in 0 s with the stale one and the kill lands during the git
    # transfer, 30 s before any upload (runbw run 2, iterations 2-3).
    stale=$(mpu_list "$pp" | awk '{print $1}'); o_iter=$(printf '%s\n' "$stale" | grep -c .)
    uid=""; keyu=""; t0=$(now)
    while [ $(( $(now) - t0 )) -lt 600 ]; do
      set -- $(aws s3api list-multipart-uploads --bucket "$BUCKET" --prefix "$pp" --query 'Uploads[?ends_with(Key, `.pack`)].[UploadId,Key]' --output text 2>/dev/null \
               | awk -v s="$stale" 'BEGIN{n=split(s,a,"\n"); for(i=1;i<=n;i++) seen[a[i]]=1} $1!="None" && !seen[$1] {print; exit}')
      uid=${1:-}; keyu=${2:-}
      [ -n "$uid" ] && [ "$uid" != None ] && break
      uid=""; sleep 0.5
    done
    if [ -z "$uid" ]; then
      wait "$pusher" 2>/dev/null
      inconc "iter $i: no multipart upload ever appeared (push result: $(inpod "cat /work/res-$i" | tr -d '[:space:]'))"; continue
    fi
    t_seen=$(( $(now) - t0 ))
    if [ "$i" -le "$DUR_ITER" ]; then
      d=$(awk "BEGIN{srand($i * 7); printf \"%.1f\", 0.5 + rand() * 3.0}"); sleep "$d"
      note "iter $i: upload $(printf '%.12s' "$uid")… on $(basename "$keyu") seen ${t_seen}s after the push began; killing ${d}s later, INSIDE the upload"
    else
      while [ $(( $(now) - t0 )) -lt 600 ]; do aws s3api head-object --bucket "$BUCKET" --key "$keyu" >/dev/null 2>&1 && break; sleep 0.2; done
      note "iter $i: the pack object now exists (upload complete); killing NOW, around the snapshot CAS and the report"
    fi
    pod=$(repo_pod "$SMALL")
    K -n "$NS" delete pod "$pod" --force --grace-period=0 >/dev/null 2>&1
    wait "$pusher" 2>/dev/null
    res=$(inpod "cat /work/res-$i" 2>/dev/null | tr -d '[:space:]')
    # Sample ONE: right after the kill, before any successor can have
    # claimed (a force-deleted holder released nothing, so the successor
    # waits out the quiet polls first). This is what the kill left.
    o_kill=$(mpu_count "$pp")
    K -n "$NS" wait --for=condition=Available "deploy/forge-$SMALL" --timeout=600s >/dev/null 2>&1
    wait_serving "$SMALL" 900
    # Sample TWO: once the successor serves. With a sweep at the claim
    # this is where the orphans go away; without one it equals sample one.
    o_after=$(mpu_count "$pp")
    after=$(snap_ref "$SMALL" "$ref")
    case "$res" in
      ACK) acked=$((acked + 1))
           [ "$after" = "$tip" ] && ok "iter $i: told ok, and the bucket holds it" \
             || bad "iter $i: TOLD OK BUT THE BUCKET LACKS IT (want $tip, bucket $after)" ;;
      *)   naked=$((naked + 1))
           if [ "$after" = "$before" ]; then
             ok "iter $i: told failed, and the bucket is unchanged ($before)"
           elif [ "$after" = "$tip" ]; then
             retry=$(inpod "DOOR=$DOOR NS=$NS /work/push.sh $name $SMALL $ref 2>&1 | tail -1")
             case "$retry" in
               *up-to-date*|*"$ref"*|"") ok "iter $i: indeterminate (the ack was lost after the CAS); the agent's retry is a clean no-op" ;;
               *) bad "iter $i: ack lost AND the retry did not resolve: $retry" ;;
             esac
           else
             bad "iter $i: TOLD FAILED AND THE BUCKET HOLDS SOMETHING ELSE ($before -> $after)"
           fi ;;
    esac
    note "iter $i: incomplete uploads $o_iter before the push, $o_kill at the kill, $o_after after the successor served (holding $(mpu_bytes "$pp") MiB of parts now)"
    if [ "$o_kill" -gt "$o_iter" ]; then
      leaked=$((leaked + 1))
      case "$SWEEP" in
        claim) [ "$o_after" -le "$o_iter" ] \
                 && { swept=$((swept + 1)); ok "iter $i: SWEPT — $((o_kill - o_iter)) upload(s) orphaned by the kill, none left once the successor served"; } \
                 || bad "iter $i: the successor served with $((o_after - o_iter)) orphaned upload(s) still held: no sweep ran at its claim (SWEEP=claim)" ;;
        none)  [ "$o_after" -ge "$o_kill" ] || note "iter $i: $((o_kill - o_after)) upload(s) vanished between the kill and the successor serving although no sweep is expected" ;;
      esac
    elif [ "$i" -le "$DUR_ITER" ]; then
      note "iter $i: the kill left no orphan — it did not land inside the upload"
    fi
  done
  gitq "$SMALL" fsck --strict --no-progress >/dev/null 2>&1 && ok "fsck --strict clean after $n crashes" || bad "fsck failed after the crash series"
  note "$acked acknowledged, $naked refused"
  [ "$acked" -gt 0 ] && [ "$naked" -gt 0 ] && ok "the kills landed on both sides of the window" \
    || note "every iteration landed the same way ($acked ack, $naked nak): the CAS-side kill and the mid-upload kills did not both occur"
  # the leak, measured — from the per-iteration samples, not a final count
  o=$(mpu_count "$pp")
  case "$SWEEP" in
    none)
      if [ "$leaked" -gt 0 ] && [ "$o" -gt "$before_n" ]; then
        ok "MEASURED: $((o - before_n)) orphaned multipart upload(s) holding $(mpu_bytes "$pp") MiB of parts after $leaked kill(s) that landed inside an upload — forge never aborts one (no list_uploads/abort_upload under forge/), so they are billed until a lifecycle rule or a hand abort"
        note "aborting them now so the bucket can be emptied"
        mpu_list "$pp" | while read -r uid k; do [ -n "$uid" ] && aws s3api abort-multipart-upload --bucket "$BUCKET" --key "$k" --upload-id "$uid" >/dev/null 2>&1; done
        note "incomplete uploads after the abort: $(mpu_count "$pp")"
      else
        inconc "no kill landed inside an upload ($o incomplete before and after): the leak's magnitude was not measured — raise DUR_MB"
      fi ;;
    claim)
      if [ "$leaked" = 0 ]; then
        inconc "no kill landed inside an upload, so there was nothing for the sweep to remove — raise DUR_MB"
      elif [ "$swept" = "$leaked" ] && [ "$o" -le "$before_n" ]; then
        ok "SWEPT: every kill that orphaned an upload ($leaked) was followed by a successor that served with none left; $o incomplete upload(s) remain under the prefix"
      else
        bad "orphans survived the successor's claim ($swept of $leaked kills swept; $o incomplete upload(s) remain): the sweep is not doing its job"
      fi ;;
  esac
}

# ── run ──────────────────────────────────────────────────────────────
echo "forge at scale — bucket $BUCKET, prefix $PREFIX, namespace $NS, run $RUN; EXPECT=$EXPECT SWEEP=$SWEEP"
echo "log: $LOG"
K -n "$NS" get pod "$AGENT" >/dev/null 2>&1 || { echo "agent pod $AGENT absent; run deploy.sh"; exit 2; }
install_agent_scripts
for l in $LEGS; do "leg_$l"; done

echo
for l in $LEGS; do case " $RAN " in *" $l "*) ;; *) bad "leg $l never ran";; esac; done
printf '\n══ %d passed, %d failed, %d inconclusive ══\n' "$PASS" "$FAIL" "$INCONC"
[ "$INCONC" -gt 0 ] && echo "INCONCLUSIVE legs could not measure what they exist for: this run is NOT green."
echo "log: $LOG  (raw timelines: $WORK)"
if [ "$FAIL" -gt 0 ]; then exit 1; elif [ "$INCONC" -gt 0 ]; then exit 2; else exit 0; fi
