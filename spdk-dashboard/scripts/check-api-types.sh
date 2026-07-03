#!/usr/bin/env bash
# Verifies the committed OpenAPI spec and generated TS types are fresh:
#   api/openapi.json      == what the backend code emits (dashboard-openapi bin)
#   src/api/schema.d.ts   == what openapi-typescript generates from that spec
# Run from spdk-dashboard/. Intended for CI (plan Phase 3) and pre-commit use.
set -euo pipefail

cd "$(dirname "$0")/.."
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

echo "==> regenerating spec from backend (cargo run --bin dashboard-openapi)"
(cd ../spdk-csi-driver && cargo run -q --bin dashboard-openapi) > "$tmpdir/openapi.json"
if ! diff -u api/openapi.json "$tmpdir/openapi.json" > "$tmpdir/spec.diff"; then
  echo "STALE: api/openapi.json does not match the backend." >&2
  echo "Regenerate: (cd ../spdk-csi-driver && cargo run -q --bin dashboard-openapi) > api/openapi.json" >&2
  head -40 "$tmpdir/spec.diff" >&2
  exit 1
fi

echo "==> regenerating TS types (openapi-typescript)"
npx --no-install openapi-typescript api/openapi.json -o "$tmpdir/schema.d.ts" >/dev/null
if ! diff -u src/api/schema.d.ts "$tmpdir/schema.d.ts" > "$tmpdir/types.diff"; then
  echo "STALE: src/api/schema.d.ts does not match api/openapi.json." >&2
  echo "Regenerate: npm run gen:api" >&2
  head -40 "$tmpdir/types.diff" >&2
  exit 1
fi

echo "OK: api/openapi.json and src/api/schema.d.ts are fresh."
