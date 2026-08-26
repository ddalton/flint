#!/usr/bin/env bash
# HTML -> PDF for the radar deck.
#
# RECONSTRUCTED, not recovered: the original PDF step was run inline and
# left nothing on disk, so this is a rebuild of it rather than the
# original command. The page CSS sets `@page { size: 11in 8.5in }` and
# every `.page` is a fixed 11x8.5in block with `page-break-after`, so a
# print-to-PDF of the HTML is the whole job — no pagination logic here.
set -eu
cd "$(dirname "$0")"
HTML="$PWD/../flint-approach-radar.html"
OUT="$PWD/../flint-approach-radar.pdf"
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
[ -x "$CHROME" ] || { echo "no Chrome at $CHROME (set CHROME=)" >&2; exit 1; }
"$CHROME" --headless --disable-gpu --no-pdf-header-footer \
  --print-to-pdf="$OUT" "file://$HTML" 2>/dev/null
echo "wrote $OUT"
