#!/usr/bin/env python3
"""sampler_selftest.py — sampler.py checked with no AWS.

1. SigV4 against AWS's PUBLISHED vectors (hardcoded expected signatures):
   - S3 "GET Object" (examplebucket, AKIAIOSFODNN7EXAMPLE, 20130524, Range),
   - S3 "GET Bucket (List Objects)" (the same keys; a query string),
   - the SigV4 test suite's "get-vanilla" (AKIDEXAMPLE, 20150830T123600Z).
   Mutations: one character of the canonical request changed must change the
   signature, both on the string and through `sign()` (the path the sampler
   uses) with the module's `canonical_request` wrapped to alter one byte.
2. The sampler against a local fake S3 (http.server in a thread, path-style)
   that VERIFIES every request's SigV4 signature from the request it received
   (the host, path and query actually sent — what the vectors cannot cover),
   serves ETags, honours If-None-Match with 304, paginates LIST, and injects
   503s, dropped connections and a LIST/GET race. Ticks are driven one by one
   (never by a timer): absence recorded once, 304s save nothing, a pointer
   change across ticks saves every generation LISTED at a tick — including
   one reaped right after, and not one that came and went between ticks —
   chunked pointers fetch each cited chunk once, a failed chunk fetch is
   retried on the next tick though the pointer is then a 304.
   A control: a wrong secret is refused by the fake store (its check can fail).
3. The CLI end to end (subprocess): --until-file and SIGTERM both end with a
   listing.json equal to the store's objects and exit 0; a final LIST that
   keeps failing exits 3 with listing.error.json and NO listing.json.
4. IMDSv2 credentials against a fake IMDS: refresh 10 minutes before
   Expiration and not before, a 401 re-mints the token, a failed refresh keeps
   a still-valid credential and raises once it has expired.

    python3 sampler_selftest.py [-v]
"""
import email.utils
import hashlib
import hmac
import http.server
import json
import os
import shutil
import signal
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import sampler as S  # noqa: E402

RESULTS = []


def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok), detail))
    print(f"  {'PASS' if ok else 'FAIL'}  {name}" + ("" if ok else f"   -- {detail}"))
    return ok


# ------------------------------------------------------------ 1. vectors --

EX_AK, EX_SK = "AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"


