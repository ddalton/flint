#!/usr/bin/env python3
"""sampler.py — E6, the bucket history of one leg's workspace prefix.

    sampler.py --bucket B --prefix writers/<leg> --out /mnt/nvme/history/<leg>
               [--interval-ms 500] [--until-file F] [--region us-west-1]
               [--endpoint http://127.0.0.1:PORT]   (path-style; tests only)

Runs on the control-plane HOST (python3 stdlib only; pods cannot reach
IMDS, the host can). Every tick:

  1. conditional GET (If-None-Match: the last etag) of
     <prefix>/.flint/lean/current, .../epoch and .../inbox. A 200 saves the
     body as <out>/<name>/<ts_ms>-<etag>.json and appends one index line;
     a 304 saves nothing; a 404 is indexed (etag null, status 404) on the
     first observation and on each change from present to absent.
  2. LIST <prefix>/.flint/lean/manifests/ (every tick — retention keeps
     only five generations, and a generation the pointer skipped is only
     visible in the listing) and GET every generation document not yet
     saved, to <out>/manifests/<key basename>.json.
  3. for a NEW pointer body: GET every chunk it cites that is not yet
     saved, to <out>/chunks/<addr>.json, and its `entries_key` if it names
     one. The syncer's default layout is CHUNKED (`chunked: true` since
     2026-09-04): a chunked pointer names no generation object, so step 2
     sees nothing on a default workspace and the chunks ARE the history.
     Chunks are content-addressed and immutable, so each is fetched once.

index.jsonl: {ts_ms, name, etag, key, bytes, http_date, sent_ms} per change
(name: current|epoch|inbox|manifest|chunk; `http_date` is the store's
Date header — with `sent_ms` and `ts_ms` it brackets the store-vs-local
clock skew to the Date header's one-second resolution).
errors.jsonl: {ts_ms, op, key, status, error, consecutive, total}. Transient
failures (5xx, 403 from an expiring credential, network errors) are logged
and backed off (interval x 2^k, capped at 10 s); the sampler never exits on
one.

At --until-file existence or SIGTERM/SIGINT: one last tick, then a full
LIST of <prefix>/ into listing.json ([{key, etag, size, last_modified}]),
then exit 0. If that final listing cannot be taken (--final-list-retries
attempts), listing.error.json is written instead and the exit is 3: an
empty listing.json would read as "the bucket held nothing".

Credentials: AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_SESSION_TOKEN
if set; otherwise IMDSv2 (token, role, credentials), refreshed 10 minutes
before `Expiration` and on an auth failure. SigV4 is implemented here and
checked against AWS's published vectors by sampler_selftest.py.

--out must not be on the root filesystem's device (the node's 8 GB EBS
root); --allow-rootfs overrides for local tests.
"""
import argparse
import datetime
import hashlib
import hmac
import json
import os
import re
import signal
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET

EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()
ALGO = "AWS4-HMAC-SHA256"
POINTER_NAMES = ("current", "epoch", "inbox")
ABSENT = "<absent>"


def now_ms():
    return int(time.time() * 1000)


# ---------------------------------------------------------------- SigV4 --

def uri_encode(s, encode_slash=True):
    """RFC 3986 encoding as SigV4 wants it: unreserved kept, everything else
    %XX (uppercase), '/' kept only in an S3 object path."""
    return urllib.parse.quote(s, safe="-_.~" if encode_slash else "-_.~/")


def canonical_query(params):
    """params: [(name, value)] — encoded, then sorted by name then value."""
    enc = sorted((uri_encode(k), uri_encode(v)) for k, v in params)
    return "&".join(f"{k}={v}" for k, v in enc)


def _norm_value(v):
    return " ".join(str(v).strip().split())


def canonical_request(method, canonical_uri, canonical_qs, headers, payload_hash):
    """headers: {name: value} — every one of them is signed."""
    hs = {k.lower(): _norm_value(v) for k, v in headers.items()}
    names = sorted(hs)
    return "\n".join([
        method,
        canonical_uri,
        canonical_qs,
        "".join(f"{n}:{hs[n]}\n" for n in names),
        ";".join(names),
        payload_hash,
    ])


def string_to_sign(amz_date, scope, creq):
    return "\n".join([ALGO, amz_date, scope, hashlib.sha256(creq.encode()).hexdigest()])


def signing_key(secret, date, region, service):
    k = hmac.new(("AWS4" + secret).encode(), date.encode(), hashlib.sha256).digest()
    for part in (region, service, "aws4_request"):
        k = hmac.new(k, part.encode(), hashlib.sha256).digest()
    return k


