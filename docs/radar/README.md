# The approach-radar generator

`../flint-approach-radar.html` and `../flint-approach-radar.pdf` are
**generated**. Edit the data here and regenerate — do not hand-edit the
HTML, or the next regeneration silently reverts you.

```sh
python3 gen_radar.py     # radar_data.json + lakefs_data.json -> ../flint-approach-radar.html
python3 measure.py       # headless Chrome: does every page, column and card FIT? (exit 1 if not)
./build-pdf.sh           # that HTML -> ../flint-approach-radar.pdf  (needs Chrome)
```

Run `measure.py` every time. A `.page` is a fixed block with no overflow
warning, and a card's overflow is painted over by the next card, so text
that does not fit simply vanishes from the PDF — it has happened three
times (rationale cells, then every chart note on page 1, then a table
header). `pdftotext` cannot see it; the DOM can. Chart notes therefore
live as the lede of each chart's "Why these numbers" page, not on page 1.

## Why this directory exists

This generator spent its first five weeks in an ephemeral session
scratchpad. The committed HTML was the only durable artifact, so the
project record said, correctly, "the committed HTML is the source of
truth" — which meant every correction was a hand-edit to generated
output, and a regeneration would have thrown them all away. Recovering
it was luck: the scratchpad had not been reaped yet. Hence this
directory.

`gen_radar.py` reproduces the pre-recovery committed HTML **byte for
byte**, which is how we know it is the real generator and not a
lookalike.

## The chain

| file | role |
|---|---|
| `wf_result.json`, `lakefs_wf_result.json` | raw multi-agent scoring runs — the provenance behind every number |
| `flint_rubric.json` | the scoring rubric those runs applied |
| `build_radar.py` | one-shot: `wf_result.json` -> `radar_data.json`. Rerunning it **discards** later hand-corrections to the data; it is kept for provenance, not as a routine step |
| `radar_data.json` | **the source of truth for scores and prose** — edit this |
| `lakefs_data.json` | page 5 (lakeFS, scored from source) |
| `gen_radar.py` | the renderer: charts, layout, and all page templates |
| `build-pdf.sh` | HTML -> PDF. **Reconstructed, not recovered** — the original step ran inline and left nothing behind |

## House rules for the numbers

- Scores are **as designed**, not as shipped. A defect that was found and
  fixed does not move a score; only a claim that is now factually wrong
  moves. Maturity and evidence live in prose, not in the score.
- Every claim should name what was drilled *and* what is still open in
  the same breath. The deck is evidence-first; it is not a brochure.
