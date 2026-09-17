#!/bin/bash
# ---------------------------------------------------------------------------
# Regenerate docs/architecture/databricks/ — the Databricks comparison:
# seven Visio figures, the SVGs the document references, an EMF per figure,
# and the canonical PDF, from ONE source of prose (the HTML) and ONE source
# of figures (databricks-visio.py).
#
#   docs/architecture/databricks/build.sh           # gate, render, check
#   docs/architecture/databricks/build.sh --check   # gate and validate only; no Chrome
#
# Source, edited by hand:
#   flint-vs-databricks-volumes.html   layout and ALL prose
#   databricks-visio.py                every figure, drawn with vsdxkit/dataflowkit
# Built, never edited by hand:
#   flint-vs-databricks.vsdx           the figures, editable in Visio
#   flint-vs-databricks-N.emf          one metafile per figure, for Office
#   diagrams/*.svg                     the figures as the HTML references them
#   flint-vs-databricks-volumes.pdf    the document
#
# Nothing here pipes a check into anything (a pipeline's exit status is the
# LAST command's); every step's status is tested on its own.
# ---------------------------------------------------------------------------
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
html="$here/flint-vs-databricks-volumes.html"
pdf="$here/flint-vs-databricks-volumes.pdf"

mode=${1:---build}
case "$mode" in --build|--check|--geometry) ;; *)
    echo "usage: $0 [--build|--check|--geometry]" >&2; exit 2 ;;
esac
command -v python3 >/dev/null || { echo "python3 not found" >&2; exit 2; }

# ── 0. the figures, with the kit's gates ───────────────────────────────────
echo "==> generating figures (gates: overflow, overlap, labels, ink, edges)"
gate_log=$(mktemp)
if ! python3 "$here/databricks-visio.py" --svg --emf >"$gate_log" 2>&1; then
    cat "$gate_log" >&2; rm -f "$gate_log"
    echo "  FAIL: a figure gate found a problem" >&2; exit 1
fi
grep -E "shapes, .* problems" "$gate_log" || true
rm -f "$gate_log"

# ── 1. every reference resolves, every figure is used once ────────────────
echo "==> validating references"
python3 - "$here" "$html" <<'PYEOF'
import glob, os, re, sys, xml.dom.minidom
here, html_path = sys.argv[1], sys.argv[2]
bad = []
svgs = sorted(glob.glob(os.path.join(here, "diagrams", "*.svg")))
for p in svgs:
    try:
        xml.dom.minidom.parse(p)
    except Exception as e:
        bad.append(f"{os.path.relpath(p, here)}: not well-formed XML — {e}")
src = open(html_path, encoding="utf-8").read()
refs = re.findall(r'<img src="([^"]+)"', src)
for r in refs:
    if not os.path.exists(os.path.join(here, r)):
        bad.append(f"the HTML references a missing file: {r}")
for p in svgs:
    if os.path.relpath(p, here) not in refs:
        bad.append(f"{os.path.relpath(p, here)}: on disk but referenced by no page")
if len(refs) != len(set(refs)):
    bad.append("the same figure is referenced by more than one page")
for m in re.finditer(r'<img src="[^"]+"(?![^>]*alt=")', src):
    bad.append("an <img> without alt text")
sections = len(re.findall(r'<section class="page', src))
toc = len(re.findall(r"<div><b>\d+ · ", src))
if toc != sections - 1:
    bad.append(f"{toc} contents entries for {sections - 1} pages")
if bad:
    print("\n".join("  FAIL " + b for b in bad), file=sys.stderr); sys.exit(1)
print(f"  {len(svgs)} figures, {len(refs)} references, {sections} sections, all resolved")
PYEOF

if [ "$mode" = "--check" ]; then
    echo "==> check only; nothing rendered"
    exit 0
fi

chrome=""
for c in "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
         "/Applications/Chromium.app/Contents/MacOS/Chromium" \
         "$(command -v google-chrome || true)" \
         "$(command -v chromium || true)"; do
    if [ -n "$c" ] && [ -x "$c" ]; then chrome="$c"; break; fi
done
[ -n "$chrome" ] || { echo "no Chrome/Chromium found — cannot render" >&2; exit 2; }

# ── 1b. MEASURED text width (--geometry) ────────────────────────────────────
# vsdxkit wraps text by an ESTIMATED advance width; a line of capitals is
# wider than the estimate and runs past its box with no gate noticing.
# This measures every line in Chrome (getComputedTextLength) against the
# smallest rectangle holding its anchor, on BOTH sides, one SVG per run.
if [ "$mode" = "--geometry" ]; then
    echo "==> measuring text geometry in Chrome"
    python3 - "$here" "$chrome" <<'PYEOF'
