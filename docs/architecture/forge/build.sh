#!/bin/bash
# ---------------------------------------------------------------------------
# Regenerate docs/architecture/forge/ — the flint-forge architecture document:
# seven A3 plates, the canonical PDF, and a Markdown rendition, all from ONE
# source of prose (the HTML) and ONE source of diagrams (forge-diagrams.py).
#
#   docs/architecture/forge/build.sh            # generate, validate, render, emit the Markdown
#   docs/architecture/forge/build.sh --check    # generate and validate only; no Chrome needed
#   docs/architecture/forge/build.sh --geometry # MEASURE every label against its box in Chrome
#
# Source, edited by hand:
#   flint-forge-architecture.html   layout and ALL prose
#   forge-diagrams.py               every plate, drawn with the deck's kit (../diagrams.py)
# Built, never edited by hand (the SVGs are committed so the HTML can reference them):
#   diagrams/*.svg                  the plates
#   flint-forge-architecture.pdf    the canonical document, one page per section
#   flint-forge-architecture.md     the Markdown rendition, referencing the SVGs
#
# The same rules as the family deck's build: nothing here pipes a check into
# anything (a pipeline's exit status is the LAST command's); every step's
# status is tested on its own; a page that overflows or a diagram that fails
# to load is counted, not assumed.
# ---------------------------------------------------------------------------
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
html="$here/flint-forge-architecture.html"
pdf="$here/flint-forge-architecture.pdf"
md="$here/flint-forge-architecture.md"

mode=${1:---build}
case "$mode" in --build|--check|--geometry) ;; *)
    echo "usage: $0 [--build|--check|--geometry]" >&2; exit 2 ;;
esac
command -v python3 >/dev/null || { echo "python3 not found" >&2; exit 2; }

# ── 0. the plates, with the generator's own fit check ──────────────────────
echo "==> generating diagrams"
python3 "$here/forge-diagrams.py" >/dev/null

# ── 1. every diagram is well-formed, every reference resolves ───────────────
echo "==> validating diagrams and references"
python3 - "$here" "$html" <<'PYEOF'
import glob, os, re, sys, xml.dom.minidom
here, html_path = sys.argv[1], sys.argv[2]
bad = []
svgs = sorted(glob.glob(os.path.join(here, "diagrams", "*.svg")))
if not svgs:
    bad.append("no diagrams found under diagrams/")
for p in svgs:
    rel = os.path.relpath(p, here)
    try:
        doc = xml.dom.minidom.parse(p)
    except Exception as e:
        bad.append(f"{rel}: not well-formed XML — {e}"); continue
    root = doc.documentElement
    if root.tagName != "svg" or not root.getAttribute("xmlns"):
        bad.append(f"{rel}: root must be <svg> with an xmlns")
    if not root.getAttribute("viewBox"):
        bad.append(f"{rel}: no viewBox")
    if not doc.getElementsByTagName("style"):
        bad.append(f"{rel}: no <style>")
    if not root.getAttribute("aria-label"):
        bad.append(f"{rel}: no aria-label")
    for t in doc.getElementsByTagName("text"):
        if t.getAttribute("fill") and t.getAttribute("class"):
            txt = "".join(n.data for n in t.childNodes if n.nodeType == n.TEXT_NODE)[:40]
            bad.append(f"{rel}: <text class=.. fill=..> loses to the class — {txt!r}")
    # a baseline on or past its box's bottom edge clips the descenders
    boxes = []
    for r in doc.getElementsByTagName("rect"):
        if r.getAttribute("class") == "halo":
            continue
        try:
            boxes.append((float(r.getAttribute("x")), float(r.getAttribute("y")),
                          float(r.getAttribute("width")), float(r.getAttribute("height")),
                          r.getAttribute("class")))
        except ValueError:
            pass
    for t in doc.getElementsByTagName("text"):
        try:
            tx, ty = float(t.getAttribute("x")), float(t.getAttribute("y"))
        except ValueError:
            continue
        txt = "".join(n.data for n in t.childNodes if n.nodeType == n.TEXT_NODE)[:40]
        for (x, y, w, h, cls) in boxes:
            if x < tx < x + w and y < ty <= y + h + 10 and ty > y + h - 4:
                bad.append(f"{rel}: text clips box[{cls}] bottom={y+h:.0f} at y={ty:.0f} — {txt!r}")
src = open(html_path, encoding="utf-8").read()
refs = re.findall(r'<img src="([^"]+)"', src)
for r in refs:
    if not os.path.exists(os.path.join(here, r)):
        bad.append(f"the HTML references a missing file: {r}")
for p in svgs:
    rel = os.path.relpath(p, here)
    if rel not in refs:
        bad.append(f"{rel}: on disk but referenced by no page")
if len(refs) != len(set(refs)):
    bad.append("the same diagram is referenced by more than one page")
sections = re.split(r'<section class="page', src)[1:]
if bad:
    print("\n".join("  FAIL " + b for b in bad), file=sys.stderr); sys.exit(1)
print(f"  {len(svgs)} diagrams, {len(refs)} references, {len(sections)-1} pages, all resolved")
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

# ── 1b. MEASURED horizontal overflow (--geometry) ──────────────────────────
if [ "$mode" = "--geometry" ]; then
    echo "==> measuring text geometry in Chrome"
    python3 - "$here" "$chrome" <<'PYEOF'
