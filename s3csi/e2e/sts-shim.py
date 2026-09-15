#!/usr/bin/env python3
"""An STS stand-in for the access drill (aws-access.sh), stdlib only.

flint-s3-broker's `sts` backend forwards `AssumeRoleWithWebIdentity` with
the pod's token to an STS that trusts the cluster's issuer. A trove
cluster's issuer is not published, so AWS STS cannot verify that token.
This shim takes its place: it answers the broker's request by calling
AWS STS `AssumeRole` on a drill role with its OWN IAM user keys, passing
the broker's `RoleSessionName`, `DurationSeconds` and — the point of the
drill — the broker's `Policy` VERBATIM, and returns AWS's response body
unchanged (the broker's parser reads the four credential tags wherever
they are).

What that keeps real: the session policy flint builds, AWS's evaluation
of it against a role whose own policy is bucket-wide, and the keys the
syncer and mount-s3 then use against S3. What it replaces: AWS verifying
the pod's JWT, which the broker has already TokenReviewed.

One JSON line per exchange on stdout — `session`, `policy_present`,
`policy_sha256`, `status` — so the drill can check which grants carried a
policy without trusting the broker's own account of it.

Env: SHIM_UPSTREAM (https://sts.<region>.amazonaws.com), SHIM_REGION,
SHIM_ROLE_ARN, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, [AWS_SESSION_TOKEN],
[SHIM_PORT=8080].
"""

import datetime
import hashlib
import hmac
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

UPSTREAM = os.environ["SHIM_UPSTREAM"].rstrip("/") + "/"
REGION = os.environ["SHIM_REGION"]
ROLE_ARN = os.environ["SHIM_ROLE_ARN"]
AK = os.environ["AWS_ACCESS_KEY_ID"]
SK = os.environ["AWS_SECRET_ACCESS_KEY"]
TOKEN = os.environ.get("AWS_SESSION_TOKEN", "")


def _hmac(key, msg):
    return hmac.new(key, msg.encode(), hashlib.sha256).digest()


def sign(body: bytes, now: datetime.datetime) -> dict:
    """SigV4 for a form POST to STS."""
    url = urllib.parse.urlsplit(UPSTREAM)
    host = url.netloc
    amz_date = now.strftime("%Y%m%dT%H%M%SZ")
    day = now.strftime("%Y%m%d")
    ctype = "application/x-www-form-urlencoded; charset=utf-8"
    payload_hash = hashlib.sha256(body).hexdigest()
    headers = {"content-type": ctype, "host": host, "x-amz-date": amz_date}
    if TOKEN:
        headers["x-amz-security-token"] = TOKEN
    signed = ";".join(sorted(headers))
    canonical = "\n".join(
        [
            "POST",
            url.path or "/",
            "",
            "".join(f"{k}:{headers[k]}\n" for k in sorted(headers)),
            signed,
            payload_hash,
        ]
    )
    scope = f"{day}/{REGION}/sts/aws4_request"
    to_sign = "\n".join(
        ["AWS4-HMAC-SHA256", amz_date, scope, hashlib.sha256(canonical.encode()).hexdigest()]
    )
    k = _hmac(("AWS4" + SK).encode(), day)
    k = _hmac(k, REGION)
    k = _hmac(k, "sts")
    k = _hmac(k, "aws4_request")
    sig = hmac.new(k, to_sign.encode(), hashlib.sha256).hexdigest()
    out = {k2: v for k2, v in headers.items() if k2 != "host"}
    out["authorization"] = (
        f"AWS4-HMAC-SHA256 Credential={AK}/{scope}, SignedHeaders={signed}, Signature={sig}"
    )
    return out


def assume(form: dict) -> tuple:
    params = {
        "Action": "AssumeRole",
        "Version": "2011-06-15",
        "RoleArn": ROLE_ARN,
        "RoleSessionName": form.get("RoleSessionName", "flint-drill")[:64] or "flint-drill",
        "DurationSeconds": form.get("DurationSeconds", "900"),
    }
    if "Policy" in form:
        params["Policy"] = form["Policy"]
    body = urllib.parse.urlencode(params).encode()
    req = urllib.request.Request(UPSTREAM, data=body, method="POST")
    for k, v in sign(body, datetime.datetime.now(datetime.timezone.utc)).items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.send_response(200 if self.path == "/healthz" else 404)
        self.end_headers()
        self.wfile.write(b"ok")

    def do_POST(self):
        n = int(self.headers.get("content-length") or 0)
        form = {k: v[0] for k, v in urllib.parse.parse_qs(self.rfile.read(n).decode()).items()}
        if form.get("Action") != "AssumeRoleWithWebIdentity":
            status, body = 400, b"<ErrorResponse><Error><Code>InvalidAction</Code></Error></ErrorResponse>"
        else:
            status, body = assume(form)
        policy = form.get("Policy")
        print(
            json.dumps(
                {
                    "session": form.get("RoleSessionName"),
                    "policy_present": policy is not None,
                    "policy_sha256": hashlib.sha256(policy.encode()).hexdigest() if policy else None,
                    "status": status,
                    "upstream_error": None if status == 200 else body.decode(errors="replace")[:300],
                }
            ),
            flush=True,
        )
        self.send_response(status)
        self.send_header("content-type", "text/xml")
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    port = int(os.environ.get("SHIM_PORT", "8080"))
    print(json.dumps({"listening": port, "upstream": UPSTREAM, "role": ROLE_ARN}), flush=True)
    sys.stdout.flush()
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
