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
#   S5–S9  THE FRONT'S PARTY TABLE (the runner's acceptance, A3 of
#       docs/plans/flint-forge-simplification-2026-09-05.md), opt-in:
#       LEGS="S0 S5 S6 S7 S9 S8". Each leg is one member of the class the
#       drill kept finding between the client and the syncer, and the
#       CONTROL ARM — the same legs against the pre-A3 git image
#       (server.gitImage=…:1.46.0-forge.6, nginx + fcgiwrap) — must FAIL
#       S5, S6 and S9, or the legs cannot see what they judge.
#   S5  a client SIGSTOPped for STALL_SECS mid-pack is not cut (X3:
#       nginx's client_body_timeout defaulted to 60 s)
#   S6  CONC_N pushes stopped mid-pack hold CONC_N receive-packs in the
#       git container, and a request beside them is answered in seconds
#       (X4: FCGIWRAP_CHILDREN=4 queued the fifth in silence)
#   S7  a rollout while the syncer is `pushing` (X6, decided on the
#       wire): the outcome recorded; told ok ⇒ durable, told failed ⇒
#       unchanged, the successor serves, the retry converges, no orphan
#       survives the claim
#   S8  CONTROL for X5: receive.keepAlive=0 and the door's bound lowered
#       to CTRL_DOOR_SECS — the door cuts the client during the hook
#       wait, so the bound is real and the keepalive is what carries a
#       long push (both settings restored after)
#   S9  the keepalive-gap probe (run 3 finding 2): across a hook wait of
#       at least GAP_MIN_WAIT s the client sees a packet at least every
#       GAP_MAX s, measured from git's own packet trace
#   S10 CLONE_N concurrent clones of one CLONE_MB branch (the fleet's
#       common case): every clone complete and at the tip, the git
#       container's upload-packs peak at CLONE_N (nothing in front of
#       git serialises them), the clones overlap, and a push in the
#       middle of the storm is acknowledged
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
# S5–S9 (the front's party table)
STALL_MB=${STALL_MB:-1024}; STALL_SECS=${STALL_SECS:-70}   # 70 > nginx's 60 s client_body_timeout default (X3)
CONC_MB=${CONC_MB:-256}; CONC_N=${CONC_N:-5}               # 5 > FCGIWRAP_CHILDREN=4 (X4)
ROLL_MB=${ROLL_MB:-12288}                                    # a batch longer than the 30 s grace (X6): 4 GiB uploads inside it at i4i rates
GAP_MB=${GAP_MB:-20480}; GAP_MAX=${GAP_MAX:-8}; GAP_MIN_WAIT=${GAP_MIN_WAIT:-60}   # keepalives every 5 s; a wait long enough to hold many
CTRL_DOOR_SECS=${CTRL_DOOR_SECS:-30}                         # S8 lowers the door's bound to this for its control
DOOR_NS=${DOOR_NS:-forge-system}; DOOR_DEPLOY=${DOOR_DEPLOY:-flint-forge-door}
CLONE_MB=${CLONE_MB:-1024}; CLONE_N=${CLONE_N:-8}               # S10: a fleet cloning one repository
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
  put_script lsall.sh <<'EOS'
#!/bin/sh
# lsall.sh <repo>: one advertisement request; prints its exit status —
# S6 times the request, so the answer's content does not matter, only
# that the server answered.
. /work/lib.sh
G ls-remote "$(url "$1")" >/dev/null 2>&1; echo "rc=$?"
EOS
  put_script clones.sh <<'EOS'
#!/bin/sh
# clones.sh <repo> <ref> <n> <tag>: n concurrent single-branch clones of
# <ref> from <repo> through the door, each into its own directory; one
# line per clone in /work/clone-<tag>.log: "<i> <start> <end> <rc> <head>".
. /work/lib.sh
repo=$1; ref=$2; n=$3; tag=$4; log=/work/clone-$tag.log; rm -f "$log"
i=1
while [ $i -le $n ]; do
  (
    d=/work/clone-$tag-$i; rm -rf "$d"; s=$(date +%s)
    G clone -q --single-branch --branch "$ref" "$(url "$repo")" "$d" >/dev/null 2>&1; rc=$?
    h=$(git -C "$d" rev-parse HEAD 2>/dev/null)
    echo "$i $s $(date +%s) $rc ${h:-none}" >> "$log"
    rm -rf "$d"
  ) &
  i=$((i+1))
