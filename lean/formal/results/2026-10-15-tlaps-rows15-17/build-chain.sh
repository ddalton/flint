#!/usr/bin/env bash
# build-chain.sh OUTDIR -- every proof module of the shipped shape's model, from
# its sources, into OUTDIR: M0-M5B (gen-m5.py, split by split-proof.py), M5C, M5D,
# M6 (the maintained texts in m5c/, m5d/, m6/), L1 (chain/), M7-M14 (their
# generators), over LeanP1Anc.tla (this directory's copy, make-anc.py's output).
# Byte-for-byte the texts chain/MD5.txt and final/md5.txt pin, on the 10-16 copy.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$(mkdir -p "$1" && cd "$1" && pwd)"
cd "$HERE/../.."   # lean/formal
T=$(mktemp -d)
python3 "$HERE/gen-m5.py" "$T/LeanP1Proof.tla" > /dev/null
python3 results/2026-10-07-tlaps-m5/split-proof.py "$T/LeanP1Proof.tla" "$OUT" > /dev/null
cp "$HERE/m5c/LeanP1ProofM5C.tla" "$HERE/m5d/LeanP1ProofM5D.tla" "$HERE/m6/LeanP1ProofM6.tla" \
   "$HERE/chain/LeanP1ProofL1.tla" "$OUT/"
cd "$HERE"
for g in gen-m7.py gen-m8m9.py gen-m10.py gen-m11.py gen-m12.py gen-m13.py gen-m14.py; do
  python3 "$g" "$OUT" > /dev/null
done
cp "$HERE/LeanP1Anc.tla" "$OUT/"
rm -rf "$T"
(cd "$OUT" && md5sum *.tla 2>/dev/null || md5 -r *.tla)
