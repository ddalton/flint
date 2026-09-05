#!/usr/bin/env python3
"""Measure the generated deck for silent overflow. Run after gen_radar.py.

Every .page is a fixed 11x8.5in block with overflow hidden and no warning
when content does not fit; a .card's overflow is painted over by the next
card's opaque background. This deck has been caught clipping three times
that way -- 11 rationale cells (2026-08-27), then all four chart notes on
page 1 (2026-09-04; they had been cut mid-sentence in the committed PDF
since the notes grew), and the lakeFS page's table header once it carried
seven columns. pdftotext cannot see any of it. The DOM can: this injects a
probe into a copy of the HTML, renders it in headless Chrome, and prints,
per page, what overflows and what is left. Exit 1 on any overflow, and on
any rationale column whose last cell ends below the column (free < 0): the
page may still fit, but that cell is standing on the foot.
"""
import os, re, subprocess, sys, tempfile, html

CHROME = os.environ.get("CHROME", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
PROBE = """<script>
window.addEventListener('load',()=>{
  const out=[];
  document.querySelectorAll('.page').forEach((p,i)=>{
    out.push(`page ${i+1}: over=${p.scrollHeight-p.clientHeight}`);
    p.querySelectorAll('.ratcol').forEach((c,j)=>{const cr=c.getBoundingClientRect();const last=c.lastElementChild.getBoundingClientRect();out.push(`  ratcol ${j}: free=${Math.round(cr.bottom-last.bottom)}`)});
    p.querySelectorAll('.card').forEach((c,j)=>{const tb=c.querySelector('table');const side=c.querySelector('.side');out.push(`  card ${j}: over=${c.scrollHeight-c.clientHeight} table-over=${tb&&side?Math.max(0,tb.scrollWidth-side.clientWidth):0}`)});
  });
  const pre=document.createElement('pre');pre.id='probe';pre.textContent=out.join('\\n');document.body.appendChild(pre);
});
</script>"""

def main():
    here = os.path.dirname(os.path.abspath(__file__))
    src = os.path.join(here, "..", "flint-approach-radar.html")
    page = open(src).read().replace("</body>", PROBE + "</body>", 1)
    with tempfile.TemporaryDirectory() as td:
        probe = os.path.join(td, "probe.html")
        open(probe, "w").write(page)
        dom = subprocess.run([CHROME, "--headless", "--disable-gpu", "--window-size=1056,816",
                              "--dump-dom", "file://" + probe],
                             capture_output=True, text=True).stdout
    m = re.search(r'<pre id="probe">(.*?)</pre>', dom, re.S)
    if not m:
        sys.exit("no probe output — is Chrome at CHROME=?")
    report = html.unescape(m.group(1))
    print(report)
    bad = [l for l in report.splitlines() if re.search(r"over=[1-9]|free=-", l)]
    if bad:
        sys.exit(f"OVERFLOW on {len(bad)} element(s) — the PDF will silently clip them:\n" + "\n".join(bad))
    print("no overflow")

if __name__ == "__main__":
    main()