done
wait
echo "done $n"
EOS
  put_script pushtrace.sh <<'EOS'
#!/bin/sh
# pushtrace.sh <name> <repo> <ref> <pkt> <curl>: push.sh with every
# packet the client reads stamped to /work/<pkt> (GIT_TRACE_PACKET:
# `sideband<` lines are receive-pack's keepalives and its report) and
# curl's own trace to /work/<curl> (GIT_TRACE_CURL: "upload completely
# sent off" marks the end of the pack on the wire). Both carry
# microsecond timestamps; the gap between them is what S9 judges.
. /work/lib.sh
rm -f "/work/$4" "/work/$5"
cd "/work/$1" && GIT_TRACE_PACKET="/work/$4" GIT_TRACE_CURL="/work/$5" GIT_TRACE_CURL_NO_DATA=1 \
  G push -q "$(url "$2")" "HEAD:refs/heads/$3"
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
  note "git container PID 1: $(K -n "$NS" exec "$p" -c git-http -- sh -c 'tr "\0" " " < /proc/1/cmdline' 2>/dev/null | head -c 120)  (flint-forge-gitcgi = the A3 runner; an entrypoint script = the nginx + fcgiwrap image, the S5–S9 control arm)"
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

# ── S5–S9: the front's party table ───────────────────────────────────
# The simplification note lists the parties between the client and the
# syncer and the knob each one had. The runner (flint-forge-gitcgi, A3)
# claims to remove nginx's and fcgiwrap's and to carry git's keepalive
# untouched; these legs judge it by that class. The client-side stalls
# are SIGSTOP/SIGCONT on the agent's git-remote-http: the TCP connection
# stays open and no byte moves, which is what a paused client looks like
# to every party on the server side. The bracketed last character keeps
# pgrep/pkill from matching their own command line.
stop_clients()      { inpod "pkill -STOP -f 'git-remote-htt[p]'" >/dev/null 2>&1; }
cont_clients()      { inpod "pkill -CONT -f 'git-remote-htt[p]'" >/dev/null 2>&1; }
clients_in_flight() { inpod "pgrep -f 'git-remote-htt[p]' | wc -l" | tr -d '[:space:]'; }
receive_packs()     { K -n "$NS" exec "$(repo_pod "$1")" -c git-http -- sh -c "pgrep -f 'receive-pack --stateless-rp[c]' | wc -l" 2>/dev/null | tr -d '[:space:]'; }   # one per request
push_bg() { # name repo ref resfile — the push runs in the agent pod; ACK|NAK lands in /work/<resfile>, its output in /work/out-<resfile>
  inpod "rm -f /work/$4 /work/out-$4" >/dev/null 2>&1
  ( K -n "$NS" exec "$AGENT" -c agent -- sh -c "DOOR=$DOOR NS=$NS /work/push.sh $1 $2 $3 >/work/out-$4 2>&1 && echo ACK > /work/$4 || echo NAK > /work/$4" >/dev/null 2>&1 ) &
}
# `; true` after the cat: inpod merges kubectl's stderr into the answer,
# and a missing result file would otherwise read as "command terminated
# with exit code 1" — which the first run took for a push's answer.
push_result() { inpod "cat /work/$1 2>/dev/null; true" | tr -d '[:space:]'; }
push_output() { inpod "tail -3 /work/out-$1 2>/dev/null; true" | tr '\n' ' '; }
wait_result() { # resfile secs — prints ACK|NAK, or nothing at the deadline
  local t0 r; t0=$(now)
  while [ $(( $(now) - t0 )) -lt "$2" ]; do r=$(push_result "$1"); [ -n "$r" ] && { echo "$r"; return 0; }; sleep 2; done
  echo ""; return 1
}
wait_clients() { # n secs — until n remote helpers are in flight; prints the count seen last
  local t0 n=0; t0=$(now)
  while [ $(( $(now) - t0 )) -lt "$2" ]; do n=$(clients_in_flight); [ "${n:-0}" -ge "$1" ] && { echo "$n"; return 0; }; sleep 0.5; done
  echo "${n:-0}"; return 1
}
door_bound() { K -n "$DOOR_NS" get deploy "$DOOR_DEPLOY" -o json 2>/dev/null | jq -r '.spec.template.spec.containers[0].args[]? | select(startswith("--upstream-timeout-secs=")) | sub("^.*=";"")' | head -1; }
set_door_bound() { # secs — patches the door's argument in place and waits for the roll
  local i
  i=$(K -n "$DOOR_NS" get deploy "$DOOR_DEPLOY" -o json 2>/dev/null | jq '.spec.template.spec.containers[0].args | to_entries[] | select(.value|startswith("--upstream-timeout-secs=")) | .key' | head -1)
  [ -n "$i" ] || return 1
  K -n "$DOOR_NS" patch deploy "$DOOR_DEPLOY" --type=json \
    -p "[{\"op\":\"replace\",\"path\":\"/spec/template/spec/containers/0/args/$i\",\"value\":\"--upstream-timeout-secs=$1\"}]" >/dev/null 2>&1 || return 1
  K -n "$DOOR_NS" rollout status "deploy/$DOOR_DEPLOY" --timeout=180s >/dev/null 2>&1
}
# gap_stats <packet-trace> <curl-trace>: "n first wait maxgap end" — the
# packets the client read after the pack left the wire: their count, the
# seconds to the first, the seconds to the last (the report), the
# longest gap between consecutive ones (the upload's end included), and
# END: the seconds from the upload's end to curl's last line, which is
# the connection's close — a cut shows up HERE, not among the packets.
# (The first S8 run reported "cut 0.5 s after the upload" from the one
# NUL sideband packet receive-pack sends when the pack is in; curl's
# "transfer closed" line 30.4 s later was the door's cut.)
gap_stats() {
  python3 - "$1" "$2" <<'PY'
import re, sys
def ts(line):
    m = re.match(r'(\d\d):(\d\d):(\d\d)\.(\d{6})', line)
    return None if not m else int(m[1])*3600 + int(m[2])*60 + int(m[3]) + int(m[4])/1e6
def mono(seq):  # a trace can cross midnight
    out, last, off = [], None, 0.0
    for t in seq:
        if last is not None and t + off < last - 1: off += 86400
        out.append(t + off); last = t + off
    return out
up = [ts(l) for l in open(sys.argv[2], errors='replace') if 'upload completely sent off' in l]
pk = [ts(l) for l in open(sys.argv[1], errors='replace') if re.search(r'packet:\s+sideband<', l)]
up = [t for t in up if t is not None]; pk = [t for t in pk if t is not None]
if not up or not pk:
    print("0 ? 0 0"); sys.exit(0)
t_up = mono(up)[-1]
pts = mono([t_up] + pk)
gaps = [b - a for a, b in zip(pts, pts[1:])]
ends = [ts(l) for l in open(sys.argv[2], errors='replace')]
ends = mono([t for t in ends if t is not None])
end = (ends[-1] - t_up) if ends else 0.0
print(f"{len(pk)} {pts[1]-t_up:.1f} {pts[-1]-t_up:.1f} {max(gaps):.1f} {end:.1f}")
PY
}

