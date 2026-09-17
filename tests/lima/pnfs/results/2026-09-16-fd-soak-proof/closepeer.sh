#!/usr/bin/env bash
# THE HAZARD OPTION A INTRODUCES.
#
# Two stateids now share ONE descriptor. The cache's own invariant says
# an OPEN stateid's entry "must not be taken from a peer that still holds
# the file open" (fd_cache.rs:538-540). Under Option A the Arc is the
# refcount, so a peer's CLOSE must drop a REFERENCE, not the descriptor.
#
# If that is wrong, reader B gets EIO/EBADF or wrong bytes after A closes.
# This drill is designed to FAIL if it is wrong.
set -u
M=/mnt/flint
rm -f $M/peer.dat
# 4 MiB of known content
head -c 4194304 /dev/urandom > $M/peer.dat
WANT=$(md5sum < $M/peer.dat | cut -d' ' -f1)

# Reader B holds the file open for the whole drill, reading slowly.
( exec 9< $M/peer.dat
  for i in $(seq 1 20); do dd if=/dev/fd/9 bs=64k count=1 iflag=fullblock >/dev/null 2>&1 || true; sleep 0.5; done
  exec 9<&- ) &
BPID=$!
sleep 1
# Reader A opens the SAME file, reads it, then closes (process exit).
for i in $(seq 1 10); do
  A=$(md5sum < $M/peer.dat | cut -d' ' -f1)
  [ "$A" = "$WANT" ] || { echo "PEER DRILL: reader A got WRONG BYTES on pass $i"; kill $BPID 2>/dev/null; exit 1; }
done
# A has now closed 10 times while B held the file open.
sleep 1
# B must still be able to read the file correctly.
C=$(md5sum < $M/peer.dat | cut -d' ' -f1)
wait $BPID 2>/dev/null
D=$(md5sum < $M/peer.dat | cut -d' ' -f1)
echo "want=$WANT"
echo "after peer closes   =$C"
echo "after peer exits    =$D"
if [ "$C" = "$WANT" ] && [ "$D" = "$WANT" ]; then
  echo "PEER DRILL: PASS (a peer's CLOSE did not take the descriptor)"
else
  echo "PEER DRILL: FAIL (shared descriptor was yanked)"; exit 1
fi