def signature(secret, amz_date, region, service, creq):
    date = amz_date[:8]
    scope = f"{date}/{region}/{service}/aws4_request"
    sts = string_to_sign(amz_date, scope, creq)
    return hmac.new(signing_key(secret, date, region, service), sts.encode(), hashlib.sha256).hexdigest()


def sign(method, canonical_uri, params, headers, access_key, secret, region, service,
         amz_date, payload_hash=EMPTY_SHA256):
    """Return (authorization header value, signature). `headers` must already
    carry host and x-amz-date (and x-amz-content-sha256 / token for S3); all
    of them are signed. The canonical request is built by the module-level
    `canonical_request`, which the self-test mutates to prove this path is
    the one the vectors pin."""
    creq = canonical_request(method, canonical_uri, canonical_query(params), headers, payload_hash)
    sig = signature(secret, amz_date, region, service, creq)
    names = ";".join(sorted(k.lower() for k in headers))
    scope = f"{amz_date[:8]}/{region}/{service}/aws4_request"
    return f"{ALGO} Credential={access_key}/{scope}, SignedHeaders={names}, Signature={sig}", sig


# ---------------------------------------------------------- credentials --

class Creds:
    def __init__(self, access_key, secret, token=None, expiration=None, source="env"):
        self.access_key, self.secret, self.token = access_key, secret, token
        self.expiration, self.source = expiration, source


def parse_iso8601(s):
    return datetime.datetime.strptime(s.replace("Z", "+0000"), "%Y-%m-%dT%H:%M:%S%z").timestamp()


class CredProvider:
    def __init__(self, imds="http://169.254.169.254", refresh_before_s=600, clock=time.time, log=None):
        self.imds, self.refresh_before_s, self.clock = imds.rstrip("/"), refresh_before_s, clock
        self.creds, self.token, self.refreshes = None, None, 0
        self.log = log or (lambda **kw: None)
        ak, sk = os.environ.get("AWS_ACCESS_KEY_ID"), os.environ.get("AWS_SECRET_ACCESS_KEY")
        if ak and sk:
            self.creds = Creds(ak, sk, os.environ.get("AWS_SESSION_TOKEN") or None, None, "env")

    def _imds(self, method, path, headers, timeout=2):
        req = urllib.request.Request(self.imds + path, method=method, headers=headers)
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.read().decode()

    def _refresh(self, retried=False):
        if self.token is None:
            self.token = self._imds("PUT", "/latest/api/token",
                                    {"X-aws-ec2-metadata-token-ttl-seconds": "21600"})
        h = {"X-aws-ec2-metadata-token": self.token}
        try:
            role = self._imds("GET", "/latest/meta-data/iam/security-credentials/", h).split()[0]
        except urllib.error.HTTPError as e:
            if e.code == 401 and not retried:  # the session token expired: mint another, once
                self.token = None
                return self._refresh(retried=True)
            raise
        doc = json.loads(self._imds("GET", f"/latest/meta-data/iam/security-credentials/{role}", h))
        if doc.get("Code", "Success") != "Success":
            raise RuntimeError(f"IMDS credentials: {doc.get('Code')}")
        self.creds = Creds(doc["AccessKeyId"], doc["SecretAccessKey"], doc.get("Token"),
                           parse_iso8601(doc["Expiration"]), "imds")
        self.refreshes += 1

    def get(self, force=False):
        c = self.creds
        if c is not None and c.source == "env":
            return c
        stale = c is None or (c.expiration is not None and self.clock() >= c.expiration - self.refresh_before_s)
        if force or stale:
            try:
                self._refresh()
            except Exception as e:  # keep a still-valid credential across a failed refresh
                self.log(op="imds", key=None, status=getattr(e, "code", None), error=f"{type(e).__name__}: {e}")
                if c is None or (c.expiration is not None and self.clock() >= c.expiration):
                    raise
                return c
        return self.creds


# ------------------------------------------------------------ S3 client --

class HttpResult:
    def __init__(self, status, headers, body, sent_ms, recv_ms):
        self.status, self.headers, self.body = status, headers, body
        self.sent_ms, self.recv_ms = sent_ms, recv_ms