leg_S5() {
  leg S5 "a client stalled ${STALL_SECS} s mid-pack is not cut (X3: nginx's client_body_timeout defaulted to 60 s beside backend bounds of an hour)"
  local name=stall-$RUN ref=agent/stall-$RUN tip before n phase0 early res after
  wait_serving "$SMALL" 900 || { inconc "$SMALL is not serving"; return; }
  tip=$(inpod "/work/build.sh $name $STALL_MB" | tail -1)
  before=$(snap_ref "$SMALL" "$ref")
  push_bg "$name" "$SMALL" "$ref" res-stall
  n=$(wait_clients 1 120) || { inconc "no git-remote-http appeared in the agent pod within 120 s (push: '$(push_result res-stall)')"; return; }
  sleep 3   # past the advertisement and the negotiation, into the body
  stop_clients
  phase0=$(phase "$(repo_pod "$SMALL")"); early=$(push_result res-stall)
  if [ -z "$early" ] && [ "$phase0" != pushing ]; then ok "PRECONDITION: the client was stopped mid-transfer (no answer yet; the server is '$phase0', not pushing)"
  else bad "PRECONDITION: the stop did not land mid-transfer (answer '$early', server '$phase0') — raise STALL_MB"; fi
  note "client SIGSTOPped for ${STALL_SECS} s"
  sleep "$STALL_SECS"
  cont_clients
  res=$(wait_result res-stall 1800); after=$(snap_ref "$SMALL" "$ref")
  case "$res" in
    ACK) [ "$after" = "$tip" ] && ok "acknowledged after a ${STALL_SECS} s stall mid-pack, and the bucket holds it" \
           || bad "TOLD OK BUT THE BUCKET LACKS IT (want $tip, bucket $after)" ;;
    NAK) bad "the stalled push FAILED: $(push_output res-stall)— a bound between the client and git cut a ${STALL_SECS} s pause (X3's class)" ;;
    *)   inconc "the stalled push never answered within 1800 s" ;;
  esac
}

