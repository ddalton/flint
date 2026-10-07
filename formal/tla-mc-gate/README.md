# The tla-mc gate

flint's TLA+ gate (`scripts/check-tla.sh`, `lean/formal/check.sh`) runs TLC. These
scripts run the same entries through [tla-mc](https://github.com/ddalton/tla-mc)
— the Rust checker that was `formal/tlc-rs` here until 2026-10-07, now its own
repository and crate — and compare it with the gate's expectations and with TLC.
Run it after a tla-mc upgrade, before trusting the new version's verdicts on
flint's models.

```sh
cargo install tla-mc --version 0.1.1 --locked     # the version flint was last checked with (2026-10-07)
export FLINT_ROOT=$PWD                             # the flint checkout

# 1. every gate entry through tla-mc's interpreter (15 s each; ONLY=file limits it to listed cfgs)
python3 formal/tla-mc-gate/sweep.py "$(which tla-mc)" 15 sweep.jsonl
# 2. TLC on the entries tla-mc decided, for the distinct counts
python3 formal/tla-mc-gate/tlc_side.py sweep.jsonl tlc.jsonl
# 3. interpreter AND generated checker on every 'agree' entry, N at a time
python3 formal/tla-mc-gate/gate_par.py "$FLINT_ROOT" sweep.jsonl tlc.jsonl,formal/tla-mc-gate/liveness-vs-tlc-54-2026-09-26.jsonl,formal/tla-mc-gate/liveness-vs-tlc-18-2026-09-26.jsonl gate.jsonl 8
python3 formal/tla-mc-gate/gate_eval.py gate.jsonl
```

`gate_par.py` finds tla-mc as `$TLAMC`, else on `PATH`, works in `$GATE_WORK`
(default `/data/gate-work`) with `$GATE_WORKERS` per run (default 4); for TLC counts,
the first file listed that has an entry wins. On a big machine, shard the sweep:
`SHARD=k/n TMPDIR=<own dir per shard> sweep.py ...` (each shard needs its own TMPDIR),
`tlc_side.py` takes `$FLINT_ROOT`, `$TLC_JAR` and `$TLC_WORKERS`, and give the few
long entries all the cores afterwards rather than leaving them on 8 or 4 workers.
The last full run (tla-mc 0.1.1, 2026-10-07: 443 of 446 decided, all agreeing) is in
tla-mc's `results/validation-2026-10-07/`. The recorded inputs: `gate-sweep-2026-09-29.jsonl`
(the entries and tla-mc's verdicts then), `tlc-vs-tlcrs-2026-09-26.jsonl` and the
two `liveness-vs-tlc-*` files (TLC's distinct counts). The runs before the move —
gate sweeps, codegen gates, the Examples sweep — are in tla-mc's `results/`.
