#!/usr/bin/env bash
# The PUBLISH-PATH drill: does lean still read every file twice, does a
# store-computed checksum now survive parallel part uploads, does a
# scoped checkout actually transfer only what it admitted, and does the
# cross-key copy behave against REAL S3 rather than against a double?
#
# WHAT THIS CAN AND CANNOT ANSWER, BY WHERE IT RUNS
#
#   LOCAL (MinIO on loopback) is a RIG SHAKEDOWN, never a result. The
#   single-read change removes a DISK read, and loopback's disk is the
#   same disk the upload is not contending for — so locally the arms sit
#   within noise and that INCONCLUSIVE is one keystroke from being
#   written down as "no effect". Local asserts rig VALIDITY only: the
#   read counters differ, the byte guards hold, L4 passes.
#
#   LIVE (i4i.large spot, one AZ, S3 gateway endpoint) is the
#   measurement. The 2026-09-10 door drill established the floor this
#   sits on: the NVMe does 263-279 MB/s and parallelism does not move it,
#   so a read the publish does not have to do is time the publish does
#   not have to spend.
#
# ARMS ARE INTERLEAVED WITHIN EACH REP, never batched by arm. A batched
# run attributes every drift in spot neighbours, S3 weather and page
# cache to whichever arm held that stretch of wall clock.
#
# ─────────────────────────────────────────────────────────────────────
# L1 — the single read.  `single` = HEAD.  `prepass` = HEAD with
#      `publish-prepass-revert.patch` applied, i.e. the ACTUAL old code,
#      not a knob. A knob would prove a branch works, not that the
#      shipped path changed.
#
#      THE NULL CONTROL IS `small`. Files at or under whole_put_max
#      (64 MiB) never reach `upload_compose` at all — they go through
#      `put_whole`, which never had a pre-pass. So `small` MUST NOT MOVE.
#      An arm that "improves" small too is measuring the weather, and
#      this drill reports INCONCLUSIVE rather than a number.
#
#      The DIRECT oracle is not the clock: /proc/<pid>/io read_bytes.
#      `prepass` must read ~2x the tree, `single` ~1x. If that ratio is
#      not there, nothing else in L1 means anything — the binaries are
#      the same binary, or the workload never took the compose path.
#
# L2 — parallel upload WITH a store-computed checksum. This combination
#      was refused by construction until `crc64_combine`, so it has never
#      run anywhere. The correctness oracle is FREE and strong: S3
#      validates the full-object CRC at CompleteMultipartUpload, so a
#      mis-folded checksum is a FAILED publish, not a silent corruption.
#
#      ANTI-VACUITY: that oracle is only worth something if the
#      validation is actually on. `L2c` publishes with a deliberately
#      wrong checksum and REQUIRES a failure. If a wrong CRC publishes
#      fine, S3 is not checking and every L2 pass means nothing.
#
# L3 — scoped checkout. 2001 files, a scope admitting 3.
#      Oracle: bytes on disk, files on disk, and read_bytes.
#      Controls: (a) the unscoped arm materialises all 2001 — if it does
#      not, the tree is wrong, not the feature; (b) a scope admitting
#      EVERYTHING must equal the unscoped arm, which is what separates
#      "the filter works" from "the filter always yields nothing"; (c)
#      after TWO barriers the 1,998 unadmitted citations must survive —
#      one barrier would pass for the wrong reason, because a deletion
#      must survive two consecutive scans before it is published.
#
# L4 — the cross-key copy, against real S3. `flint-sync probe-copy`
#      checks: a stale copy-source etag REFUSES and lands nothing; the
#      copy is byte- and checksum-identical; the destination's stamps are
#      ITS OWN (MetadataDirective is a server behaviour no double can
#      verify); the source survives; an occupied destination refuses.
#      Run TWICE — once over the CopyObject path, once with
#      FLINT_SYNC_COPY_WHOLE_MAX_MB=1 forcing MPU + UploadPartCopy, which
#      is the arm that has never executed anywhere and the first caller
#      `ComposeSpec::base_key` has ever had.
# ─────────────────────────────────────────────────────────────────────
set -euo pipefail

BUCKET="${BUCKET:?set BUCKET}"
PREFIX="${PREFIX:-publish-drill}"
REPS="${REPS:-3}"
ROOT="${ROOT:-/mnt/drill}"
OUT="${OUT:-/tmp/publish-drill-$(date +%Y%m%d-%H%M%S)}"
SINGLE_BIN="${SINGLE_BIN:-/usr/local/bin/flint-sync}"
PREPASS_BIN="${PREPASS_BIN:-/usr/local/bin/flint-sync-prepass}"