def vectors():
    print("SigV4 published vectors")
    # S3 docs, "Examples: Signature Calculations in AWS Signature Version 4", GET Object.
    h = {"host": "examplebucket.s3.amazonaws.com", "range": "bytes=0-9",
         "x-amz-content-sha256": S.EMPTY_SHA256, "x-amz-date": "20130524T000000Z"}
    creq = S.canonical_request("GET", "/test.txt", S.canonical_query([]), h, S.EMPTY_SHA256)
    expected_creq = ("GET\n/test.txt\n\nhost:examplebucket.s3.amazonaws.com\nrange:bytes=0-9\n"
                     "x-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n"
                     "x-amz-date:20130524T000000Z\n\nhost;range;x-amz-content-sha256;x-amz-date\n"
                     "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
    check("GET Object: canonical request text", creq == expected_creq, repr(creq))
    check("GET Object: canonical request hash",
          hashlib.sha256(creq.encode()).hexdigest() == "7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972")
    GET_OBJ = "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
    sig = S.signature(EX_SK, "20130524T000000Z", "us-east-1", "s3", creq)
    check("GET Object: signature", sig == GET_OBJ, sig)
    auth, sig2 = S.sign("GET", "/test.txt", [], h, EX_AK, EX_SK, "us-east-1", "s3", "20130524T000000Z")
    check("GET Object: through sign()", sig2 == GET_OBJ, sig2)
    check("GET Object: Authorization header",
          auth == ("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, "
                   f"SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature={GET_OBJ}"), auth)

    mutated = creq.replace("bytes=0-9", "bytes=0-8")
    check("mutation: one character of the canonical request -> signature differs",
          len(mutated) == len(creq) and sum(a != b for a, b in zip(mutated, creq)) == 1
          and S.signature(EX_SK, "20130524T000000Z", "us-east-1", "s3", mutated) != GET_OBJ)

    real = S.canonical_request
    try:
        S.canonical_request = lambda *a, **k: real(*a, **k).replace("/test.txt", "/test.txu", 1)
        _, bad = S.sign("GET", "/test.txt", [], h, EX_AK, EX_SK, "us-east-1", "s3", "20130524T000000Z")
    finally:
        S.canonical_request = real
    check("mutation through sign(): canonical_request altered by one byte -> vector fails", bad != GET_OBJ, bad)

    # S3 docs, GET Bucket (List Objects): GET /?max-keys=2&prefix=J
    h = {"host": "examplebucket.s3.amazonaws.com", "x-amz-content-sha256": S.EMPTY_SHA256,
         "x-amz-date": "20130524T000000Z"}
    _, sig = S.sign("GET", "/", [("prefix", "J"), ("max-keys", "2")], h, EX_AK, EX_SK, "us-east-1", "s3",
                    "20130524T000000Z")
    check("List Objects (query string, sorted): signature",
          sig == "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7", sig)

    # aws-sig-v4-test-suite get-vanilla
    h = {"host": "example.amazonaws.com", "x-amz-date": "20150830T123600Z"}
    _, sig = S.sign("GET", "/", [], h, "AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                    "us-east-1", "service", "20150830T123600Z")
    check("get-vanilla: signature", sig == "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31", sig)

    check("uri_encode: key path keeps '/', encodes the rest",
          S.uri_encode("a b/c+d~e.f", encode_slash=False) == "a%20b/c%2Bd~e.f")
    check("canonical_query: '/' encoded in a value",
          S.canonical_query([("prefix", "w/l/.flint/"), ("list-type", "2")]) == "list-type=2&prefix=w%2Fl%2F.flint%2F")


# -------------------------------------------------------------- fake S3 --

class FakeS3:
    def __init__(self, bucket, access, secret, region="us-west-1"):
        self.bucket, self.access, self.secret, self.region = bucket, access, secret, region
        self.objects = {}          # key -> (body, etag)
        self.log = []              # {method, key, list_prefix, status, inm, date}
        self.auth_failures = []
        self.fail_next = {}        # ("GET", key) | ("LIST", prefix) -> [status | "drop", ...]
        self.fail_always = set()   # ("LIST", prefix)
        self.after_list = None     # callable(prefix, page_keys), run after a LIST page is built
        self.page_size = 2
        self.clock = 0
        self.lock = threading.RLock()

    def put(self, key, body):
        if isinstance(body, (dict, list)):
            body = json.dumps(body).encode()
        with self.lock:
            self.objects[key] = (body, '"%s"' % hashlib.md5(body).hexdigest())

    def delete(self, key):
        with self.lock:
            self.objects.pop(key, None)

    def gets(self, key, status=None):
        return [e for e in self.log if e["method"] == "GET" and e["key"] == key and (status is None or e["status"] == status)]

    def start(self):
        fake = self

        class H(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def date_time_string(self, timestamp=None):
                with fake.lock:
                    fake.clock += 1
                    self._date = email.utils.formatdate(1789300000 + fake.clock, usegmt=True)
                return self._date

            def verify(self, raw_path, raw_query):
                auth = self.headers.get("Authorization", "")
                try:
                    algo, rest = auth.split(" ", 1)
                    parts = dict(p.strip().split("=", 1) for p in rest.split(","))
                    ak, date, region, service, term = parts["Credential"].split("/")
                    signed = parts["SignedHeaders"].split(";")
                except Exception:
                    return f"malformed Authorization {auth!r}"
                if algo != S.ALGO or ak != fake.access or region != fake.region or service != "s3":
                    return f"bad credential scope {parts.get('Credential')}"
                for need in ("host", "x-amz-date", "x-amz-content-sha256"):
                    if need not in signed:
                        return f"{need} not signed"
                if self.headers.get("x-amz-security-token") is not None and "x-amz-security-token" not in signed:
                    return "session token sent but not signed"
                # the canonical request rebuilt HERE from what arrived on the wire
                q = urllib.parse.parse_qsl(raw_query, keep_blank_values=True)
                cq = "&".join(f"{k}={v}" for k, v in sorted(
                    (urllib.parse.quote(k, safe="-_.~"), urllib.parse.quote(v, safe="-_.~")) for k, v in q))
                hdrs = "".join(f"{n}:{' '.join(self.headers.get(n, '').split())}\n" for n in signed)
                creq = "\n".join([self.command, raw_path, cq, hdrs, ";".join(signed),
                                  self.headers.get("x-amz-content-sha256", "")])
                want = S.signature(fake.secret, self.headers.get("x-amz-date", ""), region, "s3", creq)
                if not hmac.compare_digest(want, parts.get("Signature", "")):
                    return "SignatureDoesNotMatch"
                return None

            def reply(self, status, body=b"", headers=None):
                self.send_response(status)
                for k, v in (headers or {}).items():
                    self.send_header(k, v)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                if body and self.command != "HEAD":
                    self.wfile.write(body)
                return getattr(self, "_date", None)

            def do_GET(self):
                raw_path, _, raw_query = self.path.partition("?")
                parts = raw_path.split("/", 2)
                if len(parts) < 2 or parts[1] != fake.bucket:
                    return self.reply(404, b"<Error><Code>NoSuchBucket</Code></Error>")
                key = urllib.parse.unquote(parts[2]) if len(parts) > 2 else None
                entry = {"method": "GET" if key else "LIST", "key": key, "inm": self.headers.get("If-None-Match")}
                err = fake.verify_hook(self, raw_path, raw_query)
                if err:
                    fake.auth_failures.append((self.path, err))
                    entry["status"] = 403
                    entry["date"] = self.reply(403, f"<Error><Code>{err}</Code></Error>".encode())
                    fake.log.append(entry)
                    return
                q = dict(urllib.parse.parse_qsl(raw_query, keep_blank_values=True))
                fkey = ("GET", key) if key else ("LIST", q.get("prefix", ""))
                entry["list_prefix"] = q.get("prefix") if not key else None
                entry["token"] = q.get("continuation-token")
                with fake.lock:
                    queue = fake.fail_next.get(fkey)
                    injected = queue.pop(0) if queue else (500 if fkey in fake.fail_always else None)
                if injected == "drop":
                    entry["status"] = "drop"
                    fake.log.append(entry)
                    self.close_connection = True
                    return
                if injected:
                    entry["status"] = injected
                    entry["date"] = self.reply(injected, b"<Error><Code>InternalError</Code></Error>")
                    fake.log.append(entry)
                    return
                if key:
                    with fake.lock:
                        obj = fake.objects.get(key)
                    if obj is None:
                        entry["status"] = 404
                        entry["date"] = self.reply(404, b"<Error><Code>NoSuchKey</Code></Error>")
                    elif self.headers.get("If-None-Match") == obj[1]:
                        entry["status"] = 304
                        self.send_response(304)
                        self.send_header("ETag", obj[1])
                        self.end_headers()
                        entry["date"] = self._date
                    else:
                        entry["status"], entry["etag"] = 200, obj[1]
                        entry["date"] = self.reply(200, obj[0], {"ETag": obj[1], "Content-Type": "application/json"})
                    fake.log.append(entry)
                    return
                prefix = q.get("prefix", "")
                with fake.lock:
                    keys = sorted(k for k in fake.objects if k.startswith(prefix))
                    start = int(q["continuation-token"].split("-")[1]) if "continuation-token" in q else 0
                    page = keys[start:start + fake.page_size]
                    more = start + fake.page_size < len(keys)
                    xs = ['<?xml version="1.0" encoding="UTF-8"?>',
                          '<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">',
                          f"<Name>{fake.bucket}</Name><Prefix>{prefix}</Prefix><KeyCount>{len(page)}</KeyCount>",
                          f"<IsTruncated>{'true' if more else 'false'}</IsTruncated>"]
                    for k in page:
                        body, etag = fake.objects[k]
                        xs.append(f"<Contents><Key>{k}</Key><LastModified>2026-09-13T10:00:00.000Z</LastModified>"
                                  f"<ETag>{etag.replace(chr(34), '&quot;')}</ETag><Size>{len(body)}</Size>"
                                  "<StorageClass>STANDARD</StorageClass></Contents>")
                    if more:
                        xs.append(f"<NextContinuationToken>tok-{start + fake.page_size}</NextContinuationToken>")
                    xs.append("</ListBucketResult>")
                if fake.after_list:
                    fake.after_list(prefix, page)
                entry["status"] = 200
                entry["date"] = self.reply(200, "".join(xs).encode(), {"Content-Type": "application/xml"})
                fake.log.append(entry)

        self.verify_hook = lambda handler, p, q: handler.verify(p, q)

        class Srv(socketserver.ThreadingMixIn, http.server.HTTPServer):
            daemon_threads = True
            allow_reuse_address = True

        self.server = Srv(("127.0.0.1", 0), H)
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return self

    def stop(self):
        self.server.shutdown()
        self.server.server_close()


# ------------------------------------------------------ 2. sampler ticks --

AK, SK, TOK = "AKIDTEST", "secret/test+key", "session-token-xyz"


def read_index(out, name="index.jsonl"):
    p = os.path.join(out, name)
    if not os.path.exists(p):
        return []
    out = []
    with open(p) as f:
        for l in f:
            try:
                out.append(json.loads(l))
            except ValueError:
                pass  # a line being appended right now
    return out


def files(out, sub):
    d = os.path.join(out, sub)
    return sorted(os.listdir(d)) if os.path.isdir(d) else []


def ticks(base):
    print("sampler ticks against the fake store (signature verified per request)")
    fake = FakeS3("drill", AK, SK).start()
    os.environ.update({"AWS_ACCESS_KEY_ID": AK, "AWS_SECRET_ACCESS_KEY": SK, "AWS_SESSION_TOKEN": TOK})
    creds = S.CredProvider("http://127.0.0.1:9")  # env credentials; IMDS never touched
    out = os.path.join(base, "hist-ticks")
    s3 = S.S3("drill", "us-west-1", creds, endpoint=f"http://127.0.0.1:{fake.port}")
    smp = S.Sampler(s3, "writers/t1", out)
    L = "writers/t1/.flint/lean"
    gen = lambda n: f"{L}/manifests/{n:020d}-uuid{n}"

    # absent workspace: recorded once
    check("tick on an absent workspace succeeds", smp.safe_tick())
    idx = read_index(out)
    check("absence indexed once per pointer (etag null, status 404)",
          sorted(l["name"] for l in idx) == ["current", "epoch", "inbox"]
          and all(l["etag"] is None and l["status"] == 404 for l in idx), idx)
    smp.safe_tick()
    check("a second absent tick adds no index line", len(read_index(out)) == 3)

    # generation 1, step-one layout
    fake.put(gen(1), {"seq": 1, "entries": {"a.txt": {}}})
    fake.put(f"{L}/current", {"seq": 1, "entries_key": gen(1), "entries_seq": 1, "epoch": 1})
    fake.put(f"{L}/epoch", {"epoch": 1, "holder": "h1"})
    fake.put(f"{L}/inbox", {"entries": []})
    fake.put("writers/t1/files/a.txt", b"a")  # a workspace object: never sampled per tick
    n_idx = len(read_index(out))
    check("tick 1", smp.safe_tick())
    check("tick 1 saved current, epoch, inbox bodies",
          [len(files(out, n)) for n in ("current", "epoch", "inbox")] == [1, 1, 1])
    cur_file = files(out, "current")[0]
    with open(os.path.join(out, "current", cur_file), "rb") as f:
        check("saved body is byte-equal to the store's", f.read() == fake.objects[f"{L}/current"][0])
    check("body name is <ts_ms>-<etag sanitized>.json",
          cur_file.split("-", 1)[1] == fake.objects[f"{L}/current"][1].strip('"') + ".json", cur_file)
    check("generation 1 saved exactly once though cited AND listed",
          files(out, "manifests") == [os.path.basename(gen(1)) + ".json"] and len(fake.gets(gen(1), 200)) == 1)
    check("the workspace's files are never fetched by a tick", not fake.gets("writers/t1/files/a.txt"))

    # no change: 304s, nothing saved
    before = {n: files(out, n) for n in ("current", "epoch", "inbox", "manifests", "chunks")}
    n_idx = len(read_index(out))
    n_log = len(fake.log)
    smp.safe_tick()
    new = fake.log[n_log:]
    cond = [e for e in new if e["method"] == "GET"]
    check("unchanged tick: every pointer GET carried If-None-Match = the saved etag and got 304",
          len(cond) == 3 and all(e["status"] == 304 and e["inm"] == fake.objects[e["key"]][1] for e in cond), cond)
    check("unchanged tick: no file saved, no index line",
          before == {n: files(out, n) for n in before} and len(read_index(out)) == n_idx)

    # pointer moves to gen 2; gen 3 written (listed, not cited)
    fake.put(gen(2), {"seq": 2})
    fake.put(gen(3), {"seq": 3})
    fake.put(f"{L}/current", {"seq": 2, "entries_key": gen(2), "entries_seq": 2, "epoch": 1})
    smp.safe_tick()
    check("pointer change saves the new pointer body", len(files(out, "current")) == 2)
    check("the cited generation and the LISTED-only generation are both saved",
          {os.path.basename(gen(2)) + ".json", os.path.basename(gen(3)) + ".json"} <= set(files(out, "manifests")))

    # gen 3 reaped (it was listed at a tick: kept); gen 4 appears and disappears between ticks; pointer skips to 5
    fake.delete(gen(3))
    fake.put(gen(4), {"seq": 4})
    fake.delete(gen(4))
    fake.put(gen(5), {"seq": 5})
    fake.put(f"{L}/current", {"seq": 5, "entries_key": gen(5), "entries_seq": 5, "epoch": 1})
    smp.safe_tick()
    m = set(files(out, "manifests"))
    check("a generation reaped after it was listed at a tick stays saved", os.path.basename(gen(3)) + ".json" in m)
    check("a generation that came and went between ticks is not (cannot be) saved",
          os.path.basename(gen(4)) + ".json" not in m and not fake.gets(gen(4)))
    check("the pointer's skip target is saved", os.path.basename(gen(5)) + ".json" in m)

    # a generation listed at the tick but reaped before its GET (the LIST/GET race)
    fake.put(gen(6), {"seq": 6})
    fake.after_list = lambda prefix, page: fake.delete(gen(6)) if gen(6) in page else None
    ok = smp.safe_tick()
    fake.after_list = None
    miss = [l for l in read_index(out) if l["key"] == gen(6)]
    errs = read_index(out, "errors.jsonl")
    check("LIST/GET race: the tick still succeeds, the miss is indexed (404) and counted in errors.jsonl",
          ok and len(miss) == 1 and miss[0]["status"] == 404 and any(e["key"] == gen(6) for e in errs), (miss, errs))

    # chunked layout
    fake.put(f"{L}/chunks/c1", {"entries": {"a": {}}})
    fake.put(f"{L}/chunks/c2", {"entries": {"b": {}}})
    fake.put(f"{L}/current", {"seq": 7, "chunks": [{"addr": "c1", "first": "a", "n": 1},
                                                   {"addr": "c2", "first": "b", "n": 1}], "epoch": 2})
    smp.safe_tick()
    check("chunked pointer: every cited chunk saved", files(out, "chunks") == ["c1.json", "c2.json"])
    fake.put(f"{L}/chunks/c3", {"entries": {"c": {}}})
    fake.put(f"{L}/current", {"seq": 8, "chunks": [{"addr": "c1", "first": "a", "n": 1},
                                                   {"addr": "c3", "first": "c", "n": 1}], "epoch": 2})
    smp.safe_tick()
    check("a chunk shared by the next pointer is not fetched again",
          files(out, "chunks") == ["c1.json", "c2.json", "c3.json"] and len(fake.gets(f"{L}/chunks/c1")) == 1)

    # 503 on a pointer: logged, backed off by the caller, retried
    fake.put(f"{L}/epoch", {"epoch": 2, "holder": "h2"})
    fake.fail_next[("GET", f"{L}/epoch")] = [503]
    ok = smp.safe_tick()
    last = (read_index(out, "errors.jsonl") or [{"status": None, "consecutive": None, "error": None, "key": None}])[-1]
    check("503: the tick fails, errors.jsonl records status 503 with consecutive=1",
          not ok and last["status"] == 503 and last["consecutive"] == 1 and smp.consecutive_errors == 1, last)
    ok = smp.safe_tick()
    check("after the 503 the next tick saves the changed epoch and clears the error streak",
          ok and len(files(out, "epoch")) == 2 and smp.consecutive_errors == 0)

    # a cited chunk whose GET fails: the pointer is a 304 next tick, the chunk is still fetched
    fake.put(f"{L}/chunks/c4", {"entries": {"d": {}}})
    fake.put(f"{L}/current", {"seq": 9, "chunks": [{"addr": "c4", "first": "d", "n": 1}], "epoch": 2})
    fake.fail_next[("GET", f"{L}/chunks/c4")] = [500]
    ok1 = smp.safe_tick()
    got_after_fail = "c4.json" in files(out, "chunks")
    ok2 = smp.safe_tick()
    cur_gets = fake.gets(f"{L}/current")
    check("a failed cited-chunk GET is retried on the next tick though the pointer then answers 304",
          not ok1 and not got_after_fail and ok2 and "c4.json" in files(out, "chunks")
          and cur_gets[-1]["status"] == 304, (ok1, got_after_fail, ok2))

    # a dropped connection on the LIST
    fake.fail_next[("LIST", f"{L}/manifests/")] = ["drop"]
    ok1 = smp.safe_tick()
    ok2 = smp.safe_tick()
    last = read_index(out, "errors.jsonl")
    check("a dropped connection is a counted error, not an exit; the next tick succeeds",
          not ok1 and ok2 and any("Disconnected" in (e["error"] or "") or "Connection" in (e["error"] or "")
                                  for e in last), last[-1:])

    # index lines: fields and the store's Date header
    idx = read_index(out)
    need = {"ts_ms", "name", "etag", "key", "bytes", "http_date"}
    check("every index line carries ts_ms, name, etag, key, bytes, http_date",
          all(need <= set(l) for l in idx), [l for l in idx if not need <= set(l)][:2])
    mism = []
    for l in idx:
        if l["etag"] is None:
            continue
        served = [e for e in fake.log if e["key"] == l["key"] and e["status"] == 200 and e.get("etag") == l["etag"]]
        if not served or served[-1]["date"] != l["http_date"]:
            mism.append((l, served[-1:] if served else None))
    check("http_date is the Date header the store sent with that 200", not mism, mism[:2])
    check("pagination was exercised (a LIST page requested with a continuation-token, verified)",
          any(e["method"] == "LIST" and e.get("token") and e["status"] == 200 for e in fake.log))
    check("the fake store verified every signed request with no failure", not fake.auth_failures, fake.auth_failures[:3])

    # control: the store's signature check can fail
    bad = S.S3("drill", "us-west-1", _StaticCreds(AK, "wrong-secret", TOK), endpoint=f"http://127.0.0.1:{fake.port}")
    r = bad.request("GET", f"{L}/current")
    check("control: a wrong secret is refused 403 by the fake store's verification",
          r.status == 403 and fake.auth_failures and fake.auth_failures[-1][1] == "SignatureDoesNotMatch",
          (r.status, fake.auth_failures[-1:]))
    # control: an unsigned-but-sent header changes nothing; a SIGNED host that differs from the sent one fails

    def lying_host(self, method, key=None, params=(), extra=None):
        real_host = self.host
        self.host = "127.0.0.1:1"  # sign a different host...
        try:
            path = self.base_path + ("/" + S.uri_encode(key, encode_slash=False) if key else "")
            c = self.creds.get()
            amz = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
            signed = {"host": self.host, "x-amz-date": amz, "x-amz-content-sha256": S.EMPTY_SHA256}
            auth, _ = S.sign(method, path, list(params), signed, c.access_key, c.secret, self.region, "s3", amz)
            import urllib.request as ur
            req = ur.Request(f"http://{real_host}{path}", headers={**signed, "Host": real_host, "Authorization": auth})
            try:
                with ur.urlopen(req, timeout=5) as resp:
                    return resp.status
            except ur.HTTPError as e:
                return e.code
        finally:
            self.host = real_host

    n_fail = len(fake.auth_failures)
    status = lying_host(s3, "GET", f"{L}/current")
    check("control: a signed Host that differs from the Host sent is refused (the check reads the wire)",
          status == 403 and len(fake.auth_failures) == n_fail + 1, status)
    fake.stop()


class _StaticCreds:
    def __init__(self, ak, sk, tok):
        self.c = S.Creds(ak, sk, tok, None, "env")

    def get(self, force=False):
        return self.c


# ----------------------------------------------------------- 3. the CLI --

def wait_for(pred, timeout=15.0, step=0.02):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if pred():
            return True
        time.sleep(step)
    return pred()


def cli(base):
    print("the CLI end to end (subprocess)")
    fake = FakeS3("drill", AK, SK).start()
    env = dict(os.environ, AWS_ACCESS_KEY_ID=AK, AWS_SECRET_ACCESS_KEY=SK, AWS_SESSION_TOKEN=TOK)
    L = "writers/t2/.flint/lean"
    fake.put(f"{L}/current", {"seq": 1, "chunks": [{"addr": "k1", "first": "", "n": 1}], "epoch": 1})
    fake.put(f"{L}/chunks/k1", {"entries": {}})
    fake.put(f"{L}/epoch", {"epoch": 1})
    fake.put(f"{L}/inbox", {"entries": []})
    for i in range(5):
        fake.put(f"writers/t2/files/p{i}.txt", f"body {i}".encode())
    fake.put("writers/t2x/other", b"not under the prefix")

    def run(out, extra, prefix="writers/t2"):
        cmd = [sys.executable, os.path.join(HERE, "sampler.py"), "--bucket", "drill", "--prefix", prefix,
               "--out", out, "--interval-ms", "50", "--endpoint", f"http://127.0.0.1:{fake.port}",
               "--allow-rootfs"] + extra
        return subprocess.Popen(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def expected_listing(prefix):
        return sorted((k, fake.objects[k][1], len(fake.objects[k][0])) for k in fake.objects if k.startswith(prefix + "/"))

    # --until-file
    out = os.path.join(base, "hist-until")
    until = os.path.join(base, "until")
    p = run(out, ["--until-file", until])
    got_first = wait_for(lambda: len(read_index(out)) >= 4)
    fake.put(f"{L}/current", {"seq": 2, "chunks": [{"addr": "k1", "first": "", "n": 1}], "epoch": 1})
    got_change = wait_for(lambda: sum(1 for l in read_index(out) if l["name"] == "current") >= 2)
    open(until, "w").close()
    try:
        rc = p.wait(timeout=20)
    except subprocess.TimeoutExpired:
        p.kill()
        rc = None
    err = p.stderr.read()
    check("--until-file: first tick indexed, a later pointer change indexed", got_first and got_change)
    check("--until-file: exit 0", rc == 0, (rc, err[-500:]))
    try:
        with open(os.path.join(out, "listing.json")) as f:
            lst = json.load(f)
    except (OSError, ValueError) as e:
        lst = None
    check("--until-file: listing.json = every object under the prefix (key, etag, size, last_modified)",
          lst is not None and sorted((o["key"], o["etag"], o["size"]) for o in lst) == expected_listing("writers/t2")
          and all(o["last_modified"] for o in lst), lst)
    check("--until-file: run.json written", os.path.exists(os.path.join(out, "run.json")))

    # SIGTERM
    out = os.path.join(base, "hist-term")
    p = run(out, [])
    started = wait_for(lambda: len(read_index(out)) >= 4)
    p.send_signal(signal.SIGTERM)
    try:
        rc = p.wait(timeout=20)
    except subprocess.TimeoutExpired:
        p.kill()
        rc = None
    try:
        with open(os.path.join(out, "listing.json")) as f:
            lst = json.load(f)
    except (OSError, ValueError):
        lst = None
    check("SIGTERM: exit 0 with a full listing.json", started and rc == 0 and lst is not None
          and sorted((o["key"], o["etag"], o["size"]) for o in lst) == expected_listing("writers/t2"), rc)
    with open(os.path.join(out, "run.json")) as f:
        check("SIGTERM: run.json names the signal", json.load(f).get("signal") == signal.SIGTERM)

    # the final LIST keeps failing: exit 3, no listing.json
    L3 = "writers/t3/.flint/lean"
    fake.put(f"{L3}/current", {"seq": 1, "chunks": [], "epoch": 1})
    fake.fail_always.add(("LIST", "writers/t3/"))
    out = os.path.join(base, "hist-listfail")
    until3 = os.path.join(base, "until3")
    open(until3, "w").close()
    p = run(out, ["--until-file", until3, "--final-list-retries", "2", "--final-list-backoff-s", "0.01"],
            prefix="writers/t3")
    try:
        rc = p.wait(timeout=20)
    except subprocess.TimeoutExpired:
        p.kill()
        rc = None
    check("a final LIST that keeps failing: exit 3, listing.error.json, and NO listing.json",
          rc == 3 and os.path.exists(os.path.join(out, "listing.error.json"))
          and not os.path.exists(os.path.join(out, "listing.json")), rc)
    fake.fail_always.discard(("LIST", "writers/t3/"))

    # restart resumes the conditional GETs from index.jsonl
    out = os.path.join(base, "hist-until")
    os.remove(until)
    n_before = len(read_index(out))
    n_log = len(fake.log)
    p = run(out, ["--until-file", until])
    wait_for(lambda: sum(1 for e in fake.log[n_log:] if e["key"] == f"{L}/current") >= 1)
    open(until, "w").close()
    p.wait(timeout=20)
    first = [e for e in fake.log[n_log:] if e["key"] == f"{L}/current"][0]
    check("a restarted sampler conditions its first GET on the last indexed etag (304, nothing re-saved)",
          first["status"] == 304 and len(read_index(out)) == n_before, (first, len(read_index(out)), n_before))

    check("same_device_as_root('/') is True (the rootfs guard's predicate)", S.same_device_as_root("/"))
    check("the CLI runs signed requests the store accepted", not fake.auth_failures, fake.auth_failures[:3])
    fake.stop()


# ------------------------------------------------------------- 4. IMDS --

def imds(base):
    print("IMDSv2 credentials")
    state = {"token_puts": 0, "cred_gets": 0, "role_401": 0, "fail": False, "exp": 0}

    class H(http.server.BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def send(self, code, body):
            b = body.encode()
            self.send_response(code)
            self.send_header("Content-Length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)

        def do_PUT(self):
            if self.path == "/latest/api/token" and self.headers.get("X-aws-ec2-metadata-token-ttl-seconds"):
                state["token_puts"] += 1
                return self.send(200, f"tok{state['token_puts']}")
            self.send(400, "")

        def do_GET(self):
            if state["fail"]:
                return self.send(500, "")
            if not (self.headers.get("X-aws-ec2-metadata-token") or "").startswith("tok"):
                return self.send(401, "")
            if self.path == "/latest/meta-data/iam/security-credentials/":
                if state["role_401"]:
                    state["role_401"] -= 1
                    return self.send(401, "")
                return self.send(200, "TroveSSMInstanceProfile\n")
            if self.path == "/latest/meta-data/iam/security-credentials/TroveSSMInstanceProfile":
                state["cred_gets"] += 1
                exp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(state["exp"]))
                return self.send(200, json.dumps({"Code": "Success", "AccessKeyId": f"ASIA{state['cred_gets']}",
                                                  "SecretAccessKey": "s", "Token": "t", "Expiration": exp}))
            self.send(404, "")

    srv = http.server.HTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    saved = {k: os.environ.pop(k, None) for k in ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN")}
    try:
        now = [1789300000.0]
        state["exp"] = int(now[0]) + 3600
        logged = []
        cp = S.CredProvider(f"http://127.0.0.1:{srv.server_address[1]}", clock=lambda: now[0],
                            log=lambda **kw: logged.append(kw))
        c = cp.get()
        check("first get() fetches token, role and credentials", c.access_key == "ASIA1" and state["token_puts"] == 1
              and c.token == "t" and c.expiration == state["exp"])
        now[0] = state["exp"] - 601
        check("11 minutes before Expiration: no refresh", cp.get().access_key == "ASIA1" and state["cred_gets"] == 1)
        now[0] = state["exp"] - 599
        state["exp"] = int(now[0]) + 3600
        check("inside 10 minutes of Expiration: refreshed", cp.get().access_key == "ASIA2" and cp.refreshes == 2)
        state["role_401"] = 1
        check("an IMDS 401 (expired session token) mints a new token once and succeeds",
              cp.get(force=True).access_key == "ASIA3" and state["token_puts"] == 2)
        state["fail"] = True
        check("a failed refresh keeps the still-valid credential and logs it",
              cp.get(force=True).access_key == "ASIA3" and logged and logged[-1]["op"] == "imds")
        now[0] = state["exp"] + 1
        try:
            cp.get()
            raised = False
        except Exception:
            raised = True
        check("a failed refresh with an EXPIRED credential raises (never signs with it)", raised)
    finally:
        srv.shutdown()
        for k, v in saved.items():
            if v is not None:
                os.environ[k] = v


def main():
    base = tempfile.mkdtemp(prefix="sampler-selftest-")
    try:
        vectors()
        ticks(base)
        cli(base)
        imds(base)
    finally:
        shutil.rmtree(base, ignore_errors=True)
    bad = [r for r in RESULTS if not r[1]]
    print(f"\nsampler selftest: {len(RESULTS) - len(bad)}/{len(RESULTS)} checks passed" + (" — FAILED" if bad else ""))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