leg_S6() {
  leg S6 "$CONC_N concurrent pushes are served concurrently, and a request beside them is answered at once (X4: FCGIWRAP_CHILDREN=4 queued the fifth in silence)"
  local i n rp t0 t1 rc res after acked=0 tips=""
  wait_serving "$SMALL" 900 || { inconc "$SMALL is not serving"; return; }
  for i in $(seq 1 "$CONC_N"); do tips="$tips $(inpod "/work/build.sh conc-$RUN-$i $CONC_MB" | tail -1)"; done
  for i in $(seq 1 "$CONC_N"); do push_bg "conc-$RUN-$i" "$SMALL" "agent/conc-$RUN-$i" "res-conc-$i"; done
  n=$(wait_clients "$CONC_N" 120) || { inconc "only $n of $CONC_N remote helpers were in flight together within 120 s — raise CONC_MB"; cont_clients; return; }
  sleep 3; stop_clients
  rp=$(receive_packs "$SMALL")
  [ "${rp:-0}" -ge "$CONC_N" ] && ok "the git container runs $rp receive-pack(s) for $CONC_N stopped clients: nothing in front of git queues them" \
    || bad "the git container runs ${rp:-0} receive-pack(s) for $CONC_N stopped clients: a ceiling in front of git queued the rest (X4's class)"
  t0=$(now); rc=$(inpod "DOOR=$DOOR NS=$NS timeout 60 /work/lsall.sh $SMALL" | tr -d '[:space:]'); t1=$(now)
  [ $(( t1 - t0 )) -le 5 ] && [ "$rc" = "rc=0" ] && ok "a request beside $CONC_N in-flight pushes was answered in $(( t1 - t0 )) s" \
    || bad "a request beside $CONC_N in-flight pushes took $(( t1 - t0 )) s ('${rc:-no answer}'): it queued behind them"
  cont_clients
  set -- $tips
  for i in $(seq 1 "$CONC_N"); do
    res=$(wait_result "res-conc-$i" 1800); after=$(snap_ref "$SMALL" "agent/conc-$RUN-$i")
    if [ "$res" = ACK ] && [ "$after" = "$1" ]; then acked=$((acked + 1)); else note "push $i: '$res', bucket $after (want $1): $(push_output "res-conc-$i")"; fi
    shift
  done
  [ "$acked" = "$CONC_N" ] && ok "all $CONC_N concurrent pushes acknowledged and in the bucket" || bad "$acked of $CONC_N concurrent pushes acknowledged and durable"
}