mkdir -p "$OUT"
say() { printf '%s  %s\n' "$(date -u +%H:%M:%SZ)" "$*" | tee -a "$OUT/log"; }

for b in "$SINGLE_BIN" "$PREPASS_BIN"; do
  [ -x "$b" ] || { say "FATAL: $b missing"; exit 2; }
done
# The two binaries must DIFFER. Building the control arm from a patch is
# exactly the step that silently no-ops, and two identical binaries would
# produce a clean, symmetric, meaningless result.
if cmp -s "$SINGLE_BIN" "$PREPASS_BIN"; then
  say "FATAL: the two arms are the SAME BINARY — the patch did not apply"
  exit 2
fi

# ── seed ─────────────────────────────────────────────────────────────
# The seed writes to a FIXED key space, so a REUSED bucket carries a
# previous run's objects and the checkout it feeds is not cold. Refuse
# rather than measure a warm tree that looks cold.
seed() {
  local name=$1 dir="$ROOT/$name"
  if [ -d "$dir" ] && [ -n "$(ls -A "$dir" 2>/dev/null)" ]; then
    say "FATAL: $dir is not empty — a reused tree is not a cold one"; exit 2
  fi
  mkdir -p "$dir"
  case "$name" in
    big)    for i in $(seq 1 6); do
              dd if=/dev/urandom of="$dir/w$i.bin" bs=1M count=1024 status=none; done ;;
    mixed)  for i in $(seq 1 4); do
              dd if=/dev/urandom of="$dir/w$i.bin" bs=1M count=1024 status=none; done
            for i in $(seq 1 2000); do
              dd if=/dev/urandom of="$dir/f$i.dat" bs=4k count=1 status=none; done ;;
    small)  for i in $(seq 1 20000); do
              dd if=/dev/urandom of="$dir/s$i.dat" bs=8k count=1 status=none; done ;;
    scoped) mkdir -p "$dir/inputs"
            for i in $(seq 1 3); do
              dd if=/dev/urandom of="$dir/inputs/in$i.bin" bs=1M count=4 status=none; done
            for i in $(seq 1 1998); do
              dd if=/dev/urandom of="$dir/out$i.dat" bs=1M count=1 status=none; done ;;
  esac
  # Sum FILE sizes, never `du -s`: du counts the directory inode too
  # (+4096) and the byte guard would fail for a reason that is not S3.
  find "$dir" -type f -printf '%s\n' | awk '{n+=$1} END {print n}'
}

# read_bytes for a command. /proc/<pid>/io DISAPPEARS the moment the
# process exits, so reading it after `wait` yields nothing and the guard
# below would compare 0 against 0 and pass. A poller keeps the last
# readable snapshot; the counter is monotonic, so the last one is the
# total.
timed() {
  local label=$1; shift
  local t0 t1 rb
  t0=$(date +%s.%N)
  "$@" > "$OUT/$label.out" 2> "$OUT/$label.err" &
  local pid=$!
  ( while [ -r "/proc/$pid/io" ]; do
      cp "/proc/$pid/io" "$OUT/$label.io" 2>/dev/null || true
      sleep 0.2
    done ) &
  local poller=$!
  local rc=0; wait $pid || rc=$?
  kill $poller 2>/dev/null || true; wait $poller 2>/dev/null || true
  t1=$(date +%s.%N)
  if [ $rc -ne 0 ]; then say "  $label FAILED rc=$rc (see $OUT/$label.err)"; return $rc; fi
  rb=$(awk '/^read_bytes:/ {print $2}' "$OUT/$label.io" 2>/dev/null || echo 0)
  printf '%s\t%s\t%s\n' "$label" "$(echo "$t1 - $t0" | bc)" "$rb" >> "$OUT/results.tsv"
  say "  $label  $(echo "$t1 - $t0" | bc)s  read_bytes=$rb"
}

env_common() {
  export FLINT_SYNC_BUCKET="$BUCKET" FLINT_SYNC_ROOT="$1" FLINT_SYNC_PREFIX="$2"
  export FLINT_SYNC_FLOOR_SECS=1 FLINT_SYNC_MAX_BYTES=0 FLINT_SYNC_MAX_FILES=0
}

