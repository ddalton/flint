#!/bin/bash
# A profile-guided build of a generated checker, for long runs (a world of
# hours). Generates the checker, builds it instrumented, runs the same world
# for a sample period, merges the profile and rebuilds with it. Not worth it
# for small worlds: the extra build and the sample cost minutes.
#
#   pgo-build.sh TLC_RS SPEC.tla CFG GEN_DIR TARGET_DIR [SAMPLE_SECS] [WORKERS]
#
# TLC_RS      the tlc-rs binary to generate with
# SPEC.tla    the spec; the sample run happens in its directory
# CFG         its config (the world to be run)
# GEN_DIR     where the checker's crate is generated
# TARGET_DIR  cargo's target dir for the final build; the binary is printed
# SAMPLE_SECS seconds of sampling, ended at the next level boundary
#             (TLCRS_STOP_AFTER_SECS; default 120)
# WORKERS     for the sample run (default: all cores)
#
# Needs llvm-profdata from the toolchain (`rustup component add llvm-tools`).
set -euo pipefail
[ $# -ge 5 ] || { sed -n '2,19p' "$0"; exit 2; }
TLC_RS=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
SPEC=$2; CFG=$3; GEN=$4; TGT=$5; SECS=${6:-120}
WORKERS=${7:-$(getconf _NPROCESSORS_ONLN)}
SPEC_DIR=$(cd "$(dirname "$SPEC")" && pwd)
PROFDATA=$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-profdata 2>/dev/null | head -1)
[ -x "$PROFDATA" ] || { echo "pgo-build: no llvm-profdata; run: rustup component add llvm-tools" >&2; exit 1; }
mkdir -p "$TGT"; TGT=$(cd "$TGT" && pwd)
PD=$TGT/pgo-data; rm -rf "$PD"; mkdir -p "$PD"

# The generated crate sets target-cpu=native in .cargo/config.toml; RUSTFLAGS
# replaces those flags, so it is repeated here.
BASE="-C target-cpu=native"

( cd "$SPEC_DIR" && "$TLC_RS" -codegen "$GEN" -config "$CFG" "$(basename "$SPEC")" )
echo "pgo-build: instrumented build" >&2
( cd "$GEN" && RUSTFLAGS="$BASE -C profile-generate=$PD" CARGO_TARGET_DIR="$TGT/instrumented" cargo build --release -q )
BIN=$(ls "$TGT"/instrumented/release/tlcgen-* | grep -v '\.d$' | head -1)

echo "pgo-build: sampling ${SECS}s at $WORKERS workers" >&2
META=$(mktemp -d)
set +e
( cd "$SPEC_DIR" && TLCRS_STOP_AFTER_SECS=$SECS "$BIN" -workers "$WORKERS" -checkpoint 0 -metadir "$META/st" -config "$CFG" "$(basename "$SPEC")" > "$TGT/pgo-sample.out" 2>&1 )
rc=$?
set -e
rm -rf "$META"
# 3: stopped by TLCRS_STOP_AFTER_SECS; 0: the world finished inside the sample
[ $rc -eq 3 ] || [ $rc -eq 0 ] || { echo "pgo-build: sample run failed (rc=$rc), see $TGT/pgo-sample.out" >&2; exit 1; }
ls "$PD"/*.profraw >/dev/null 2>&1 || { echo "pgo-build: the sample run wrote no profile" >&2; exit 1; }
"$PROFDATA" merge -o "$TGT/merged.profdata" "$PD"

echo "pgo-build: optimized build" >&2
( cd "$GEN" && RUSTFLAGS="$BASE -C profile-use=$TGT/merged.profdata" CARGO_TARGET_DIR="$TGT" cargo build --release -q )
ls "$TGT"/release/tlcgen-* | grep -v '\.d$' | head -1
