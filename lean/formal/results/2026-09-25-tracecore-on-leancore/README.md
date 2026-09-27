# The trace check against LeanCore, as it stood before P2 (archived 2026-09-25)

`TraceCore.tla` targeted `LeanCore.tla`: the cell's entries and declared
removals, consumed by writers. Its last full run (2026-09-25, before step 5
slice 3) was **44/44**: 16 traces accepted, 16 mutations and 12 controls
rejected. `traces-core/` here is that code's output.

After slices 1-3 a UI save, delete and rename commit in one CAS, and the
cell holds no entries. No trace of that code can be a `LeanCore` behaviour:
there, a UI write never moves the document. The check now targets
`LeanP2.tla` (`trace/`). These files are kept, unchanged, as the record.
