# Offset-stamped payload generator for the pNFS drills.
#
# Emits exactly `blocks` × 4096 bytes on stdout. Every 4 KiB block opens
# with a 64-byte header naming the block's OWN offset and the file it
# belongs to, dot-padded, then a fixed non-zero filler:
#
#   flint-pnfs v1 off=1052672 f=d-7.................<4032 bytes of filler>
#
# WHY NOT ZEROS (what this replaces). A flint DS stores only its slice
# of a striped file: the byte ranges routed to other DSes are left as
# sparse HOLES (`pnfs/ds/io.rs` — "bytes the kernel routes to other DSes
# become sparse holes"). A hole reads back as zeros. So with an
# all-zeros payload a read served from the WRONG DS — the dominant
# stripe-map failure — returns exactly the bytes the oracle expects, and
# the drill passes with a completely broken layout. The old
# `ZEROS_SHA` check could not distinguish "correct data" from "no data
# at all"; every one of these drills was blind to it.
#
# Stamping makes all four failure shapes loud:
#   * misrouted read      → hole → zeros → sha mismatch, header empty
#   * misaligned write    → header names a different offset
#   * cross-file mixing   → header names a different file (this is the
#                           class the ADR 0004 bench caught as silent
#                           corruption when DSes rebased by basename)
#   * unconfirmed truncate → stale stamped bytes survive past EOF
#
# The filler is non-zero for the same reason as the header: no part of a
# block may be able to impersonate a hole.
#
# Deliberately POSIX-awk only — this runs under busybox awk inside the
# writer pod.
#
#   awk -v fid=<file-id> -v blocks=<n> -f stamp.awk

BEGIN {
    blk = 4096
    hdr = 64

    if (fid == "")      { print "stamp.awk: -v fid= is required"    > "/dev/stderr"; exit 1 }
    if (blocks + 0 <= 0) { print "stamp.awk: -v blocks= must be > 0" > "/dev/stderr"; exit 1 }

    # Doubling beats appending: both strings are built once.
    dots = "."
    while (length(dots) < hdr) dots = dots dots
    fill = "0123456789abcdef"
    while (length(fill) < blk - hdr) fill = fill fill
    fill = substr(fill, 1, blk - hdr)

    # A header that overflows `hdr` would be silently truncated by the
    # substr below, dropping the offset and turning the stamp back into
    # the rubber stamp this file exists to replace. Refuse instead —
    # check the LAST (longest) header this run would emit.
    probe = sprintf("flint-pnfs v1 off=%d f=%s", (blocks - 1) * blk, fid)
    if (length(probe) > hdr) {
        printf "stamp.awk: header is %d bytes, limit %d (fid '%s' too long)\n", \
            length(probe), hdr, fid > "/dev/stderr"
        exit 1
    }

    for (b = 0; b < blocks; b++) {
        h = sprintf("flint-pnfs v1 off=%d f=%s", b * blk, fid)
        printf "%s%s", substr(h dots, 1, hdr), fill
    }
}
