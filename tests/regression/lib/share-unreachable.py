#!/usr/bin/env python3
"""Name every Ready FlintShare the operator cannot poll, read on stdin.

    kubectl -n ns get flintshare -o json | share-unreachable.py

Prints a comma-separated list of `name(reason)` for shares carrying
`HubReachable=False`, or nothing when all is well. Prints `UNREADABLE`
— never nothing — when the input is not the JSON it expected, because
the caller's test is `[ -z "$output" ]` and a silent failure there is
an assertion that passes by not looking.

This lives in a file rather than inline for a reason worth keeping.
The first version was `kubectl ... | python3 - <<'PY'`, which does not
work at all: `python3 -` reads its PROGRAM from stdin and the heredoc
is the later redirection, so it wins — the pipe from kubectl is
discarded, `json.load(sys.stdin)` sees an already-consumed stream, and
the script dies with a JSONDecodeError. Stdout stays empty, the
caller's `[ -z ... ]` succeeds, and the leg reports a pass. It printed
a traceback into the middle of a green run for exactly this reason.

Only `HubReachable` on a READY share counts: a parked share is not
polled at all, so a False there is stale rather than news.
"""
import json
import sys

try:
    doc = json.load(sys.stdin)
    items = doc["items"]
except Exception as e:
    print(f"UNREADABLE({type(e).__name__})")
    sys.exit(0)

bad = []
for item in items:
    status = item.get("status") or {}
    if status.get("phase") != "Ready":
        continue
    for cond in status.get("conditions") or []:
        if cond.get("type") == "HubReachable" and cond.get("status") == "False":
            bad.append(f"{item['metadata']['name']}({cond.get('reason')})")
print(",".join(bad))