leg_S7() {
  leg S7 "a rollout during a ${ROLL_MB} MiB push (X6: terminationGracePeriodSeconds 30 against a batch of minutes, SIGTERM seen only between batches): the outcome recorded; told ok ⇒ durable, told failed ⇒ unchanged, the successor serves, a retry converges, no orphan survives its claim"
  local name=roll-$RUN ref=agent/roll-$RUN tip before pod0 t0 t_roll t_gone res after retry o pp
  wait_serving "$SMALL" 900 || { inconc "$SMALL is not serving"; return; }
  pp=$(key "$SMALL" objects/pack/)
  tip=$(inpod "/work/build.sh $name $ROLL_MB" | tail -1); before=$(snap_ref "$SMALL" "$ref")
  push_bg "$name" "$SMALL" "$ref" res-roll
  pod0=$(repo_pod "$SMALL"); t0=$(now)
  while [ $(( $(now) - t0 )) -lt 900 ] && [ "$(phase "$pod0")" != pushing ] && [ -z "$(push_result res-roll)" ]; do sleep 1; done
  [ "$(phase "$pod0")" = pushing ] || { inconc "the syncer never reached 'pushing' (push: '$(push_result res-roll)'): the batch was over before a roll could land — raise ROLL_MB"; return; }
  t_roll=$(now); K -n "$NS" rollout restart "deploy/forge-$SMALL" >/dev/null 2>&1
  note "rollout restarted while $pod0 was pushing"
  while [ $(( $(now) - t_roll )) -lt 300 ] && K -n "$NS" get pod "$pod0" >/dev/null 2>&1; do sleep 1; done
  t_gone=$(( $(now) - t_roll ))
  if [ "$t_gone" -ge 28 ]; then note "the old pod was gone ${t_gone} s after the restart: the grace period ran out — SIGKILL mid-batch"
  else note "the old pod was gone ${t_gone} s after the restart: it exited inside the grace period (the batch finished, or the SIGTERM arm ran between batches)"; fi
  res=$(wait_result res-roll 1800)
  K -n "$NS" wait --for=condition=Available "deploy/forge-$SMALL" --timeout=600s >/dev/null 2>&1
  wait_serving "$SMALL" 900 || { inconc "the successor never served"; return; }
  after=$(snap_ref "$SMALL" "$ref")
  case "$res" in
    ACK) [ "$after" = "$tip" ] && ok "told ok across the roll, and the bucket holds it" || bad "TOLD OK BUT THE BUCKET LACKS IT (want $tip, bucket $after)" ;;
    NAK) if [ "$after" = "$before" ]; then ok "told failed, and the bucket is unchanged ($(push_output res-roll))"
         elif [ "$after" = "$tip" ]; then ok "told failed, but durable (the roll cut the client after the CAS) — run 3's transition; the retry must be a no-op"
         else bad "TOLD FAILED AND THE BUCKET HOLDS SOMETHING ELSE ($before -> $after)"; fi ;;
    *)   inconc "the push never answered within 1800 s" ;;
  esac
  retry=$(inpod "DOOR=$DOOR NS=$NS /work/push.sh $name $SMALL $ref 2>&1 | tail -1")
  [ "$(snap_ref "$SMALL" "$ref")" = "$tip" ] && ok "the retry converged: the bucket names $ref at the tip" || bad "the retry did not converge: $retry"
  o=$(mpu_count "$pp")
  [ "${o:-0}" = 0 ] && ok "no incomplete upload under the prefix once the successor served" || bad "$o incomplete upload(s) survived the successor's claim"
  note "X6 on the wire: told '$res' after the roll, the old pod gone in ${t_gone} s; a roll that waited for the batch would have answered ok every time"
}