# ── L1: the single read ──────────────────────────────────────────────
leg1() {
  local wl=$1 bytes=$2
  for rep in $(seq 1 "$REPS"); do
    for arm in single prepass; do        # INTERLEAVED, not batched
      local dir="$ROOT/$wl" pfx="$PREFIX/l1/$wl/$arm/$rep"
      rm -rf "$dir/.flint-sync"
      env_common "$dir" "$pfx"
      local bin=$SINGLE_BIN; [ "$arm" = prepass ] && bin=$PREPASS_BIN
      timed "l1-$wl-$arm-$rep" "$bin" barrier || true
    done
  done
  # THE GUARD. prepass must read ~2x the tree, single ~1x. Without this
  # ratio the clock numbers describe two runs of the same code.
  local s p
  s=$(awk -v w="l1-$wl-single" '$1 ~ w {n+=$3; c++} END {print (c? n/c : 0)}' "$OUT/results.tsv")
  p=$(awk -v w="l1-$wl-prepass" '$1 ~ w {n+=$3; c++} END {print (c? n/c : 0)}' "$OUT/results.tsv")
  say "L1/$wl read_bytes: single=$s prepass=$p tree=$bytes"
  awk -v s="$s" -v p="$p" -v b="$bytes" 'BEGIN{
    if (b==0 || s==0) {print "  INCONCLUSIVE: no read accounting"; exit}
    r = p/s
    printf "  prepass/single read ratio = %.2f\n", r
    if (r < 1.5) print "  *** GUARD FAILED: the arms did not differ in bytes read."
    else         print "  guard OK: the control arm really does read the file twice."
  }'
}

# ── L2: parallel upload WITH a store-computed checksum ───────────────
leg2() {
  local dir="$ROOT/big"
  for rep in $(seq 1 "$REPS"); do
    for par in 1 8; do
      rm -rf "$dir/.flint-sync"
      env_common "$dir" "$PREFIX/l2/par$par/$rep"
      FLINT_SYNC_UPLOAD_PART_PARALLELISM=$par \
        timed "l2-par$par-$rep" "$SINGLE_BIN" barrier || true
    done
  done
  say "L2: every publish above completed CompleteMultipartUpload, which"
  say "    S3 refuses on a full-object checksum mismatch. A pass here is"
  say "    the fold being right, not just the upload being fast."
}

# L2c: the anti-vacuity arm for L2's oracle. If S3 is NOT validating
# full-object checksums, every L2 pass is worthless — so prove the
# validation fires. Deliberately driven with the AWS CLI and NOT with a
# corruption knob in flint-sync: the question is whether S3 checks, and
# shipping a "publish wrong bytes" switch in the real binary to ask it
# would be a worse answer than no answer.
leg2_control() {
  say "L2c: completing an MPU with a DELIBERATELY WRONG CRC; S3 must refuse"
  local k="$PREFIX/l2c/probe.bin" f="$OUT/l2c.bin"
  dd if=/dev/urandom of="$f" bs=1M count=8 status=none
  local real wrong up etag
  real=$(aws s3api put-object --bucket "$BUCKET" --key "$k.tmp" --body "$f" \
           --checksum-algorithm CRC64NVME --query ChecksumCRC64NVME --output text 2>/dev/null || echo "")
  aws s3api delete-object --bucket "$BUCKET" --key "$k.tmp" >/dev/null 2>&1 || true
  if [ -z "$real" ]; then
    say "  INCONCLUSIVE: this endpoint did not return a CRC64NVME at all."
    say "  L2's oracle cannot be trusted here; treat L2 as UNVERIFIED."
    return
  fi
  # Flip one base64 character to get a well-formed but WRONG checksum.
  wrong=$(printf '%s' "$real" | sed 's/^./Z/')
  [ "$wrong" = "$real" ] && wrong=$(printf '%s' "$real" | sed 's/^./Y/')
  up=$(aws s3api create-multipart-upload --bucket "$BUCKET" --key "$k" \
         --checksum-algorithm CRC64NVME --query UploadId --output text)
  etag=$(aws s3api upload-part --bucket "$BUCKET" --key "$k" --part-number 1 \
           --upload-id "$up" --body "$f" --query ETag --output text)
  if aws s3api complete-multipart-upload --bucket "$BUCKET" --key "$k" \
       --upload-id "$up" --checksum-crc64nvme "$wrong" \
       --multipart-upload "{\"Parts\":[{\"PartNumber\":1,\"ETag\":$etag}]}" \
       > "$OUT/l2c.out" 2>&1; then
    say "  *** GUARD FAILED: a WRONG full-object checksum completed."
    say "      S3 is not validating, so every L2 pass proves nothing."
    aws s3api delete-object --bucket "$BUCKET" --key "$k" >/dev/null 2>&1 || true
  else
    say "  guard OK: S3 rejected the mismatched checksum (see $OUT/l2c.out)"
    aws s3api abort-multipart-upload --bucket "$BUCKET" --key "$k" \
      --upload-id "$up" >/dev/null 2>&1 || true
  fi
}