class S3:
    def __init__(self, bucket, region, creds, endpoint=None, timeout=10):
        self.bucket, self.region, self.creds, self.timeout = bucket, region, creds, timeout
        if endpoint:
            u = urllib.parse.urlsplit(endpoint)
            self.scheme, self.host, self.base_path = u.scheme, u.netloc, "/" + bucket
        else:
            if "." in bucket:
                raise SystemExit("virtual-hosted TLS needs a bucket name without dots")
            self.scheme, self.host, self.base_path = "https", f"{bucket}.s3.{region}.amazonaws.com", ""

    def request(self, method, key=None, params=(), extra=None):
        """key None = the bucket (LIST). Returns HttpResult for ANY HTTP status;
        raises only on transport failure."""
        path = self.base_path + ("/" + uri_encode(key, encode_slash=False) if key is not None else "")
        if not path:
            path = "/"
        c = self.creds.get()
        amz_date = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        signed = {"host": self.host, "x-amz-date": amz_date, "x-amz-content-sha256": EMPTY_SHA256}
        if c.token:
            signed["x-amz-security-token"] = c.token
        auth, _ = sign(method, path, list(params), signed, c.access_key, c.secret, self.region, "s3", amz_date)
        qs = canonical_query(list(params))
        url = f"{self.scheme}://{self.host}{path}" + (f"?{qs}" if qs else "")
        headers = dict(signed)
        headers["Authorization"] = auth
        headers.update(extra or {})
        req = urllib.request.Request(url, method=method, headers=headers)
        sent = now_ms()
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as r:
                body = r.read()
                return HttpResult(r.status, r.headers, body, sent, now_ms())
        except urllib.error.HTTPError as e:
            try:
                body = e.read()
            except Exception:
                body = b""
            return HttpResult(e.code, e.headers, body, sent, now_ms())

    def list(self, prefix):
        """Every object under prefix: [{key, etag, size, last_modified}].
        Raises S3Error on any non-200 page."""
        out, token = [], None
        while True:
            params = [("list-type", "2"), ("prefix", prefix)]
            if token:
                params.append(("continuation-token", token))
            r = self.request("GET", None, params)
            if r.status != 200:
                raise S3Error("list", prefix, r.status, r.body[:300])
            root = ET.fromstring(r.body)
            strip = lambda t: t.rsplit("}", 1)[-1]
            truncated, token = False, None
            for el in root:
                tag = strip(el.tag)
                if tag == "Contents":
                    o = {strip(c.tag): (c.text or "") for c in el}
                    out.append({"key": o.get("Key"), "etag": o.get("ETag"),
                                "size": int(o.get("Size") or 0), "last_modified": o.get("LastModified")})
                elif tag == "IsTruncated":
                    truncated = (el.text or "").strip().lower() == "true"
                elif tag == "NextContinuationToken":
                    token = el.text
            if not truncated:
                return out
            if not token:
                raise S3Error("list", prefix, 200, b"truncated page without NextContinuationToken")


class S3Error(Exception):
    def __init__(self, op, key, status, detail):
        super().__init__(f"{op} {key}: HTTP {status} {detail!r}")
        self.op, self.key, self.status = op, key, status


# --------------------------------------------------------------- sampler --

def sanitize(s):
    return re.sub(r"[^A-Za-z0-9._-]", "_", (s or "").strip('"')) or "none"


def write_atomic(path, data):
    tmp = f"{path}.tmp.{os.getpid()}"
    with open(tmp, "wb") as f:
        f.write(data)
    os.replace(tmp, path)