leg_S8() {
  leg S8 "CONTROL: receive.keepAlive=0 and the door's bound at ${CTRL_DOOR_SECS} s — the door cuts the client during a hook wait longer than the bound, so the bound is real and the keepalive is what carries a long push (X5's anti-vacuity; both settings restored after)"
  local name=gapctl-$RUN ref=agent/gapctl-$RUN tip before b0 t0 t1 rc out stats n first wait maxgap end after t_land
  wait_serving "$SMALL" 900 || { inconc "$SMALL is not serving"; return; }
  b0=$(door_bound); [ -n "$b0" ] || { inconc "no --upstream-timeout-secs among deploy/$DOOR_DEPLOY's args in $DOOR_NS (DOOR_NS/DOOR_DEPLOY)"; return; }
  set_door_bound "$CTRL_DOOR_SECS" || { inconc "could not set the door's bound to ${CTRL_DOOR_SECS} s"; return; }
  gitq "$SMALL" config receive.keepAlive 0 >/dev/null 2>&1
  [ "$(gitq "$SMALL" config receive.keepAlive 2>/dev/null)" = 0 ] || { inconc "could not set receive.keepAlive=0 on $SMALL"; set_door_bound "$b0"; return; }
  note "for the control: the door's bound ${b0} → ${CTRL_DOOR_SECS} s, receive.keepAlive 5 → 0 on $SMALL"
  tip=$(inpod "/work/build.sh $name $GAP_MB" | tail -1); before=$(snap_ref "$SMALL" "$ref")
  t0=$(now); out=$(inpod "DOOR=$DOOR NS=$NS /work/pushtrace.sh $name $SMALL $ref pkt-$name curl-$name"); rc=$?; t1=$(now)
  inpod "cat /work/pkt-$name" > "$WORK/pkt-$name" 2>/dev/null; inpod "cat /work/curl-$name" > "$WORK/curl-$name" 2>/dev/null
  set -- $(gap_stats "$WORK/pkt-$name" "$WORK/curl-$name"); n=${1:-0}; first=${2:-?}; wait=${3:-0}; maxgap=${4:-0}; end=${5:-0}
  # the batch runs on without its client: wait for the bucket to settle before restoring anything
  t_land=""; while [ $(( $(now) - t1 )) -lt 1800 ]; do [ "$(snap_ref "$SMALL" "$ref")" = "$tip" ] && { t_land=$(( $(now) - t0 )); break; }; sleep 5; done
  after=$(snap_ref "$SMALL" "$ref")
  gitq "$SMALL" config receive.keepAlive 5 >/dev/null 2>&1; set_door_bound "$b0"
  note "restored: receive.keepAlive=5, the door's bound ${b0} s"
  note "client answered after $(( t1 - t0 )) s (rc=$rc); $n packet(s) read after the upload, the last ${wait} s after it; the connection ended ${end} s after the upload; the bucket named the tip ${t_land:-never} s after the push began"
  [ -n "$t_land" ] && [ "$(( t_land - (t1 - t0) ))" -ge 0 ] || { inconc "the bucket never named the tip: the server side did not finish, nothing here is a bound's doing"; return; }
  if [ "$rc" != 0 ] && [ "${end%.*}" -ge $(( CTRL_DOOR_SECS - 5 )) ] && [ "${end%.*}" -le $(( CTRL_DOOR_SECS + 15 )) ]; then
    ok "CONTROL: without keepalives the door cut the client ${end} s after the pack left the wire (bound ${CTRL_DOOR_SECS} s): $(grep -o 'transfer closed[^"]*\|Recv failure[^"]*\|Empty reply[^"]*' "$WORK/curl-$name" | tail -1)"
    [ "$after" = "$tip" ] && note "…and the batch landed anyway ($(( t_land - (t1 - t0) )) s after the cut): told failed but durable (run 3 finding 3); a retry is a no-op"
  elif [ "$rc" = 0 ]; then
    bad "CONTROL FAILED: acknowledged through a ${wait} s wait with no keepalives and a ${CTRL_DOOR_SECS} s bound — the bound is not what this rig thinks it is, and S9 cannot tell a held keepalive from a moving one"
  else
    bad "the push failed but not at the bound (the connection ended ${end} s after the upload against ${CTRL_DOOR_SECS} s): $(printf '%s' "$out" | tail -1)"
  fi
}

