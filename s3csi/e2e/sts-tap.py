#!/usr/bin/env python3
"""A logging pass-through in front of a real STS, for the local access drill
(local-access.sh), stdlib only.

flint-s3-broker's `sts` backend POSTs `AssumeRoleWithWebIdentity` to this
tap, which forwards the form body to SHIM_UPSTREAM byte for byte and returns
the upstream's status and body unchanged. Unlike sts-shim.py it signs and
rewrites nothing: the STS behind it verifies the pod's token itself.

One JSON line per exchange on stdout — `session`, `action`, `policy` (the
text, or null), `status` — so the drill can check which grants carried a
policy, and read the policy flint actually sent, without trusting the
broker's own account of it. The token is never logged.

Env: SHIM_UPSTREAM (e.g. http://minio.flint-system.svc:9000/), [SHIM_PORT=8080].
"""

import json
import os
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

UPSTREAM = os.environ["SHIM_UPSTREAM"]


class Tap(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok\n")

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        form = {k: v[0] for k, v in urllib.parse.parse_qs(body.decode()).items()}
        req = urllib.request.Request(UPSTREAM, data=body, method="POST")
        req.add_header("Content-Type", self.headers.get("Content-Type") or "application/x-www-form-urlencoded")
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                status, out = r.status, r.read()
        except urllib.error.HTTPError as e:
            status, out = e.code, e.read()
        except OSError as e:
            status, out = 502, str(e).encode()
        print(json.dumps({
            "session": form.get("RoleSessionName"),
            "action": form.get("Action"),
            "policy": form.get("Policy"),
            "status": status,
        }), flush=True)
        self.send_response(status)
        self.send_header("Content-Type", "text/xml")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)


if __name__ == "__main__":
    port = int(os.environ.get("SHIM_PORT", "8080"))
    print(json.dumps({"listening": port, "upstream": UPSTREAM}), flush=True)
    ThreadingHTTPServer(("", port), Tap).serve_forever()
