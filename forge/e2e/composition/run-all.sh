#!/usr/bin/env bash
# Run every composition drill (C1-C5) and summarise.
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
tp=0; tf=0
for drill in c1-shared-prefix c2-export-contention c3-foreign-write \
             c4-three-readers c5-cited-delete; do
  printf '\n\n######## %s ########\n' "$drill"
  out=$(bash "$d/$drill.sh" 2>&1 | grep -v 'Terminated: 15\|Killed: 9')
  printf '%s\n' "$out"
  line=$(printf '%s' "$out" | grep -E '^[A-Z0-9]+: [0-9]+ passed' | tail -1)
  p=$(printf '%s' "$line" | sed 's/.*: \([0-9]*\) passed.*/\1/')
  f=$(printf '%s' "$line" | sed 's/.*passed, \([0-9]*\) failed.*/\1/')
  tp=$((tp + ${p:-0})); tf=$((tf + ${f:-0}))
done
printf '\n\n######## composition suite ########\n'
printf 'total: %d passed, %d failed\n' "$tp" "$tf"
printf '\nA FAIL here is a finding, not a broken drill: every leg is\n'
printf 'phrased so that PASS means the composition rule held.\n'
[ "${DOWN:-0}" = "1" ] && docker rm -f flint-composition-minio >/dev/null 2>&1 && echo "MinIO stopped"
exit 0