class Sampler:
    def __init__(self, s3, prefix, out, interval_ms=500):
        self.s3, self.prefix, self.out, self.interval_ms = s3, prefix.strip("/"), out, interval_ms
        self.lean = f"{self.prefix}/.flint/lean"
        for d in POINTER_NAMES + ("manifests", "chunks"):
            os.makedirs(os.path.join(out, d), exist_ok=True)
        # per pointer name: None = not yet observed, ABSENT, or the last etag saved
        self.state = {n: None for n in POINTER_NAMES}
        self.saved = {kind: {os.path.splitext(f)[0] for f in os.listdir(os.path.join(out, kind + "s"))
                             if f.endswith(".json")} for kind in ("manifest", "chunk")}
        self.pending_cited = []  # pointer bodies whose cited objects are not all fetched yet
        self.consecutive_errors, self.total_errors = 0, 0
        self.ticks = self.changes = self.slow_ticks = 0
        self._resume_index()

    def _resume_index(self):
        """A restarted sampler conditions on the last etag it recorded."""
        p = os.path.join(self.out, "index.jsonl")
        if not os.path.exists(p):
            return
        with open(p) as f:
            for line in f:
                try:
                    o = json.loads(line)
                except ValueError:
                    continue
                if o.get("name") in self.state:
                    self.state[o["name"]] = o.get("etag") or ABSENT

    def _append(self, fname, obj):
        with open(os.path.join(self.out, fname), "a") as f:
            f.write(json.dumps(obj, sort_keys=True) + "\n")

    def error(self, op, key, status, error):
        self.total_errors += 1
        self._append("errors.jsonl", {"ts_ms": now_ms(), "op": op, "key": key, "status": status,
                                      "error": error, "consecutive": self.consecutive_errors + 1,
                                      "total": self.total_errors})

    def index(self, name, key, etag, body, r, **extra):
        line = {"ts_ms": r.recv_ms, "sent_ms": r.sent_ms, "name": name, "etag": etag, "key": key,
                "bytes": None if body is None else len(body),
                "http_date": r.headers.get("Date") if r.headers else None}
        line.update(extra)
        self._append("index.jsonl", line)
        self.changes += 1

    def poll_pointer(self, name):
        """Conditional GET of one pointer object. Returns the body on a change
        to a present object, else None. Raises S3Error on any other status."""
        key = f"{self.lean}/{name}"
        last = self.state[name]
        extra = {"If-None-Match": last} if last not in (None, ABSENT) else None
        r = self.s3.request("GET", key, (), extra)
        if r.status == 304:
            return None
        if r.status == 200:
            etag = r.headers.get("ETag")
            if etag == last:
                return None  # a store that ignored If-None-Match: nothing new
            write_atomic(os.path.join(self.out, name, f"{r.recv_ms}-{sanitize(etag)}.json"), r.body)
            self.state[name] = etag
            self.index(name, key, etag, r.body, r)
            return r.body
        if r.status == 404:
            if last != ABSENT:  # first observation, or a change to absence
                self.state[name] = ABSENT
                self.index(name, key, None, None, r, status=404)
            return None
        raise S3Error("get", key, r.status, r.body[:300])

    def fetch_once(self, kind, key, ident):
        """GET an immutable object (a generation document or a chunk) not yet saved."""
        r = self.s3.request("GET", key)
        if r.status == 200:
            write_atomic(os.path.join(self.out, kind + "s", f"{sanitize(ident)}.json"), r.body)
            self.saved[kind].add(sanitize(ident))
            self.index(kind, key, r.headers.get("ETag"), r.body, r)
            return
        if r.status == 404:
            # listed (or cited) at this tick, gone by the GET: record the miss, never retry-loop on it
            self.index(kind, key, None, None, r, status=404)
            self.error("get", key, 404, f"{kind} gone before it could be read")
            return
        raise S3Error("get", key, r.status, r.body[:300])

    def tick(self):
        """One sampling pass. Raises on a transient failure (the caller backs
        off); everything already saved stays saved, and a pointer whose cited
        objects were not all fetched is retried on the next tick."""
        self.ticks += 1
        body = self.poll_pointer("current")
        if body is not None:
            self.pending_cited.append(body)
        self.poll_pointer("epoch")
        self.poll_pointer("inbox")
        while self.pending_cited:
            self.fetch_cited(self.pending_cited[0])
            self.pending_cited.pop(0)
        mprefix = f"{self.lean}/manifests/"
        for o in self.s3.list(mprefix):
            base = o["key"][len(mprefix):]
            if base and sanitize(base) not in self.saved["manifest"]:
                self.fetch_once("manifest", o["key"], base)

    def safe_tick(self, creds=None):
        """tick() with the error accounting the main loop needs: a failure is
        logged to errors.jsonl and counted, never raised. Returns True on success."""
        try:
            self.tick()
            self.consecutive_errors = 0
            return True
        except Exception as e:
            status = getattr(e, "status", None)
            self.error("tick", getattr(e, "key", None), status, f"{type(e).__name__}: {e}")
            self.consecutive_errors += 1
            if creds is not None and status in (400, 403):  # an expired or rotated credential
                try:
                    creds.get(force=True)
                except Exception:
                    pass
            return False

    def fetch_cited(self, body):
        try:
            ptr = json.loads(body)
        except ValueError as e:
            self.error("parse", f"{self.lean}/current", None, f"pointer body is not JSON: {e}")
            return
        if not isinstance(ptr, dict):
            return
        ek = ptr.get("entries_key")
        if isinstance(ek, str) and ek.startswith(f"{self.lean}/manifests/"):
            base = ek.rsplit("/", 1)[-1]
            if sanitize(base) not in self.saved["manifest"]:
                self.fetch_once("manifest", ek, base)
        for c in ptr.get("chunks") or []:
            addr = c.get("addr") if isinstance(c, dict) else None
            if isinstance(addr, str) and addr and sanitize(addr) not in self.saved["chunk"]:
                self.fetch_once("chunk", f"{self.lean}/chunks/{addr}", addr)

    def final_listing(self, retries=10, backoff_s=2.0):
        """listing.json on success (returns the object count); after `retries`
        failures listing.error.json and None — never an empty listing."""
        last = None
        for attempt in range(retries):
            try:
                objs = self.s3.list(self.prefix + "/")
                write_atomic(os.path.join(self.out, "listing.json"), json.dumps(objs, indent=1).encode())
                return len(objs)
            except Exception as e:
                last = f"{type(e).__name__}: {e}"
                self.error("final-list", self.prefix + "/", getattr(e, "status", None), last)
                if attempt + 1 < retries:
                    time.sleep(min(backoff_s * (2 ** attempt), 30))
        write_atomic(os.path.join(self.out, "listing.error.json"),
                     json.dumps({"ts_ms": now_ms(), "attempts": retries, "error": last}).encode())
        return None