leg_S9() {
  leg S9 "keepalives through the front: across a hook wait of at least ${GAP_MIN_WAIT} s the client sees a packet at least every ${GAP_MAX} s (run 3 finding 2: through fcgiwrap they arrived in one burst with the report); the door's bound is $(door_bound) s"
  local name=gap-$RUN ref=agent/gap-$RUN tip t0 t1 rc out n first wait maxgap after
  wait_serving "$SMALL" 900 || { inconc "$SMALL is not serving"; return; }
  [ "$(gitq "$SMALL" config receive.keepAlive 2>/dev/null)" = 5 ] && ok "PRECONDITION: receive.keepAlive=5 on $SMALL" \
    || bad "PRECONDITION: receive.keepAlive is '$(gitq "$SMALL" config receive.keepAlive 2>/dev/null)' on $SMALL"
  tip=$(inpod "/work/build.sh $name $GAP_MB" | tail -1)
  t0=$(now); out=$(inpod "DOOR=$DOOR NS=$NS /work/pushtrace.sh $name $SMALL $ref pkt-$name curl-$name"); rc=$?; t1=$(now)
  inpod "cat /work/pkt-$name" > "$WORK/pkt-$name" 2>/dev/null; inpod "cat /work/curl-$name" > "$WORK/curl-$name" 2>/dev/null
  set -- $(gap_stats "$WORK/pkt-$name" "$WORK/curl-$name"); n=${1:-0}; first=${2:-?}; wait=${3:-0}; maxgap=${4:-0}
  after=$(snap_ref "$SMALL" "$ref")
  [ "$rc" = 0 ] && [ "$after" = "$tip" ] && ok "push of ${GAP_MB} MiB acknowledged in $(( t1 - t0 )) s, and the bucket holds it" \
    || bad "push rc=$rc, bucket $after (want $tip): $(printf '%s' "$out" | tail -2 | tr '\n' ' ')"
  note "$n packet(s) reached the client after the pack left the wire: the first ${first} s after it, the report ${wait} s after it, the longest gap ${maxgap} s"
  if [ "$n" = 0 ] || [ "$first" = "?" ]; then inconc "no packet trace to judge (n=$n): GIT_TRACE_PACKET/GIT_TRACE_CURL wrote nothing usable"
  elif [ "${wait%.*}" -lt "$GAP_MIN_WAIT" ]; then inconc "the wait after the upload (${wait} s) is under ${GAP_MIN_WAIT} s: too few keepalives to judge — raise GAP_MB"
  elif python3 -c "import sys; sys.exit(0 if float('$maxgap') <= $GAP_MAX else 1)"; then ok "every gap ≤ ${GAP_MAX} s across a ${wait} s wait ($n packets): the keepalives cross the front as they are sent"
  else bad "a gap of ${maxgap} s inside a ${wait} s wait ($n packets, the first after ${first} s): the front held the keepalives (run 3 finding 2's class)"; fi
}

# clones.sh runs inside the agent pod (installed by install_agent_scripts):
#   clones.sh <repo> <ref> <n> <tag>: n concurrent single-branch clones of
#   <ref>, each into /work/clone-<tag>-<i>; per clone "<i> <start> <end>
#   <rc> <head>" to /work/clone-<tag>.log; prints the wall time.
upload_packs() { K -n "$NS" exec "$(repo_pod "$1")" -c git-http -- sh -c "pgrep -f 'upload-pack --stateless-rp[c]' | wc -l" 2>/dev/null | tr -d '[:space:]'; }
up_watch() { # repo outfile — the git container's upload-pack count, twice a second, until <outfile>.stop
  ( while [ ! -e "$2.stop" ]; do echo "$(now) $(upload_packs "$1")"; sleep 0.5; done ) > "$2" 2>/dev/null &
}

