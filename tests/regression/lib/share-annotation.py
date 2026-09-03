#!/usr/bin/env python3
"""Read one annotation off a FlintShare, given `kubectl get -o json` on stdin.

Why not `-o jsonpath`: the key is `chert.us/requested-at`, which carries
BOTH a dot and a slash. `{.metadata.annotations.flint\\.io/requested-at}`
looks right and silently yields nothing — which is how this drill
reported "the gateway did not arm the wake annotation" against a gateway
whose unit tests prove it emits exactly that merge patch. A false
failure in a drill is worse than no drill: it sends you to debug working
code.

Usage:  kubectl get flintshare X -o json | share-annotation.py <key>
Prints the value, or nothing if absent. Never fails on a missing key.
"""
import json
import sys

key = sys.argv[1]
raw = sys.stdin.read().strip()
if not raw:
    sys.exit(0)
try:
    obj = json.loads(raw)
except Exception:
    sys.exit(0)
print((obj.get("metadata", {}).get("annotations") or {}).get(key, ""))
