# writers-live — the rig for the many-agents live drill

Plan of record: `docs/plans/flint-lean-writers-live-drill-plan.md`. This
file pins the INTERFACES between the pieces, so each can be built and
tested on its own: the agent's journal, the syncer's event trace, the
collection layout, and what each oracle reads.

| Piece | Runs | Reads | Writes |
|---|---|---|---|
| `agent.sh` | in each agent pod (busybox sh) | its workspace tree, `.flint/publish.ack` | the tree, `.flint/publish`, its journal |
| `oracle.py` | on the Mac, over a collected leg | the collect layout below | a verdict JSON; exit 1 on any failed oracle |
| `oracle_selftest.py` | on the Mac | synthetic legs it builds | pass/fail per injected fault |
| `timeline.py` | on the Mac | traces, clock offsets, optional access logs, history | one chronological table, filterable by path/time |
| `drill.sh` | on the Mac, driving the cluster over SSM | — | runs one leg end to end |

## 1. The agent journal

`/agent/journal.jsonl` — OUTSIDE the tree (an emptyDir), because anything
inside the tree is published. Append-only, one JSON object per line;
uploaded at collection.

The **UI actor** (`ui.sh`, in the verifier pod) journals the same way,
with `agent: "ui"`: each op is a gateway HTTP write
(`/lean/v1/<workspace>/files/<path>`), and its "ack" is the gateway's
success response carrying the new etag (the write is durable in the
bucket and tracked in the inbox from that moment), recorded as
`{"k":"ack","agent":"ui","nonce":"ui-<n>","status":"ok","etag":"…"}`
with no `seq`. O3 treats a UI write's ack seq as "any later op".

Two line kinds, joined by `nonce`:

```json
{"k":"op","agent":"a3","n":17,"t_ms":1757800000123,"op":"write","path":"hot/p07.txt","sha256":"…","base":"…|absent","nonce":"a3-9"}
{"k":"op","agent":"a3","n":18,"t_ms":1757800000125,"op":"delete","path":"hot/p11.txt","base":"…","nonce":"a3-9"}
{"k":"op","agent":"a3","n":19,"t_ms":1757800000127,"op":"mv","path":"hot/p02.txt","to":"hot/p29.txt","base":"…","to_base":"…|absent","sha256":"…","nonce":"a3-9"}
{"k":"ack","agent":"a3","t_ms":1757800004410,"nonce":"a3-9","status":"ok","seq":412,"dropped":[],"covered":["a3-8","a3-9"]}
{"k":"ack","agent":"a3","t_ms":1757800019000,"nonce":"a3-10","status":"no-ack"}
```

- `base` is the sha256 of the path in THIS agent's tree immediately
  before the op (`absent` if missing). It is what lets the oracle tell a
  sequential edit (base = the prior acked content) from a concurrent one.
- An agent touches each path at most once per publish batch.
- One publish per batch: `printf '{"nonce":"<agent>-<batch>"}' > .flint/publish`,
  then wait until `.flint/publish.ack`'s `nonces` contains the nonce
  (timeout = 3 × floorSecs ⇒ an ack line with `status:"no-ack"`).
- `status` is the ack's `status` (`ok`, `partial`); `dropped` is
  `report.dropped`; `seq` is the ack's `seq`; `covered` is `nonces`.
- Modes (`AGENT_MODE`): `disjoint` (own subtree `<agent>/`), `hot` (shared
  `hot/p00..p29.txt`, content tagged with agent + n), `churn` (shared
  `churn/p00..p49.txt`: 40% write, 30% delete, 20% mv, 10% write of the
  path's current content), `vocab` (shared set, bodies from 8 fixed
  strings, so identical bytes are the norm). Seeded by `AGENT_SEED`.
- Control: `/agent/pause` present ⇒ finish the current batch and idle;
  `/agent/stop` ⇒ final publish, wait for its ack, write
  `{"k":"stopped"}`, exit 0.

## 2. The syncer event trace (`FLINT_SYNC_EVENT_TRACE=1`)

One JSON object per line on stderr, every line starting `{"ts_ms":`
(other stderr lines are prose and are skipped by parsers).