# ── L3: scoped checkout ──────────────────────────────────────────────
leg3() {
  local src="$ROOT/scoped"
  env_common "$src" "$PREFIX/l3"
  "$SINGLE_BIN" barrier >> "$OUT/l3-seed.out" 2>&1   # publish 2001 files once
  for rep in $(seq 1 "$REPS"); do
    for arm in unscoped scoped wide; do   # `wide` = a scope admitting ALL
      local dir="$ROOT/l3-$arm-$rep"; rm -rf "$dir"; mkdir -p "$dir"
      env_common "$dir" "$PREFIX/l3"
      case $arm in
        unscoped) unset FLINT_SYNC_CHECKOUT_SCOPE ;;
        scoped)   export FLINT_SYNC_CHECKOUT_SCOPE="inputs" ;;
        wide)     export FLINT_SYNC_CHECKOUT_SCOPE="inputs,out1.dat" ;;
      esac
      timed "l3-$arm-$rep" "$SINGLE_BIN" checkout || true
      local n b
      n=$(find "$dir" -type f -not -path '*/.flint-sync/*' | wc -l)
      b=$(find "$dir" -type f -not -path '*/.flint-sync/*' -printf '%s\n' | awk '{n+=$1} END{print n+0}')
      say "  l3-$arm-$rep files=$n bytes=$b"
      printf 'l3-%s-%s\tfiles\t%s\tbytes\t%s\n' "$arm" "$rep" "$n" "$b" >> "$OUT/l3.tsv"
    done
  done
  say "L3 guards: unscoped must be 2001 files; scoped must be 3; and"
  say "    scoped < unscoped in BOTH bytes and read_bytes. A scoped arm"
  say "    that matches unscoped means the filter never ran."
  # The safety claim, on a real bucket: two barriers, nothing deleted.
  local dir="$ROOT/l3-scoped-1"
  env_common "$dir" "$PREFIX/l3"
  export FLINT_SYNC_CHECKOUT_SCOPE="inputs"
  "$SINGLE_BIN" barrier >> "$OUT/l3-b1.out" 2>&1 || true
  "$SINGLE_BIN" barrier >> "$OUT/l3-b2.out" 2>&1 || true
  say "L3 safety: after TWO barriers from a 3-of-2001 workspace, the"
  say "    manifest must still carry 2001 entries. ONE barrier would pass"
  say "    for the wrong reason — a delete must survive two scans."
}

# ── L4: the cross-key copy against REAL S3 ───────────────────────────
leg4() {
  env_common "$ROOT/big" "$PREFIX/l4"
  say "L4a: CopyObject arm"
  FLINT_SYNC_COPY_WHOLE_MAX_MB=5120 "$SINGLE_BIN" probe-copy 2>&1 | tee -a "$OUT/l4a.out" || true
  say "L4b: MPU + UploadPartCopy arm (first execution anywhere)"
  FLINT_SYNC_COPY_WHOLE_MAX_MB=1 "$SINGLE_BIN" probe-copy 2>&1 | tee -a "$OUT/l4b.out" || true
}

# ── §RUN ─────────────────────────────────────────────────────────────
say "seeding"
B_BIG=$(seed big); B_MIXED=$(seed mixed); B_SMALL=$(seed small); seed scoped > /dev/null
say "seeded: big=$B_BIG mixed=$B_MIXED small=$B_SMALL"

leg1 big   "$B_BIG"
leg1 mixed "$B_MIXED"
leg1 small "$B_SMALL"    # the NULL control: this one must NOT move
leg2
leg2_control
leg3
leg4

say "done. results: $OUT/results.tsv  $OUT/l3.tsv  log: $OUT/log"
say "REMINDER: L1/small is the null control. If it moved, L1/big and"
say "          L1/mixed are weather, not a result."
