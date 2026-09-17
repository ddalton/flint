#!/usr/bin/env bash
# PROOF 3 — BREADTH. git is a tmp-write-then-rename storm: many distinct
# files, rename-over, index lock files. A different path than sqlite.
# If descriptors are bounded here too, the fix is not sqlite-shaped.
set -u
P=$(pgrep -f flint-pnfs-mds | head -1); [ -n "$P" ] || { echo "NO HUB"; exit 2; }
N=40
B=$(sudo ls /proc/$P/fd | wc -l)
echo "fds before git: $B"
rm -rf /mnt/flint/repo
mkdir -p /mnt/flint/repo || { echo "mkdir FAILED"; exit 2; }
cd /mnt/flint/repo || { echo "cd FAILED — the rest would run in the wrong directory"; exit 2; }
git init -q . && git config user.email a@b.c && git config user.name t || { echo "git init FAILED"; exit 2; }
for i in $(seq 1 $N); do
  echo "content $i" > "f$i.txt"
  git add "f$i.txt" >/dev/null 2>&1 && git commit -q -m "c$i" >/dev/null 2>&1
  [ $((i % 10)) -eq 0 ] && echo "  at $i commits: fds=$(sudo ls /proc/$P/fd | wc -l)"
done
C=$(git rev-list --count HEAD 2>/dev/null || echo 0)
echo "commits: $C  expected: $N"
git fsck --no-progress >/dev/null 2>&1 && FSCK=CLEAN || FSCK=FAILED
echo "git fsck: $FSCK"
cd /; sleep 5
F=$(sudo ls /proc/$P/fd | wc -l)
echo "fds after $N commits: $F  baseline=$B  delta=$((F-B))"
if [ "$C" != "$N" ]; then echo "VERDICT=INCONCLUSIVE (only $C/$N commits landed)"; exit 1; fi
[ "$FSCK" = CLEAN ] || { echo "VERDICT=FAIL (repository corrupt)"; exit 1; }
echo "VERDICT=MEASURED"
