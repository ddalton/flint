#!/usr/bin/env bash
# LARGE REPOSITORY — the size regime every other forge drill misses.
#
# The suite's biggest payload is f1-durability's 48x256 KiB (12 MiB)
# and lfs.sh's 4 MiB, against a 64 MiB whole-put ceiling. So three
# shipped defects lived on paths no leg could reach:
#
#   * the multipart upload had never been executed by ANY test or leg;
#   * the restore held every pack TWICE (a flat 2.05x of object size,
#     ~20.5 GB at the 10 GB envelope of design §5) on the path that
#     runs at every pod start;
#   * a restore slow enough to outlast the takeover window loses the
#     repository to a challenger while its own pod is alive.
#
# None of those are subtle. They were invisible because every leg
# stands in the one size regime where they do not exist. This leg
# stands somewhere else.
#
#   ./run-largerepo.sh            # default 160 MiB of incompressible data
#   REPO_MB=512 ./run-largerepo.sh
#   KEEP=1 ./run-largerepo.sh     # keep $WORK for inspection
set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)
BUCKET=${BUCKET:-largerepo}
MINIO_NAME=${MINIO_NAME:-flint-largerepo-minio}
MINIO_PORT=${MINIO_PORT:-9101}
# shellcheck source=../composition/rig.sh
. "$HERE/../composition/rig.sh"

REPO_MB=${REPO_MB:-160}
CEILING=$((64 * 1024 * 1024))     # packio::WHOLE_PUT_MAX
INCONC=0
inconc() { INCONC=$((INCONC+1)); printf '  INCONCLUSIVE  %s\n' "$*"; }

# An inconclusive leg is NOT a pass. The rig's own verdict counts only
# PASS and FAIL, so a leg that could not measure what it exists to
# measure would report "0 failed" and read as green — the exact way a
# measurement rig lies (tests/k8s/oci-ab: "INCONCLUSIVE is not PASS").
lr_verdict() {
  verdict "large-repo"
  local rc=$?
  if [ "$INCONC" -gt 0 ]; then
    printf 'INCONCLUSIVE: %d leg(s) could not be decided — this run is NOT green.\n' "$INCONC"
    return 2
  fi
  return $rc
}

# ── precondition: the binary under test is the tree under test ───────
#
# `cargo build --bins` SILENTLY SKIPS flint-forge-syncer: it carries
# `required-features = ["s3"]`, so a plain build leaves whatever binary
# was there before at exactly the path FORGE_BIN points to. A drill run
# after one reports green about code that is not in the tree. Caught
# once for real while writing this leg.
binary_is_fresh() {
  head_ "precondition: FORGE_BIN is newer than the sources it claims to be"
  local newest
  newest=$(find "$REPO_ROOT/forge/syncer/src" "$REPO_ROOT/crates/flint-store/src" \
             -name '*.rs' -newer "$FORGE_BIN" 2>/dev/null | head -3)
  if [ -n "$newest" ]; then
    bad "FORGE_BIN is older than $(echo "$newest" | wc -l | tr -d ' ')+ source file(s):"
    echo "$newest" | sed 's/^/          /'
    say "        rebuild with:  cargo build --bins --features s3"
    return 1
  fi
  ok "FORGE_BIN is not older than any source under forge/syncer or flint-store"
  return 0
}

# Peak memory of a process, in bytes. macOS's `peak memory footprint`
# counts compressed pages; its RSS does NOT, and reading RSS here
# understated a 2 GiB fetch as 1.05 GB and PLATEAUED, which would have
# reported the defect this leg exists for as benign.
peak_bytes() {
  local out
  out=$(/usr/bin/time -l "$@" 2>&1 >/dev/null)
  local v
  v=$(printf '%s\n' "$out" | awk '/peak memory footprint/{print $1}')
  [ -n "$v" ] || v=$(printf '%s\n' "$out" | awk '/[Mm]aximum resident set size/{print $1}')
  printf '%s' "${v:-0}"
}