import glob, json, os, re, subprocess, sys, tempfile
here, chrome = sys.argv[1], sys.argv[2]
PROBE = """
<script>
window.addEventListener('load', function () {
  var out = [], svg = document.querySelector('svg');
  // <path> as well as <rect>: every component the kit draws as a symbol
  // (rect, cylinder, block arrow) is a PATH, and a probe that read only
  // <rect> passed a line of capitals touching its box — it never saw the box.
  var rects = Array.from(svg.querySelectorAll('rect, path')).map(function (r) {
    var bb = r.getBBox();
    return { x:bb.x, y:bb.y, w:bb.width, h:bb.height };
  }).filter(function (b) { return b.w > 0 && b.h > 0 && !(b.x === 0 && b.y === 0); });
  Array.from(svg.querySelectorAll('text')).forEach(function (t) {
    var x = +t.getAttribute('x'), y = +t.getAttribute('y');
    var txt = t.textContent || ''; if (!txt.trim()) return;
    var w = t.getComputedTextLength(), a = t.getAttribute('text-anchor');
    var left = a === 'middle' ? x - w/2 : (a === 'end' ? x - w : x);
    var cand = rects.filter(function (b) {
      return b.x < x && x < b.x + b.w && b.y < y && y <= b.y + b.h; });
    if (!cand.length) return;
    var b = cand.reduce(function (m, c) { return c.w*c.h < m.w*m.h ? c : m; });
    var over = Math.max((left + w) - (b.x + b.w), b.x - left);
    if (over > -2) out.push({ over:Math.round(over*10)/10, text:txt.slice(0,60) });
  });
  document.title = 'RESULT' + JSON.stringify(out);
});
</script>
"""
bad = 0
for p in sorted(glob.glob(os.path.join(here, "diagrams", "*.svg"))):
    html = "<!DOCTYPE html><meta charset=utf-8><body style='margin:0'>" + open(p).read() + PROBE
    with tempfile.NamedTemporaryFile("w", suffix=".html", delete=False) as f:
        f.write(html); tmp = f.name
    try:
        dom = subprocess.run([chrome, "--headless", "--disable-gpu",
                              "--virtual-time-budget=1500", "--dump-dom", "file://" + tmp],
                             capture_output=True, text=True, timeout=90).stdout
    finally:
        os.unlink(tmp)
    m = re.search(r"<title>RESULT(\[.*?\])</title>", dom, re.S)
    rel = os.path.relpath(p, here)
    if not m:
        print(f"  FAIL {rel}: the probe did not run — cannot conclude"); bad += 1; continue
    for h in sorted(json.loads(m.group(1)), key=lambda h: -h["over"]):
        print(f"  FAIL {rel}: text within {h['over']}px of its box edge or past it — {h['text']!r}")
        bad += 1
if bad:
    sys.exit(1)
print("  every line of text clears its box by at least 2px")
PYEOF
    echo "==> geometry only; nothing rendered"
    exit 0
fi

# ── 2. the PDF ──────────────────────────────────────────────────────────────
echo "==> rendering PDF"
rm -f "$pdf"
"$chrome" --headless --disable-gpu --no-pdf-header-footer \
    --print-to-pdf="$pdf" "file://$html" 2>/dev/null
[ -s "$pdf" ] || { echo "chrome wrote no PDF" >&2; exit 1; }

# ── 3. the PDF says what the HTML said ──────────────────────────────────────
expected=$(grep -c '<section class="page' "$html")
if command -v pdfinfo >/dev/null; then
    got=$(pdfinfo "$pdf" | awk '/^Pages:/{print $2}')
    if [ "$got" != "$expected" ]; then
        echo "  FAIL: $got PDF pages for $expected sections — a page is overflowing" >&2
        exit 1
    fi
    echo "  $got pages, one per section"
else
    echo "  WARN: pdfinfo absent — page count UNVERIFIED" >&2
fi
if command -v pdftotext >/dev/null; then
    txt=$(pdftotext "$pdf" - 2>/dev/null || true)
    for probe in "Based on public sources only" "goofys-dbr" "Kata" "OpenSharing" \
                 "s3.csi.chert.us" "fuse4dbricks" "FuseMounter"; do
        case "$txt" in
            *"$probe"*) ;;
            *) echo "  FAIL: '$probe' is in the source but not in the rendered text" >&2; exit 1 ;;
        esac
    done
    echo "  probe strings present in the rendered text"
fi
echo "==> done"
ls -la "$pdf"
