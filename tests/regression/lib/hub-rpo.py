#!/usr/bin/env python3
"""Summarise a hub's /status recovery point, for the drill's S3 leg.

Its own file rather than a heredoc inside a shell function, and that is
not tidiness: the first version was inlined in single quotes, so the
`\\"` escapes it needed reached python as literal backslashes and the
whole thing was a SyntaxError. The loop then printed an empty line and
polled for two hundred seconds against a function that could never have
answered.

Reads the /status document on stdin. Prints one line.
"""
import json
import sys

raw = sys.stdin.read().strip()
if not raw:
    print("no /status body")
    sys.exit(0)
try:
    d = json.loads(raw)
except Exception as e:
    print(f"unparseable /status ({e}): {raw[:120]}")
    sys.exit(0)

tier = d.get("tier") or {}
rpo = d.get("rpo") or tier.get("rpo") or {}
clean = d.get("rpoClean")
if clean is None:
    clean = tier.get("rpoClean")

print(
    f"phase={d.get('phase')} rpoClean={clean} "
    f"dirty={rpo.get('dirtyFiles')} manifestCurrent={rpo.get('manifestCurrent')} "
    f"tombstones={rpo.get('tombstones')} beyondRpo={rpo.get('beyondRpo')}"
)
