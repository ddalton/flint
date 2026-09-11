# The scoped-read probes, RUN

The audit of 2026-09-11 shipped four probes and ran none of them
(`scoped-audit-probes.patch`, marked UNRUN). This is that run:
`cargo test probe_ -- --nocapture --test-threads=1` in `lean/sidecar`,
against `14b3637c`. **6 passed, 1 failed** — and the failure is the good
news.

## P1 — `probe_scoped_baseline_vs_whole_tree_sync`: PREDICTION CONFIRMED

Setup: a baseline hand-narrowed to `inputs/` (entries + `prev_scan`
scoped, `inst_base` left FULL), the out-of-scope files removed from the
tree. A sibling then changes `outputs/b.txt` and deletes
`outputs/d.txt`. Then the ordinary whole-tree `sync`.

```
scoped baseline: entries=["inputs/a.txt"]
scoped baseline: inst_base=["inputs/a.txt","outputs/b.txt","outputs/c.txt","outputs/d.txt"]
report: {"applied":["outputs/b.txt"],"deleted":[],"conflicts":[],"seq":2,"out_of_scope_foreign":0}
on-disk outputs/b.txt: Some("foreign b v2")     <- re-materialised
on-disk outputs/c.txt: None                      <- untouched
after entries=["inputs/a.txt","outputs/b.txt"]   <- THE SCOPE GREW
```

**A scoped workspace's held set drifts by exactly what the remote
happens to touch.** Not by what the workspace asked for, and not under
its control. The changed out-of-scope path came back; the unchanged one
did not. This is measured now, not argued.

## P3 — `probe_inbox_widens_scope_then_deletes_the_object`: CONFIRMED

```
first_absence=["other/big.bin"] then deleted=["other/big.bin"]
```

And this is the part that makes P1 more than a curiosity. Once a path
has drifted into the baseline, it is fully owned: removing it locally
publishes the object DELETE on the second scan, exactly as for a path
the workspace asked for. **Read-drift converts into write-authority.** A
workspace scoped to three files can end up deleting a fourth it never
requested, if the remote touched it and the agent then tidied up.

## P2 — `probe_scoped_baseline_out_of_scope_local_files`: CONFIRMED SAFE

```
conflicts: [("outputs/clobber.txt","sync-dirty"),
            ("outputs/killme.txt","sync-remote-delete-vs-dirty")]
after entries=["inputs/a.txt"]
```

Out-of-scope files that exist locally but are not cited become
conflicts, never deletes, and never enter the baseline. The uncited
state is safe on its own.

## P7 — `probe_a_transient_write_error_permanently_drops_an_inbox_entry`: FALSIFIED BY THE FIX

This one FAILED, and that is the result. The probe asserts the
pre-`28be02b7` behaviour verbatim in its own doc comment — *"any write
error inside `write_file_atomic_in` — ENOSPC, EACCES, EIO — is recorded
as `consume-refused-containment` and the entry is CONSUMED... Nothing
ever re-offers it"* — and asserts
`kind.starts_with("consume-refused-containment")`.

At HEAD, the same EACCES produces:

```
kind = "consume-write-failed (will retry): ... Permission denied (os error 13)"
consumed = 0
```

An oracle written independently, before the fix, describing the bug in
its own words, no longer reproduces it. That is a stronger verification
of `28be02b7` than the test that shipped with it.

## What is still not run

The C2 control — that narrowing `inst_base` alongside the baseline makes
every unadmitted entry read as foreign. `manifest.rs:998` is
`base.get(p).map(|b| b != &e.etag).unwrap_or(true)`, so absent-from-base
means changed by construction; P1 deliberately left `inst_base` FULL and
therefore says nothing about the narrowed case. That control becomes a
permanent test in phase 2 of
`docs/plans/flint-lean-scoped-read-design.md`, because C2 is the single
constraint the whole design rests on.
