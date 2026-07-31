#!/bin/sh
#
# ds-stripe-check.sh — runs INSIDE a flint-pnfs DS pod (busybox sh).
# Piped in by `verify_ds_stripes` in lib.sh:
#
#   kubectl exec -i -n NS <ds-pod> -- sh -s -- <prefix> <blk> <blocks> <samples>
#
# THE STRIPE-MAP INVARIANT. A DS holds only its slice of each striped
# file; the byte ranges belonging to other DSes are sparse holes. So on
# any DS, every 4 KiB block of a drill file is either a hole or the real
# thing — and if it is real, its stamp must name the offset it actually
# sits at and the file it actually belongs to. Checking that needs no
# knowledge of the stripe width or the device order, which is the point:
# it does not re-derive the layout the MDS computed, so it can catch the
# MDS computing it wrongly.
#
# What a violation means:
#   header names another offset → the client wrote at the wrong offset,
#                                 or the DS rebased the write (pattern_
#                                 offset / stripe_unit disagreement)
#   header names another file   → two files share backing bytes (the
#                                 basename-collision class)
#   nothing but holes           → this DS never received a stripe; the
#                                 caller decides whether that is a bug,
#                                 but it is NEVER reported as a pass
#
# Exits 0 whatever it finds — the caller reads the summary lines. It
# prints STRIPES-OK only when it examined at least one real block and
# every real block it examined was correctly placed.

set -u

PFX=${1:?prefix}
BLK=${2:?block size}
BLOCKS=${3:?blocks per file}
SAMPLES=${4:?samples per file}

STRIDE=$(( BLOCKS / SAMPLES ))
[ "$STRIDE" -ge 1 ] || STRIDE=1

files=$(find /data -name "${PFX}-*.bin" 2>/dev/null | sort)
if [ -z "$files" ]; then
  echo "NO-FILES no ${PFX}-*.bin under /data"
  exit 0
fi

nf=0; seen=0; data=0; holes=0; bad=0

for f in $files; do
  nf=$(( nf + 1 ))
  base=${f##*/}
  base=${base%.bin}
  b=0
  while [ "$b" -lt "$BLOCKS" ]; do
    seen=$(( seen + 1 ))
    want="flint-pnfs v1 off=$(( b * BLK )) f=${base}"
    # NULs vanish through command substitution, so a hole comes back as
    # the empty string. Dots are kept: `want` is a prefix of the padded
    # header, so a prefix match is exact up to the padding.
    got=$(dd if="$f" bs="$BLK" skip="$b" count=1 2>/dev/null | head -c 64 | tr -d '\000')
    if [ -z "$got" ]; then
      holes=$(( holes + 1 ))
    else
      case "$got" in
        "${want}"*) data=$(( data + 1 )) ;;
        *) bad=$(( bad + 1 ))
           echo "STRIPE-MISMATCH ${f} block ${b} (offset $(( b * BLK )))"
           echo "  want '${want}'"
           echo "  got  '${got}'" ;;
      esac
    fi
    b=$(( b + STRIDE ))
  done
done

# Always state the coverage: a check that sampled every STRIDE-th block
# has NOT looked at the rest, and a summary that hid that would read as
# "all clean" when it means "the 16 blocks I opened were clean".
echo "SCANNED files=${nf} blocks_examined=${seen} of $(( nf * BLOCKS )) (every ${STRIDE}th) data=${data} holes=${holes} mismatched=${bad}"

if [ "$bad" -eq 0 ] && [ "$data" -gt 0 ]; then
  echo "STRIPES-OK"
elif [ "$bad" -eq 0 ]; then
  echo "ALL-HOLES every block examined was a hole — this DS proved nothing"
fi
