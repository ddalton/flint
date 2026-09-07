#!/usr/bin/env python3
"""Watch one syncer's /status and time its wake, at 50 ms.

The wall clock from "the holder died" to "the successor serves" carries
up to one claim poll of quantisation, and on a loopback store that noise
is bigger than the restore it is supposed to measure. So this records
BOTH edges: when the successor entered `importing` (it had claimed) and
when it reached `serving`. The span between them is the successor's own
work, with the poll outside it.

    watchphase.py <status-port> <seconds>   ->  "<import_ms> <serving_ms>"
"""
import json
import sys
import time
import urllib.request

port, secs = sys.argv[1], float(sys.argv[2])
t0 = time.time()
imp = srv = None
while time.time() - t0 < secs:
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/status", timeout=1) as r:
            phase = json.load(r).get("phase")
        if imp is None and phase in ("importing", "serving", "sweeping"):
            imp = time.time() - t0
        if phase == "serving":
            srv = time.time() - t0
            break
    except Exception:
        pass
    time.sleep(0.05)
print(int((imp if imp is not None else -0.001) * 1000),
      int((srv if srv is not None else -0.001) * 1000))