leg_S10() {
  leg S10 "$CLONE_N concurrent clones of a ${CLONE_MB} MiB branch are served concurrently, every one complete and at the tip, and a push beside them is acknowledged (the fleet's common case: many agents cloning one repository)"
  local name=clone-$RUN ref=agent/clone-$RUN tip t0 t1 solo n_ok n_bad peak wall agg pw pres ptip
  wait_serving "$SMALL" 900 || { inconc "$SMALL is not serving"; return; }
  tip=$(inpod "/work/build.sh $name $CLONE_MB" | tail -1)
  inpod "DOOR=$DOOR NS=$NS /work/push.sh $name $SMALL $ref" >/dev/null 2>&1
  [ "$(snap_ref "$SMALL" "$ref")" = "$tip" ] && ok "PRECONDITION: $ref holds a ${CLONE_MB} MiB history at $tip" || { inconc "the branch to clone did not land"; return; }
  # one clone alone, for the ratio
  t0=$(now); inpod "DOOR=$DOOR NS=$NS /work/clones.sh $SMALL $ref 1 solo-$RUN" >/dev/null 2>&1; t1=$(now); solo=$(( t1 - t0 ))
  note "one clone alone: ${solo} s ($(python3 -c "print(f'{$CLONE_MB/max($solo,1):.0f}')") MiB/s)"
  # the storm, with a push in the middle of it and the server's upload-packs watched
  up_watch "$SMALL" "$WORK/up-$RUN"; pw=$!
  inpod "/work/build.sh push-$name $CONC_MB" >/dev/null 2>&1
  t0=$(now)
  ( K -n "$NS" exec "$AGENT" -c agent -- sh -c "DOOR=$DOOR NS=$NS /work/clones.sh $SMALL $ref $CLONE_N storm-$RUN" > "$WORK/clones-$RUN.out" 2>&1 ) & cw=$!
  sleep 2; push_bg "push-$name" "$SMALL" "agent/push-$name" res-clonepush
  wait "$cw" 2>/dev/null; t1=$(now); wall=$(( t1 - t0 ))
  pres=$(wait_result res-clonepush 900)
  stop_watch "$WORK/up-$RUN" "$pw"
  inpod "cat /work/clone-storm-$RUN.log; true" > "$WORK/clone-storm-$RUN.log" 2>/dev/null
  n_ok=$(awk -v t="$tip" '$4==0 && $5==t' "$WORK/clone-storm-$RUN.log" | wc -l | tr -d ' ')
  n_bad=$(( CLONE_N - n_ok ))
  peak=$(max_col "$WORK/up-$RUN" 2)
  [ "$n_bad" = 0 ] && ok "all $CLONE_N concurrent clones completed at the tip $tip" \
    || bad "$n_bad of $CLONE_N clones failed or stopped short: $(awk '$4!=0 {printf "#%s rc=%s ", $1, $4}' "$WORK/clone-storm-$RUN.log")"
  [ "${peak:-0}" -ge "$CLONE_N" ] && ok "the git container ran $peak upload-pack(s) at the peak for $CLONE_N clients: nothing in front of git serialised them" \
    || bad "the git container peaked at ${peak:-0} upload-pack(s) for $CLONE_N clients: a ceiling in front of git queued the rest (X4's class, read side)"
  agg=$(python3 -c "print(f'{$CLONE_N*$CLONE_MB/max($wall,1):.0f}')")
  note "$CLONE_N clones in ${wall} s wall (${agg} MiB/s aggregate) against ${solo} s alone: $(python3 -c "print(f'{$wall/max($solo,1):.1f}')")× one clone's time for ${CLONE_N}× the bytes"
  [ "$wall" -lt $(( solo * CLONE_N )) ] && ok "the clones overlapped (wall < $CLONE_N × solo)" || bad "the clones ran one after another (wall ${wall} s ≥ $CLONE_N × ${solo} s)"
  ptip=$(snap_ref "$SMALL" "agent/push-$name")
  [ "$pres" = ACK ] && [ "$ptip" != "<absent>" ] && ok "a push in the middle of the storm was acknowledged and is in the bucket" \
    || bad "the push beside the storm: '$pres', bucket $ptip — $(push_output res-clonepush)"
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