def same_device_as_root(path):
    p = os.path.abspath(path)
    while not os.path.exists(p):
        p = os.path.dirname(p)
    return os.stat(p).st_dev == os.stat("/").st_dev


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--bucket", required=True)
    ap.add_argument("--prefix", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--interval-ms", type=int, default=500)
    ap.add_argument("--until-file")
    ap.add_argument("--region", default="us-west-1")
    ap.add_argument("--endpoint", help="path-style endpoint, e.g. http://127.0.0.1:9000 (tests)")
    ap.add_argument("--imds", default="http://169.254.169.254")
    ap.add_argument("--final-list-retries", type=int, default=10)
    ap.add_argument("--final-list-backoff-s", type=float, default=2.0)
    ap.add_argument("--allow-rootfs", action="store_true")
    a = ap.parse_args(argv)

    if not a.allow_rootfs and same_device_as_root(a.out):
        print(f"sampler: refusing --out {a.out}: it is on the root filesystem's device "
              "(evidence belongs on /mnt/nvme); --allow-rootfs overrides", file=sys.stderr)
        return 2
    os.makedirs(a.out, exist_ok=True)

    stop = {"sig": None}

    def on_signal(signum, _frame):
        stop["sig"] = signum

    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGINT, on_signal)

    sampler = None
    creds = CredProvider(a.imds, log=lambda **kw: sampler and sampler.error(**kw))
    s3 = S3(a.bucket, a.region, creds, a.endpoint)
    sampler = Sampler(s3, a.prefix, a.out, a.interval_ms)
    started = now_ms()
    print(f"sampler: s3://{a.bucket}/{sampler.lean} -> {a.out} every {a.interval_ms} ms", file=sys.stderr)

    def should_stop():
        return stop["sig"] is not None or (a.until_file and os.path.exists(a.until_file))

    while True:
        finishing = should_stop()  # decided BEFORE the tick: one last pass, then stop
        t0 = time.monotonic()
        sampler.safe_tick(creds)
        if finishing:
            break
        interval = a.interval_ms / 1000
        if time.monotonic() - t0 > interval:
            sampler.slow_ticks += 1
        delay = interval if not sampler.consecutive_errors else min(interval * (2 ** sampler.consecutive_errors), 10.0)
        next_at = t0 + delay
        while time.monotonic() < next_at and not should_stop():
            time.sleep(min(0.05, max(0.0, next_at - time.monotonic())))

    n = sampler.final_listing(a.final_list_retries, a.final_list_backoff_s)
    run = {"started_ms": started, "stopped_ms": now_ms(), "signal": stop["sig"], "ticks": sampler.ticks,
           "changes": sampler.changes, "errors": sampler.total_errors, "slow_ticks": sampler.slow_ticks,
           "credential_refreshes": creds.refreshes, "listing_objects": n,
           "bucket": a.bucket, "prefix": sampler.prefix, "interval_ms": a.interval_ms}
    write_atomic(os.path.join(a.out, "run.json"), json.dumps(run, indent=1).encode())
    print(f"sampler: stopped; {json.dumps(run)}", file=sys.stderr)
    return 0 if n is not None else 3


if __name__ == "__main__":
    sys.exit(main())
