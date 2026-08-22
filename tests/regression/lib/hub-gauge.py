#!/usr/bin/env python3
"""Print one dotted field out of a hub's /status document, read on stdin.

    curl -s $EP/status | hub-gauge.py tier.gauges.evictedFiles

Exists because an inlined heredoc python in the drill broke on shell
quoting once already (`\"` became two literal characters inside a
single-quoted string, and the poller died with a SyntaxError that read
like the hub was unreachable). A missing field prints EMPTY rather than
raising: the caller is polling for something to APPEAR, and a traceback
in that loop is indistinguishable from a hub that is not answering.
"""
import json
import sys

try:
    doc = json.load(sys.stdin)
except Exception:
    print("")
    sys.exit(0)

cur = doc
for key in sys.argv[1].split("."):
    if not isinstance(cur, dict):
        cur = None
        break
    cur = cur.get(key)

print("" if cur is None else cur)