Common fields, on EVERY line: `ts_ms` (wall, epoch ms), `mono_ms`
(monotonic since the process started), `holder` (holder id), `ev`.
`flush` (the barrier's `flush_uuid`) appears ONLY on `scan`, `upload`,
`observed`, `merge`, `cas`, `gc`, `drill_hold` and `queue`. Parsers must
tolerate events and fields they do not know. (Corrected 2026-09-13 to the
trace as built; the syncer side pins these names in a Rust test.)

| `ev` | Fields |
|---|---|
| `barrier_start` | `source`, `declared` |
| `consume` | `path`, `etag`, `from` (`queue`/`inbox`), `action` (`already`, `missing`, `superseded`, `dirty-preserved`, `refused-checksum`, `refused`, `deferred`, `adopted`) |
| `tombstone` | `path`, `action` (`absent`, `deferred`, `kept-dirty`, `removed`) |
| `scan` | `flush`, `uploads`, `deletes`, `first_absence` |
| `upload` | `flush`, `path`, `outcome` (`put`, `adopted`, `parked` — `etag` is the foreign one, `deferred` — no `etag`), `etag` |
| `claim` | `verdict`: `claimed` with `how` (`fresh`, `adopted-own`, `deposed`, `orphaned-own`, `skipped-handoff`, `released`) and `epoch`, plus `prior` `{holder, epoch, released, handoff, waiters}` on a takeover; `waiting` with `behind`, `quiet_polls`, `waited_ms`; `deadline` with `behind`, `waited_ms`. There is no `verdict: "deposed"`: a deposal is `claimed` with `how: "deposed"` |
| `observed` | `flush`, `path`, `etag`, `still`, `own_put` (bool: the barrier's own PUT rather than an adopt or a citation repair — every citation a commit adds is re-read under the fence, finding 13) |
| `merge` | `flush`, `theirs_seq`, `upserts`, `deletes`, `foreign`, `gone`, `adds_nothing` (bool) |
| `cas` | `flush`, `seq`, `expected`, `result`: `ok` (with `etag`) or `lost` (no `etag`) |
| `gc` | `flush`, `path`, `head`, `result`: `absent` (`head` null), `deleted` (`head` = the etag deleted), `skip` (`head` = the unrecognized etag, plus `recognized`), `replaced-absent` (the conditional DELETE got 412 and the re-HEAD found the object gone; `head` = the etag the GC recognized). Tools treat any other `result` as informational |
| `queue` | `flush`, `upserts`, `tombstones` |
| `release` | `epoch`, `waiters_at_claim` |
| `fence` | `where`, `detail` |
| `drill_hold` | `flush`, `where`, `path`, `secs` |
| `barrier_end` | `seq`, `uploaded`, `deleted`, `parked`, `consumed`, `no_change`, `ms`, `requests` (`{get,head,put,copy,delete,list,multipart}`, cumulative per process, or null) |
| `ack` | `verb` (the Rust Debug name), `nonces`, `status`, `seq`, `dropped`, `boundary` |
| `sync` | `scoped`, `applied`, `deleted`, `conflicts`, `seq`, `hidden` |

There is no `baseline` event and no `skew` event. Every line names its
`holder` (before a pod's first claim it used to be null; fixed). A real
two-writer trace from `the_event_trace_reconstructs_a_two_writer_interleaving`
(lean/syncer/src/tests.rs) is kept at `testdata/trace-two-writers.jsonl`.

## 3. The collect layout (per leg)

```
collect/<leg>/
  meta.json                     {leg, prefix, floor_secs, agents:[…], t_start_ms, t_quiesce_ms, t_end_ms}
  agents/<agent>/journal.jsonl
  agents/<agent>/tree.sha256    `sha256  path` lines, sorted, paths relative to the tree root,
                                excluding any component starting with ".flint"
  agents/<agent>/conflicts.jsonl  the writer's .flint-sync/conflicts.jsonl
  agents/<agent>/state.tar.gz   .flint-sync/ and .flint/ (E7)
  bucket/manifest.json          the resolved manifest: {seq, entries:{path:{key,etag,crc64_b64,size}}}
  bucket/listing.json           every object under the prefix: [{key,etag,size,last_modified}]
  bucket/preserved.json         every object under <prefix>/.flint/lean/conflicts/: [{key, etag, sha256}]
  bucket/heads.json             HEAD of every citation: {path:{etag|null}}
  checkout/tree.sha256          a fresh reader checkout's digest (same format)
  checkout/exit.json            {code, stderr_tail}
  traces/<agent>.jsonl          the E1 lines of that agent's worker (from the node shipper)
  nodes/<node>/chrony.jsonl     {ts_ms, offset_ms}
```

## 4. The oracles (`oracle.py <collect/leg> [--idle-from MS --idle-to MS]`)

- **O1** `checkout/exit.json` code 0, and every `bucket/heads.json` etag
  equals the manifest's.
- **O2** every `agents/*/tree.sha256` equals `checkout/tree.sha256`
  (a diff names the paths: extra, missing, differing).
- **O3** for every `op` whose nonce is covered by an ack with `status`
  `ok`, or `partial` without the path in `dropped` — call its content `h`
  and its ack seq `s`: accounted for if the final manifest's content for
  the path is `h` (via `agents/*/tree.sha256` = checkout digest), OR a
  later-acked op on the path (ack seq ≥ `s`, any agent) has `base == h`,
  OR `h` is the sha256 of a preserved copy. Otherwise: LOSS, reported with
  the op, its ack, and every later op on the path.
- **O4** preserved objects in `bucket/preserved.json` equal, one for one,
  the `preserved_key`s named by `upload-412-preserved` and `consume-dirty`
  records across all `agents/*/conflicts.jsonl`, and every named key exists.
- **O5** across `traces/`: `claim` `verdict == "deadline"` count and the
  deposal count — `verdict == "claimed"` with `how == "deposed"` (both
  must be 0 unless the leg declares faults), `waiting` > 0. `how ==
  "orphaned-own"` is counted and reported, never failed.
- **O6** (`--idle-*` window): no `cas` with `result ok`, no `claim` with
  `claimed`, and per-writer `barrier_end.requests` deltas per tick at or
  under the declared baseline.

Output: `{"leg":…, "oracles":{"O1":{"pass":true,"details":…}, …}, "pass":bool}`.

## 5. Implementation notes

Decisions made while building `agent.sh`, `ui.sh`, `oracle.py`,
`oracle_selftest.py`, `timeline.py` and `agent_localtest.sh`, where §1–§4
were silent or did not fit. Self-tests: `python3 oracle_selftest.py`
(42 synthetic scenarios + 13 checks on the real trace fixture),
`python3 timeline.py --selftest`, `bash agent_localtest.sh`.

### 5.1 The agent (`agent.sh`)

- **Temp files.** Content is written to `.<name>.<agent>-<n>.flint-sync-tmp`
  beside the target and renamed. The syncer's scan skips the
  `.flint-sync-tmp` suffix (`scan.rs`), so no scan publishes a half-written
  temp; the name differs from the syncer's own `<name>.flint-sync-tmp`. The
  sentinel is written to `.flint/.publish.<agent>.tmp` and renamed: a torn
  body is honoured as a bare touch WITHOUT the nonce, which would read as a
  false `no-ack`.
- **The base read is race-checked.** The syncer also writes the tree
  (consumes), so "the file just before the op" can change under the agent.
  The inode is read before hashing and again just before the rename/unlink;
  a change retries (`RACE_TRIES`, default 5, then a `skip` line). An absent
  path is created with `ln` (fails if something appeared) rather than `mv`.
  **Residual:** the gap between the last inode check and the `mv`/`rm`
  exec (milliseconds) cannot be closed in BusyBox sh (no RENAME_EXCHANGE).
  An O3 LOSS whose superseding op sits within milliseconds of a `consume` of
  that path in that writer's trace is a rig suspect: read it with
  `timeline.py --path P --journals --context` before calling it a defect.
- **Mode sets.** `disjoint` `<agent>/pNN.txt` ×50, mix 80/10/10/0;
  `hot` `hot/pNN.txt` ×30, writes only; `churn` `churn/pNN.txt` ×50,
  40/30/20/10; `vocab` `vocab/pNN.txt` ×30, 70/30/0/0 (A4 wants deletes).
  Mix is write/delete/mv/same-bytes; `AGENT_PATHS`, `AGENT_MIX` override.
  A delete, mv or same-bytes of an absent path becomes a write; a mv takes
  an unused destination and may overwrite it (`to_base`).
- **Extra journal kinds** (the oracle ignores unknown kinds): `start`,
  `paused`, `resumed`, `stopped`, `terminated` (each with `t_ms`, `batch`,
  `n`) and `skip` (`path`, `reason`, `nonce`). An `ack` line also carries
  `report` `{uploaded, deleted, parked, consumed, no_change}` (A1's
  consumed-from-peers guard), `seq: null` when the ack has none, and status
  `publish-failed` if the sentinel could not be written.
- **Restart.** `n` and the batch resume from the journal (never reused);
  the LCG is re-seeded from (`AGENT_SEED`, batch).
- **Stop** publishes one final batch with no ops. SIGTERM journals
  `terminated` and exits 143 at the next batch boundary.
- **Clock:** `date +%s%N`, else `gdate`, else `/proc/uptime` anchored at a
  second boundary (10 ms), else perl, else whole seconds.
- **Ack parsing** without jq: flatten the file, extract `status`, `seq`,
  `nonces`, `report.dropped` and the counters by key with `sed`; a nonce is
  covered only when it appears QUOTED in `nonces`. Tested on serde compact
  and pretty output, including a conflict record whose `kind` string
  contains `"dropped": [...]` and `"nonces"`.

### 5.2 The UI actor (`ui.sh`)

- Read from `lean/gateway/src/{http,workspace}.rs` at HEAD, **not observed
  on the wire** (the ASSUMPTION block in `ui.sh` is the one place to fix):
  every route needs `Authorization: Bearer` (`GATEWAY_TOKEN` or
  `GATEWAY_TOKEN_FILE`); an overwrite without `If-Match` is refused 428. So
  each write is GET then PUT: `base` = sha256 of the bytes the GET served,
  `If-Match` = its `etag` header; a 404 gives `base: "absent"` and
  `If-None-Match: *`. Success = 2xx with `{"etag": …}`.
- A 409 (window open, concurrent write) or 412 (file changed) is journaled
  as an `ack` with `status: "refused"`, `http`, `error`, and retried from a
  fresh GET up to `UI_RETRIES` (4); every attempt is its own op line and
  nonce `ui-<n>`. 5xx or transport failure: `status: "error"`. O3 counts
  neither. The UI never re-sends a 412's `etag` header to force an
  overwrite of bytes it did not read (for example an uncited object left
  after a delete): that would manufacture losses.
- `ack` ok lines carry `etag` (the response's JSON string, still escaped)
  and `http`; op lines carry `base_etag`. `ui.sh` sources `agent.sh`
  (`AGENT_LIB_ONLY=1`); defaults `JOURNAL=/ui/journal.jsonl`,
  `CONTROL_DIR=/ui`. Collected as `agents/ui/journal.jsonl`, with no tree.

### 5.3 Collect layout additions

- `meta.json` `agent_nodes: {"<agent>": "<node>", "ui": "<node>"}` maps each
  journal and trace source to its node. `nodes/<node>/chrony.jsonl`
  `offset_ms` = node clock − true time (positive: the node is AHEAD;
  chronyc's "N seconds fast" is +N×1000). A stamp `ts` is corrected to
  `ts − offset(ts)`, linearly interpolated, clamped at the ends. No map or
  no samples: uncorrected.
- Collect `agents/<agent>/conflicts.1.jsonl` too (the syncer's rotated
  log, `CONFLICTS_PREV` in `state.rs`); O4 reads it when present. A
  `conflicts.dropped` file means records were rotated away and O4 cannot be
  exact.
- `tree.sha256` accepts `sha256  path` or `sha256 *path`, strips a leading
  `./`, and the oracle re-applies the exclusion: any component starting
  `.flint` AND any name ending `.flint-sync-tmp`.
- `heads.json` values may be `{"etag": …}`, `{"etag": null}` or bare;
  etags compare without quotes and `W/`. `preserved.json` keys may carry the
  prefix (O4 strips `meta.prefix/`).
- `traces/<name>.jsonl`: one file per worker process lineage (agent name or
  holder id). O5's "followed by a claim" is within one file.

### 5.4 Oracles (`oracle.py`)

- `--oracles O1,O3,…` selects per leg (A4 cannot run O4: a deleted worker
  pod takes its conflicts log). Unselected, or O6 with no window:
  `"pass": null`. `--faults-declared` and `--wall-slack-ms` (default 0) are
  flags; `meta.json` does not carry them.
- **O1** also fails when a citation has no HEAD, and when the checkout
  digest's paths differ from the manifest's entry paths (a checkout that
  exits 0 but wrote the wrong set).
- **O2** fails with no checkout digest, no agent trees, or a non-`ui` agent
  in `meta.agents` with no tree.
- **O3.** *Acked* = the op's nonce is in `covered` (or is the nonce) of an
  `ok`/`partial` ack in the same agent's journal, and for `partial` the
  op's path is not in `dropped` — so a `no-ack` batch later covered counts,
  at that later ack's seq. *Later* = both acks carry an integer seq: seq_Y ≥
  seq_X; otherwise (a UI write, a null seq): the op's journal time t_Y ≥ t_X
  (clock-corrected, minus the slack). The op time of X, not its ack time,
  because h exists from X's publish onwards and a UI GET can read it before
  X's agent has polled its ack file. The later op must itself be acked (as
  §4 says); a LOSS flags `unacked_op_with_base_h` for A4, where a killed
  agent's unacked edit may still have published. A preserved copy accounts
  for a write only if its key names the SAME path
  (`.flint/lean/conflicts/<uuid>/<path>`). Final content = the checkout
  digest (fallback when absent: paths where every agent tree agrees). Zero
  acked writes, or a malformed acked op, fails O3. Each LOSS lists every op
  on the path after X, with ack status and seq, and a `timeline.py` command.
- **O4** is a set equality both ways (duplicates reported); a
  `preserved_key` on a record of another kind does not count.
- **O5** per the corrected §2: a deposal is `claimed` + `how: "deposed"`;
  the old `verdict: "deposed"` is not counted; `orphaned-own` is reported
  and never fails. With `--faults-declared`, deposals do not fail and each
  `deadline` must be followed by a `claimed` in the same trace.
- **O6**: window inclusive, trace times clock-corrected. Delta = change in
  the SUM of `barrier_end.requests` between consecutive `barrier_end`s of
  one trace (the last before the window is the reference; a decrease is a
  restart, counted and skipped). Every trace needs ≥ 1 tick in the window,
  and with `--request-baseline` ≥ 1 measurable delta (null `requests` would
  otherwise pass silently). `cas` `lost` is not an install.

### 5.5 Timeline (`timeline.py`)

`--context` adds the path-less events of the barrier enclosing a match —
`barrier_start`..`barrier_end` in the same trace — and events sharing its
`flush`. E5 access logs and E6 history are not merged yet.

### 5.6 Contract concerns for the drill

- §3's digest exclusion ("any component starting `.flint`") is broader
  than the scan, which skips only the root `.flint`/`.flint-sync` and the
  `.flint-sync-tmp` suffix: `hot/.flintx` would be published yet excluded
  from every digest. Harmless for this rig; but the collector must also
  exclude `*.flint-sync-tmp`, or an agent killed mid-write leaves a temp in
  its tree that is never in the manifest (a false O2).
- The plan's §1 journal shape (the ack embedded in the op line) differs
  from §1 here (separate `ack` lines); this README was implemented.
- O4 needs every writer's conflicts log from the leg's whole life; any leg
  that deletes a worker pod needs E7 before each deletion or must drop O4.