mib() { python3 -c "print(f'{int($1)/1048576:.1f}')"; }

# ── the repository ───────────────────────────────────────────────────
build_big_repo() {  # build_big_repo <clone-dir> <mib>
  local c=$1 mb=$2 i=0
  mkdir -p "$c/blobs"
  # Incompressible, so the pack is the size of the content: git would
  # otherwise deflate a synthetic pattern to nothing and the pack would
  # never reach the ceiling this leg is about.
  while [ $i -lt "$mb" ]; do
    dd if=/dev/urandom of="$c/blobs/b$i" bs=1048576 count=1 status=none
    i=$((i+1))
  done
  git_c "$c" add -A >/dev/null 2>&1
  git_c "$c" commit -q -m "a repository of $mb MiB" >/dev/null 2>&1
  git_c "$c" rev-parse HEAD
}

main() {
  rig_init || { say "rig_init failed"; return 1; }
  rig_gate || true
  binary_is_fresh || { lr_verdict; return 1; }

  local prefix="lr/A" bare="$WORK/bare.git" clone="$WORK/clone"
  # rig_purge takes PREFIXES. Passing the bucket name purges
  # s3://<bucket>/<bucket>, which is nothing, and the next run then
  # reads the previous run's snapshot and is refused "stale info".
  rig_purge "$prefix" 2>/dev/null || true
  new_bare_repo "$bare" || { bad "could not init the bare repo"; lr_verdict; return 1; }
  new_clone "$bare" "$clone" >/dev/null 2>&1
  git -C "$clone" config user.name driller; git -C "$clone" config user.email driller@invalid

  head_ "seeding a ${REPO_MB} MiB repository (incompressible)"
  local tip; tip=$(build_big_repo "$clone" "$REPO_MB")
  [ -n "$tip" ] && ok "seeded, tip $tip" || { bad "seed failed"; lr_verdict; return 1; }

  # ── L1: the pack crosses the ceiling and is composed ───────────────
  head_ "L1 — a pack above the whole-put ceiling is uploaded multipart"
  FORGE_SOCKET=/tmp/fc-L.sock forge_up L "$bare" "$prefix"
  wait_key "$prefix/git/snapshot" 60 >/dev/null 2>&1 || note "no snapshot yet (fresh prefix)"
  local out rc
  out=$(FORGE_SOCKET=/tmp/fc-L.sock push "$clone" "HEAD:refs/heads/main"); rc=$?
  if [ $rc -eq 0 ]; then ok "the push was accepted"; else
    bad "the push failed (rc=$rc)"; printf '%s\n' "$out" | sed 's/^/        /' | head -8
  fi

  local packkey size etag
  packkey=$(aws s3api list-objects-v2 --bucket "$BUCKET" --prefix "$prefix/git/objects/pack/" \
            --query 'Contents[?ends_with(Key,`.pack`)]|[0].Key' --output text 2>/dev/null)
  if [ -n "$packkey" ] && [ "$packkey" != "None" ]; then
    size=$(aws s3api head-object --bucket "$BUCKET" --key "$packkey" --query 'ContentLength' --output text 2>/dev/null)
    etag=$(aws s3api head-object --bucket "$BUCKET" --key "$packkey" --query 'ETag' --output text 2>/dev/null)
    if [ "${size:-0}" -gt "$CEILING" ]; then
      ok "the pack is $(mib "$size") MiB, above the $(mib $CEILING) MiB ceiling"
    else
      bad "the pack is only $(mib "${size:-0}") MiB — this leg did not reach the regime it tests"
    fi
    # S3 and MinIO mark a multipart object's ETag with a -<partcount>
    # suffix. A whole PUT has a bare MD5. This is the only externally
    # visible proof of WHICH upload path ran.
    case "$etag" in
      *-[0-9]*) ok "the ETag carries a part count ($etag) — it was composed, not whole-PUT" ;;
      *)        bad "the ETag has no part count ($etag) — the whole-put path ran above its ceiling" ;;
    esac
  else
    bad "no pack object under $prefix/git/objects/pack/"
  fi
  forge_down L

  # ── L2: the restore's memory does not scale with the repository ────
  head_ "L2 — a restore does not need as much memory as the repository is big"
  local fresh="$WORK/restored.git"
  new_bare_repo "$fresh" || bad "could not init the restore target"
  # There is no exit-after-restore knob and inventing one would be a
  # test of a knob. The socket is opened only AFTER restore returns
  # (server.rs: restore, then serve), so its appearance IS the signal
  # that the measured work is done; the process is then killed and
  # /usr/bin/time reports on exit either way.
  local tf="$WORK/time-restore.txt" sock=/tmp/fc-R.sock
  rm -f "$sock"
  ( export FLINT_FORGE_BUCKET="$BUCKET" FLINT_FORGE_PREFIX="$prefix" \
           FLINT_FORGE_REPO="$fresh" FLINT_FORGE_ENDPOINT="$ENDPOINT" \
           FLINT_FORGE_HOOKS_PATH="$fresh/hooks-flint" \
           FLINT_FORGE_SOCKET="$sock" \
           FLINT_FORGE_STATUS_ADDR="127.0.0.1:$((9848 + RANDOM % 900))" \
           FLINT_FORGE_HEARTBEAT_SECS=2 FLINT_FORGE_BATCH_WINDOW_MS=200 \
           FLINT_FORGE_SYNC_BIN="$SYNC_BIN"
    exec /usr/bin/time -l "$FORGE_BIN" ) > "$WORK/restore.log" 2> "$tf" &
  local tpid=$!
  local waited=0
  while [ ! -S "$sock" ] && [ $waited -lt 180 ]; do sleep 1; waited=$((waited+1)); done
  if [ ! -S "$sock" ]; then
    inconc "the restore never opened its socket in ${waited}s — nothing was measured"
    kill -9 $tpid 2>/dev/null; wait $tpid 2>/dev/null
  else
    note "restore completed in ~${waited}s"
    # Signal time's CHILD, not time: SIGTERM to the reporter kills it
    # before it reports, which is how this leg first came back
    # "no peak-memory figure on this platform" on a platform that has one.
    local kid; kid=$(pgrep -P "$tpid" 2>/dev/null | head -1)
    [ -n "$kid" ] && kill "$kid" 2>/dev/null || kill "$tpid" 2>/dev/null
    wait $tpid 2>/dev/null
    local peak
    peak=$(awk '/peak memory footprint/{print $1}' "$tf")
    [ -n "$peak" ] || peak=$(awk '/[Mm]aximum resident set size/{print $1}' "$tf")
    if [ -z "$peak" ]; then
      inconc "no peak-memory figure from /usr/bin/time on this platform"
    elif [ "${size:-0}" -gt 0 ] && [ "$peak" -lt "$size" ]; then
      ok "restore peaked at $(mib "$peak") MiB for a $(mib "$size") MiB pack"
    else
      bad "restore peaked at $(mib "$peak") MiB for a $(mib "${size:-0}") MiB pack — memory scales with the repository"
    fi
    # The restore must also be CORRECT, not merely cheap.
    if git -C "$fresh" fsck --strict --no-progress >/dev/null 2>&1; then
      ok "the restored repository passes git fsck --strict"
    else
      bad "the restored repository fails git fsck --strict"
    fi
    if [ "$(git -C "$fresh" rev-parse refs/heads/main 2>/dev/null)" = "$tip" ]; then
      ok "the restored main is at the pushed tip"
    else
      bad "the restored main is not at the pushed tip"
    fi
  fi

  lr_verdict
}

trap 'rig_clean' EXIT
main "$@"