import glob, json, os, re, subprocess, sys, tempfile
here, chrome = sys.argv[1], sys.argv[2]
PROBE = """
<script>
window.addEventListener('load', function () {
  var out = [], svg = document.querySelector('svg');
  var rects = Array.from(svg.querySelectorAll('rect')).map(function (r) {
    return { x:+r.getAttribute('x'), y:+r.getAttribute('y'),
             w:+r.getAttribute('width'), h:+r.getAttribute('height'),
             cls:r.getAttribute('class') || '' };
  }).filter(function (b) { return !isNaN(b.x) && !isNaN(b.w) && b.cls !== 'halo'; });
  Array.from(svg.querySelectorAll('text')).forEach(function (t) {
    var x = +t.getAttribute('x'), y = +t.getAttribute('y');
    if (isNaN(x) || isNaN(y)) return;
    var txt = t.textContent || ''; if (!txt.trim()) return;
    var w = t.getComputedTextLength(), a = getComputedStyle(t).textAnchor;
    var left = a === 'middle' ? x - w/2 : (a === 'end' ? x - w : x);
    var cand = rects.filter(function (b) {
      return b.x < x && x < b.x + b.w && b.y < y && y <= b.y + b.h + 10; });
    if (!cand.length) return;
    var b = cand.reduce(function (m, c) { return c.w*c.h < m.w*m.h ? c : m; });
    var over = (left + w) - (b.x + b.w);
    if (over > -3) out.push({ over:Math.round(over), box:b.cls,
                              boxRight:Math.round(b.x+b.w), text:txt.slice(0,58) });
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
        print(f"  FAIL {rel}: +{h['over']}px past box[{h['box']}] right={h['boxRight']} — {h['text']!r}")
        bad += 1
if bad:
    sys.exit(1)
print("  no text overflows its box")
PYEOF
    echo "==> geometry only; nothing rendered"
    exit 0
fi

# ── 2. the Markdown rendition, extracted from the one HTML ─────────────────
echo "==> generating Markdown"
python3 - "$html" "$md" "$here" <<'PYEOF'
import html as H, os, re, sys
src = open(sys.argv[1], encoding="utf-8").read()
here = sys.argv[3]

def inline(s):
    s = re.sub(r"<br\s*/?>", " ", s)
    s = re.sub(r"<code>(.*?)</code>", r"`\1`", s, flags=re.S)
    s = re.sub(r"<b>(.*?)</b>", r"**\1**", s, flags=re.S)
    s = re.sub(r"<i>(.*?)</i>", r"*\1*", s, flags=re.S)
    s = re.sub(r"<[^>]+>", "", s)
    s = H.unescape(s)
    return re.sub(r"\s+", " ", s).strip()

def grab(pat, text, flags=re.S):
    m = re.search(pat, text, flags)
    return m.group(1) if m else None

out, sections = [], re.split(r'<section class="page', src)[1:]
cover = sections[0]
out += ["# " + inline(grab(r"<h1>(.*?)</h1>", cover)), ""]
out += ["> " + inline(grab(r'<p class="sub">(.*?)</p>', cover)), ""]
for p in re.findall(r"<p[^>]*>(.*?)</p>", grab(r'<div class="meta">(.*?)</div>', cover)):
    out += [inline(p), ""]
out += ["## Contents", ""]
entries = re.findall(r"<div>(.*?)</div>", cover.split('<div class="toc">', 1)[1], re.S)
out += ["- " + inline(d) for d in entries] + [""]
for sec in sections[1:]:
    out += [f"## {inline(grab(r'<h1>(.*?)</h1>', sec))}", ""]
    out += [f"*{inline(grab(r'<span class=.kicker.>(.*?)</span>', sec))}*", ""]
    out += [inline(grab(r'<p class="dek">(.*?)</p>', sec)), ""]
    img, alt = grab(r'<figure>.*?<img src="([^"]+)"', sec), grab(r'<figure>.*?alt="([^"]*)"', sec)
    out += [f"![{H.unescape(alt or '')}]({img})", ""]
    for p in re.findall(r"<p>(.*?)</p>", grab(r"<figcaption>(.*?)</figcaption>", sec)):
        out += [inline(p), ""]
if len(entries) != len(sections) - 1:
    sys.exit(f"  FAIL: {len(entries)} contents entries for {len(sections)-1} pages")
text = "\n".join(out).rstrip() + "\n"
for ref in re.findall(r"!\[[^\]]*\]\(([^)]+)\)", text):
    if not os.path.exists(os.path.join(here, ref)):
        sys.exit(f"  FAIL: the Markdown references a missing {ref}")
open(sys.argv[2], "w", encoding="utf-8").write(text)
n_figs = len(re.findall(r"!\[", text))
print(f"  -> {os.path.basename(sys.argv[2])}  ({n_figs} figures)")
PYEOF

# ── 3. the canonical PDF ───────────────────────────────────────────────────
echo "==> rendering PDF"
"$chrome" --headless --disable-gpu --no-pdf-header-footer \
    --print-to-pdf="$pdf" "file://$html" 2>/dev/null
[ -s "$pdf" ] || { echo "chrome wrote no PDF" >&2; exit 1; }

# ── 4. the PDF says what the HTML said ─────────────────────────────────────
expected=$(grep -c '<section class="page' "$html")
if command -v pdfinfo >/dev/null; then
    got=$(pdfinfo "$pdf" | awk '/^Pages:/{print $2}')
    if [ "$got" != "$expected" ]; then
        echo "  FAIL: $got PDF pages for $expected sections — a page is overflowing" >&2; exit 1
    fi
    echo "  $got pages, one per section"
fi
if command -v pdftotext >/dev/null; then
    txt=$(pdftotext "$pdf" - 2>/dev/null || true)
    for probe in "flint-forge-gitcgi" "proc-receive" "TokenReview" "X-Remote-User" "Continuity" "2,776,804"; do
        case "$txt" in
            *"$probe"*) ;;
            *) echo "  WARN: '$probe' is in the source but not in the rendered text" >&2 ;;
        esac
    done
fi
echo "==> done"
ls -la "$pdf" "$md"
