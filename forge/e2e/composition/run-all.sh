#!/usr/bin/env bash
# Run every composition drill (C1-C7) and summarise.
#
#   bash forge/e2e/composition/run-all.sh          # run them
#   DOWN=1 bash forge/e2e/composition/run-all.sh   # ...then stop MinIO
#
# Each drill is self-contained: it brings its own repository, its own
# prefixes and its own processes up and tears them down. They share
# only the MinIO container and the bucket, and they never share a
# prefix with each other.
set -uo pipefail
cd "$(dirname "$0")/../../.."
d=forge/e2e/composition
tp=0; tf=0; tk=0
for drill in c1-shared-prefix c2-export-contention c3-foreign-write \
             c4-three-readers c5-cited-delete c6-undo c7-prewarm; do
  printf '\n\n######## %s ########\n' "$drill"
  out=$(bash "$d/$drill.sh" 2>&1 | grep -v 'Terminated: 15\|Killed: 9')
  printf '%s\n' "$out"
  line=$(printf '%s' "$out" | grep -E '^[A-Z0-9]+: [0-9]+ passed' | tail -1)
  p=$(printf '%s' "$line" | sed 's/.*: \([0-9]*\) passed.*/\1/')
  f=$(printf '%s' "$line" | sed 's/.*passed, \([0-9]*\) failed.*/\1/')
  k=$(printf '%s' "$line" | sed -n 's/.*failed, \([0-9]*\) accepted.*/\1/p')
  tp=$((tp + ${p:-0})); tf=$((tf + ${f:-0})); tk=$((tk + ${k:-0}))
done
printf '\n\n######## composition suite ########\n'
printf 'total: %d passed, %d failed, %d accepted\n' "$tp" "$tf" "$tk"
printf '\nPASS means the composition rule held. KNOWN is a condition the system\n'
printf 'permits and nobody is fixing — named in design doc section 17, counted\n'
printf 'here so a NEW failure is visible instead of being lost among them.\n'
printf 'STALE means an accepted condition stopped reproducing, which is also\n'
printf 'something a human has to look at.\n'
if [ "$tf" -eq 0 ]; then
  printf '\nGREEN: %d accepted conditions outstanding, nothing unexpected.\n' "$tk"
else
  printf '\nRED: %d unexpected outcome(s), not among the accepted conditions.\n' "$tf"
fi
[ "${DOWN:-0}" = "1" ] && docker rm -f flint-composition-minio >/dev/null 2>&1 && echo "MinIO stopped"
[ "$tf" -eq 0 ]
