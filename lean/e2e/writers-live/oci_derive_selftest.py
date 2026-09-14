#!/usr/bin/env python3
"""oci_derive_selftest.py — tests for oci_derive.py and verify_image.sh that
run on a Mac with NO Docker.

  python3 oci_derive_selftest.py [--require-network] [--keep] [--scratch DIR]

Sections:
  0 MODEL      the rootfs model (overwrite, whiteout, opaque, lower-only
               whiteouts, type change, symlinked parent, hardlink, ENOTDIR)
               checked against hand-written expectations.
  1 SYNTHETIC  a fake base served by a local registry DOUBLE (bearer-token
               challenge, Accept-gated manifests, blob 307 to a "CDN" path
               that rejects an Authorization header, an index with arm64 +
               amd64 + an attestation manifest). The real CLI derives from it
               through the real pull code; the output is re-read and applied
               by an INDEPENDENT reader/applier in this file (not the tool's
               verifier) and by the tool's verifier, and the two must agree.
               Determinism: same inputs -> same layer, config, manifest and
               archive digests; different content or SOURCE_DATE_EPOCH ->
               different layer digest (so the equality is not vacuous).
  2 CONTROLS   mutations of a GOOD output, each of which must FAIL the
               verifier, and on exactly the named check(s) — every other check
               is kept passing by re-signing digests, so the failure is the
               mutation's and not a side effect:
                 a1 added layer moved first in the manifest only -> diff-id, expect
                 a2 moved first in manifest AND config, all re-signed -> expect ONLY
                 b  one byte flipped in the added layer blob -> blob-digest
                 b2 layer content changed, re-gzipped, digest re-signed,
                    diff_id not -> diff-id, expect
                 c  last diff_id dropped, config re-signed -> diff-id-count ONLY
                 e  (oci) image name without docker.io/ -> image-name ONLY
               in both the OCI and docker formats, plus a no-op re-sign that
               must still PASS (the mutation machinery itself breaks nothing).
               Then derive-side refusals from a lying/corrupt registry: a
               corrupted blob, a manifest body that differs from
               Docker-Content-Digest, a manifest that differs from the digest it
               was requested by (with a consistent header), a base whose
               diff_id is wrong, a symlinked parent (and its twin: the resolved
               path is accepted), an absent platform.
  3 LIVE       library/busybox:1.36 linux/amd64 from Docker Hub, adding
               /drill-marker (a random nonce) and overwriting /etc/group; SKIP
               if there is no network (FAIL with --require-network).
  4 NODE SCRIPT verify_image.sh against a `ctr` DOUBLE, under sh and dash:
               exit codes, ref normalization, symlink refusal, digest and CRI
               gates, and that the mount is always unmounted and removed. The
               double proves the script's control flow only, not containerd.

Scratch goes under --scratch, $OCI_DERIVE_SCRATCH, /private/tmp/claude-503 (if
present) or the system temp dir — never the repo — and is removed at the end
unless --keep. Exit 0 iff no check FAILED.
"""

import argparse
import gzip
import hashlib
import http.server
import io
import json
import os
import posixpath
import re
import secrets
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import traceback
import urllib.error
import urllib.parse
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
TOOL = os.path.join(HERE, "oci_derive.py")
VERIFY_SH = os.path.join(HERE, "verify_image.sh")
REPO_ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
sys.path.insert(0, HERE)
sys.dont_write_bytecode = True  # no __pycache__ in the repo
import oci_derive as od  # noqa: E402

RESULTS = []
TAG = "docker.io/dilipdalton/flint-s3-worker-lean:writers-selftest"
FLINT = "/usr/local/bin/flint-sync"


def record(section, name, status, detail=""):
    RESULTS.append((section, name, status, detail))
    line = f"  {status:<4} [{section}] {name}"
    if detail:
        line += f" — {detail}"
    print(line, flush=True)


class Sec:
    def __init__(self, name):
        self.name = name
        print(f"\n== {name}", flush=True)

    def check(self, name, cond, detail="", note=""):
        """`detail` is printed when the check FAILS; `note` always."""
        record(self.name, name, "PASS" if cond else "FAIL", note if cond else (detail or note))
        return bool(cond)

    def skip(self, name, why):
        record(self.name, name, "SKIP", why)


def sha(b):
    return hashlib.sha256(b).hexdigest()


def dg(b):
    return "sha256:" + sha(b)


def jb(o):
    return json.dumps(o, separators=(",", ":")).encode()


# ------------------------------------------------------------ image builders

def make_layer_raw(entries):
    """entries: (name, kind, payload[, mode]); kind in dir/file/symlink/hardlink."""
    bio = io.BytesIO()
    with tarfile.open(fileobj=bio, mode="w", format=tarfile.PAX_FORMAT) as tf:
        for e in entries:
            name, kind, payload = e[0], e[1], e[2] if len(e) > 2 else None
            ti = tarfile.TarInfo(name)
            ti.mtime = 1700000000
            ti.mode = e[3] if len(e) > 3 else 0o755
            if kind == "dir":
                ti.type = tarfile.DIRTYPE
                tf.addfile(ti)
            elif kind == "file":
                ti.size = len(payload)
                tf.addfile(ti, io.BytesIO(payload))
            elif kind == "symlink":
                ti.type, ti.linkname = tarfile.SYMTYPE, payload
                tf.addfile(ti)
            elif kind == "hardlink":
                ti.type, ti.linkname = tarfile.LNKTYPE, payload
                tf.addfile(ti)
    return bio.getvalue()


def make_layer(entries):
    raw = make_layer_raw(entries)
    return gzip.compress(raw, mtime=0), dg(raw)


def make_image(layers, flavor="oci", arch="amd64", cfg_mutate=None):
    lb = [make_layer(entries) for entries in layers]
    cfg = {"architecture": arch, "os": "linux", "created": "2026-09-01T00:00:00Z",
           "config": {"Env": ["PATH=/usr/local/bin:/usr/bin:/bin"], "Entrypoint": [FLINT], "Cmd": ["run"],
                      "User": "65532:65532", "WorkingDir": "/workspace", "Labels": {"org.example.base": "yes"}},
           "rootfs": {"type": "layers", "diff_ids": [d for _, d in lb]},
           "history": [{"created": "2026-09-01T00:00:00Z", "created_by": f"COPY layer {i}"} for i in range(len(lb))]}
    if cfg_mutate:
        cfg_mutate(cfg)
    cfgb = jb(cfg)
    oci = flavor == "oci"
    man = {"schemaVersion": 2, "mediaType": od.MT_OCI_MANIFEST if oci else od.MT_DOCKER_MANIFEST,
           "config": {"mediaType": od.MT_OCI_CONFIG if oci else od.MT_DOCKER_CONFIG,
                      "digest": dg(cfgb), "size": len(cfgb)},
           "layers": [{"mediaType": od.MT_OCI_LAYER_GZIP if oci else od.MT_DOCKER_LAYER_GZIP,
                       "digest": dg(gz), "size": len(gz)} for gz, _ in lb]}
    manb = jb(man)
    blobs = {dg(cfgb): cfgb}
    blobs.update({dg(gz): gz for gz, _ in lb})
    return {"manifest": manb, "mt": man["mediaType"], "digest": dg(manb), "blobs": blobs, "config": cfg,
            "diff_ids": [d for _, d in lb], "layer_digests": [dg(gz) for gz, _ in lb]}


def make_index(entries, flavor="oci"):
    idx = {"schemaVersion": 2, "mediaType": od.MT_OCI_INDEX if flavor == "oci" else od.MT_DOCKER_LIST,
           "manifests": []}
    for img, plat, ann in entries:
        d = {"mediaType": img["mt"], "digest": img["digest"], "size": len(img["manifest"]), "platform": plat}
        if ann:
            d["annotations"] = ann
        idx["manifests"].append(d)
    b = jb(idx)
    return {"manifest": b, "mt": idx["mediaType"], "digest": dg(b)}


BASE_L1 = [("usr/", "dir"), ("usr/local/", "dir"), ("usr/local/bin/", "dir"),
           ("usr/local/bin/flint-sync", "file", b"OLDEST"), ("usr/bin/", "dir"), ("usr/bin/tool", "file", b"TOOL"),
           ("bin", "symlink", "usr/bin"), ("etc/", "dir"), ("etc/removed", "file", b"gone"),
           ("etc/keep", "file", b"KEEP"), ("opt/", "dir"), ("opt/a", "file", b"A"), ("opt/b", "file", b"B")]
BASE_L2 = [("usr/local/bin/flint-sync", "file", b"OLD"), ("etc/.wh.removed", "file", b""),
           ("opt/.wh..wh..opq", "file", b""), ("opt/c", "file", b"C")]


# --------------------------------------------------------- registry double

class FakeRegistry:
    """A distribution-API DOUBLE with the properties the pull code depends on:
    401 + Bearer challenge until a token is presented; manifests only served
    when the Accept header lists their media type; blobs 307 to a /cdn/ path
    that REJECTS an Authorization header (as S3-backed CDNs do)."""

    def __init__(self):
        self.repos = {}
        self.stats = {}
        self.lock = threading.Lock()

    def bump(self, k):
        with self.lock:
            self.stats[k] = self.stats.get(k, 0) + 1

    def add(self, repo, tags, images, extra_blobs=None, faults=None):
        r = self.repos.setdefault(repo, {"manifests": {}, "blobs": {}, "faults": {}})
        for img in images:
            r["manifests"][img["digest"]] = (img["mt"], img["manifest"])
            r["blobs"].update(img.get("blobs", {}))
        for tag, img in tags.items():
            r["manifests"][tag] = (img["mt"], img["manifest"])
        r["blobs"].update(extra_blobs or {})
        r["faults"].update(faults or {})

    def start(self):
        reg = self

        class H(http.server.BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def send(self, code, body=b"", headers=None):
                self.send_response(code)
                for k, v in (headers or {}).items():
                    self.send_header(k, v)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                try:
                    reg.handle(self)
                except Exception as e:  # a double that dies silently hides everything
                    reg.bump("handler_exception")
                    self.send(500, str(e).encode())

        self.srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.port = self.srv.server_address[1]
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()
        return self

    def stop(self):
        self.srv.shutdown()

    def handle(self, h):
        u = urllib.parse.urlparse(h.path)
        if u.path == "/token":
            q = urllib.parse.parse_qs(u.query)
            if q.get("service", [""])[0] != "fake-registry":
                return h.send(400, b"bad service")
            self.bump("token")
            return h.send(200, jb({"token": "tok:" + q.get("scope", [""])[0]}), {"Content-Type": "application/json"})
        m = re.match(r"^/v2/(.+)/(manifests|blobs)/([^/]+)$", u.path)
        if m:
            repo, kind, ref = m.groups()
            scope = f"repository:{repo}:pull"
            if h.headers.get("Authorization") != f"Bearer tok:{scope}":
                self.bump("unauth_401")
                return h.send(401, b'{"errors":[{"code":"UNAUTHORIZED"}]}', {
                    "WWW-Authenticate": f'Bearer realm="http://127.0.0.1:{self.port}/token",'
                                        f'service="fake-registry",scope="{scope}"'})
            r = self.repos.get(repo)
            if r is None:
                return h.send(404, b"NAME_UNKNOWN")
            if kind == "manifests":
                if ref not in r["manifests"]:
                    return h.send(404, b"MANIFEST_UNKNOWN")
                mt, body = r["manifests"][ref]
                accept = [a.strip() for a in (h.headers.get("Accept") or "").split(",")]
                if mt not in accept:
                    self.bump("accept_miss")
                    return h.send(404, b"MANIFEST_UNKNOWN (not acceptable)")
                self.bump("manifest_get")
                header = dg(body)
                lie = r["faults"].get("lie_manifest")
                if lie and lie["ref"] == ref:
                    body = lie["body"]
                    if lie["header"] == "consistent":
                        header = dg(body)
                    self.bump("lie_served")
                return h.send(200, body, {"Content-Type": mt, "Docker-Content-Digest": header})
            if ref not in r["blobs"]:
                return h.send(404, b"BLOB_UNKNOWN")
            self.bump("blob_redirect")
            return h.send(307, b"", {"Location": f"http://127.0.0.1:{self.port}/cdn/{repo}/{ref}"})
        m = re.match(r"^/cdn/(.+)/(sha256:[0-9a-f]{64})$", u.path)
        if m:
            if h.headers.get("Authorization"):
                self.bump("cdn_auth_rejected")
                return h.send(400, b"InvalidArgument: Only one auth mechanism allowed")
            repo, d = m.groups()
            r = self.repos[repo]
            body = r["blobs"][d]
            if r["faults"].get("corrupt_blob") == d:
                body = bytearray(body)
                body[len(body) // 2] ^= 0x01
                body = bytes(body)
                self.bump("corrupt_served")
            self.bump("cdn_get")
            return h.send(200, body, {"Content-Type": "application/octet-stream"})
        return h.send(404, b"not found")


# ---------------------------------------------- independent reader / applier

def read_archive_independent(path):
    """Re-read an archive WITHOUT oci_derive: hash every referenced blob,
    gunzip every layer, compare against the config. -> dict."""
    with tarfile.open(path) as t:
        files = {posixpath.normpath(m.name): t.extractfile(m).read() for m in t.getmembers() if m.isfile()}
    problems, out = [], {"files": files}
    if "oci-layout" in files:
        out["format"] = "oci"
        idx = json.loads(files["index.json"])
        d = idx["manifests"][0]
        out["index_desc"] = d

        def blob(desc, what):
            b = files.get("blobs/sha256/" + desc["digest"].split(":", 1)[1])
            if b is None:
                problems.append(f"{what} missing")
            elif dg(b) != desc["digest"] or len(b) != desc["size"]:
                problems.append(f"{what} digest/size mismatch")
            return b

        man = json.loads(blob(d, "manifest"))
        cfg = json.loads(blob(man["config"], "config"))
        layers = [blob(ld, f"layer {i}") for i, ld in enumerate(man["layers"])]
        out.update(manifest=man, name=(d.get("annotations") or {}).get(od.ANN_CONTAINERD_NAME))
    else:
        out["format"] = "docker"
        mj = json.loads(files["manifest.json"])[0]

        def named(n, what):
            b = files.get(n)
            mm = re.match(r"^blobs/sha256/([0-9a-f]{64})$", n)
            if b is None:
                problems.append(f"{what} missing")
            elif mm and sha(b) != mm.group(1):
                problems.append(f"{what} name/digest mismatch")
            return b

        cfg = json.loads(named(mj["Config"], "config"))
        layers = [named(n, f"layer {i}") for i, n in enumerate(mj["Layers"])]
        out.update(manifest_json=mj, name=mj["RepoTags"][0])
    raw = [gzip.decompress(b) if b[:2] == b"\x1f\x8b" else b for b in layers]
    diff_ids = cfg["rootfs"]["diff_ids"]
    if len(diff_ids) != len(raw):
        problems.append(f"{len(diff_ids)} diff_ids for {len(raw)} layers")
    for i, r in enumerate(raw):
        if i < len(diff_ids) and dg(r) != diff_ids[i]:
            problems.append(f"layer {i} diff_id mismatch")
    out.update(config=cfg, layers=layers, raw_layers=raw, problems=problems)
    return out


def apply_simple(raw_layers):
    """path -> bytes (file) | 'DIR' | ('SYMLINK', target). Overwrites and
    whiteouts (lower layers only, incl. opaque), hardlinks as copies. It does
    not resolve symlinked parents; the images it is used on never write
    through one (the tool refuses to)."""
    fs = {"/": "DIR"}

    def rm_tree(p):
        for k in [k for k in fs if k == p or k.startswith(p.rstrip("/") + "/")]:
            if k != "/":
                del fs[k]

    for raw in raw_layers:
        t = tarfile.open(fileobj=io.BytesIO(raw))
        members = t.getmembers()
        for m in members:
            p = posixpath.normpath("/" + m.name)
            base, d = posixpath.basename(p), posixpath.dirname(p)
            if base == ".wh..wh..opq":
                for k in [k for k in fs if k.startswith(d.rstrip("/") + "/")]:
                    del fs[k]
            elif base.startswith(".wh."):
                rm_tree(posixpath.join(d, base[4:]))
        for m in members:
            p = posixpath.normpath("/" + m.name)
            if p == "/" or posixpath.basename(p).startswith(".wh."):
                continue
            if m.isdir():
                if fs.get(p) != "DIR":
                    rm_tree(p)
                fs[p] = "DIR"
                continue
            rm_tree(p)
            if m.isfile():
                fs[p] = t.extractfile(m).read()
            elif m.issym():
                fs[p] = ("SYMLINK", m.linkname)
            elif m.islnk():
                fs[p] = fs[posixpath.normpath("/" + m.linkname)]
    return fs


# ----------------------------------------------------------------- mutations

class Mut:
    """Edit an archive in memory, re-signing whatever the mutation does not
    mean to break."""

    def __init__(self, path):
        with tarfile.open(path) as t:
            self.members = [[m.name, t.extractfile(m).read() if m.isfile() else None] for m in t.getmembers()]
        self.fmt = "oci" if self.get("oci-layout") is not None else "docker"

    def get(self, name):
        for n, b in self.members:
            if posixpath.normpath(n) == name:
                return b
        return None

    def put(self, name, data):
        for m in self.members:
            if posixpath.normpath(m[0]) == name:
                m[1] = data
                return
        self.members.append([name, data])

    def put_blob(self, data):
        self.put("blobs/sha256/" + sha(data), data)
        return {"digest": dg(data), "size": len(data)}

    def blob(self, digest):
        return self.get("blobs/sha256/" + digest.split(":", 1)[1])

    def save(self, path):
        with open(path, "wb") as raw, tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as tf:
            for n, b in self.members:
                ti = tarfile.TarInfo(n)
                if b is None:
                    ti.type = tarfile.DIRTYPE
                    tf.addfile(ti)
                else:
                    ti.size = len(b)
                    tf.addfile(ti, io.BytesIO(b))
        return path

    # oci
    def index(self):
        return json.loads(self.get("index.json"))

    def manifest(self):
        return json.loads(self.blob(self.index()["manifests"][0]["digest"]))

    def write_manifest(self, man):
        desc = self.put_blob(jb(man))
        idx = self.index()
        idx["manifests"][0].update(desc)
        self.put("index.json", jb(idx))

    # docker
    def mj(self):
        return json.loads(self.get("manifest.json"))

    def write_mj(self, mj):
        self.put("manifest.json", jb(mj))

    # both
    def config(self):
        if self.fmt == "oci":
            return json.loads(self.blob(self.manifest()["config"]["digest"]))
        return json.loads(self.get(self.mj()[0]["Config"]))

    def write_config(self, cfg):
        desc = self.put_blob(jb(cfg))
        if self.fmt == "oci":
            man = self.manifest()
            man["config"].update(desc)
            self.write_manifest(man)
        else:
            mj = self.mj()
            mj[0]["Config"] = "blobs/sha256/" + desc["digest"].split(":", 1)[1]
            self.write_mj(mj)

    def layer_list(self):
        return self.manifest()["layers"] if self.fmt == "oci" else self.mj()[0]["Layers"]

    def write_layer_list(self, layers):
        if self.fmt == "oci":
            man = self.manifest()
            man["layers"] = layers
            self.write_manifest(man)
        else:
            mj = self.mj()
            mj[0]["Layers"] = layers
            self.write_mj(mj)

    def top_layer_name(self):
        top = self.layer_list()[-1]
        return ("blobs/sha256/" + top["digest"].split(":", 1)[1]) if self.fmt == "oci" else top


def m_noop(m):
    m.write_config(m.config())


def m_a1(m):
    ls = m.layer_list()
    m.write_layer_list([ls[-1]] + ls[:-1])


def m_a2(m):
    cfg = m.config()
    d = cfg["rootfs"]["diff_ids"]
    cfg["rootfs"]["diff_ids"] = [d[-1]] + d[:-1]
    m.write_config(cfg)
    ls = m.layer_list()
    m.write_layer_list([ls[-1]] + ls[:-1])


def m_b(m):
    name = m.top_layer_name()
    b = bytearray(m.get(name))
    b[len(b) // 2] ^= 0x01
    m.put(name, bytes(b))


def m_b2(m, old=b"NEW", new=b"NEX"):
    name = m.top_layer_name()
    raw = gzip.decompress(m.get(name))
    assert raw.count(old) == 1, "mutation b2 needs exactly one occurrence"
    gz = gzip.compress(raw.replace(old, new), mtime=0)
    desc = m.put_blob(gz)
    ls = m.layer_list()
    if m.fmt == "oci":
        ls[-1] = {**ls[-1], **desc}
    else:
        ls[-1] = "blobs/sha256/" + desc["digest"].split(":", 1)[1]
    m.write_layer_list(ls)


def m_c(m):
    cfg = m.config()
    cfg["rootfs"]["diff_ids"].pop()
    m.write_config(cfg)


def m_f(m):
    """Replace the top layer with one whose `bin/` DIRECTORY entry replaces the
    base's `bin -> usr/bin` symlink, still carrying flint-sync NEW, with the
    layer digest AND diff_id re-signed: only --strict-top-layer can see it."""
    raw = make_layer_raw([("bin/", "dir"), ("usr/", "dir"), ("usr/local/", "dir"), ("usr/local/bin/", "dir"),
                          ("usr/local/bin/flint-sync", "file", b"NEW")])
    desc = m.put_blob(gzip.compress(raw, mtime=0))
    cfg = m.config()
    cfg["rootfs"]["diff_ids"][-1] = dg(raw)
    m.write_config(cfg)
    ls = m.layer_list()
    ls[-1] = {**ls[-1], **desc} if m.fmt == "oci" else "blobs/sha256/" + desc["digest"].split(":", 1)[1]
    m.write_layer_list(ls)


def m_e(m):
    idx = m.index()
    idx["manifests"][0]["annotations"][od.ANN_CONTAINERD_NAME] = TAG.removeprefix("docker.io/")
    m.put("index.json", jb(idx))


# ------------------------------------------------------------------ helpers

def run_tool(args, env_extra=None):
    env = dict(os.environ)
    env.pop("SOURCE_DATE_EPOCH", None)
    env.update(env_extra or {})
    p = subprocess.run([sys.executable, TOOL] + args, capture_output=True, text=True, env=env, timeout=900)
    summary = None
    if p.returncode == 0:
        try:
            summary = json.loads(p.stdout)
        except ValueError:
            pass
    return p.returncode, summary, p.stderr


def new_layer_members(arch):
    top = arch["raw_layers"][-1]
    return {m.name: m for m in tarfile.open(fileobj=io.BytesIO(top)).getmembers()}


# ================================================================= sections

def section_model():
    s = Sec("0 MODEL")

    def fs_of(*layers):
        fs, evs = od.Rootfs(), []
        for i, entries in enumerate(layers):
            gz, diff = make_layer(entries)
            _, got, ev = fs.apply_blob(io.BytesIO(gz), i)
            assert got == diff, (got, diff)
            evs.append(ev)
        return fs, evs

    def content(fs, p):
        e, _ = fs.lookup(p)
        return e.content if e is not None and e.kind == "file" else None

    fs, _ = fs_of([("a", "file", b"1")], [("a", "file", b"2")])
    s.check("overwrite: the upper layer's bytes win", content(fs, "/a") == b"2" and fs.lookup("/a")[0].layer == 1)
    fs, _ = fs_of([("d/", "dir"), ("d/x", "file", b"1"), ("d/y", "file", b"y")], [("d/.wh.x", "file", b"")])
    s.check("whiteout removes the lower entry, keeps its siblings",
            fs.lookup("/d/x")[0] is None and content(fs, "/d/y") == b"y")
    fs, _ = fs_of([("d/x", "file", b"1")], [("d/x", "file", b"2"), ("d/.wh.x", "file", b"")])
    s.check("a whiteout hides LOWER layers only (same-layer entry survives)", content(fs, "/d/x") == b"2")
    fs, _ = fs_of([("o/a", "file", b"A"), ("o/sub/b", "file", b"B")],
                  [("o/.wh..wh..opq", "file", b""), ("o/c", "file", b"C")])
    s.check("opaque whiteout hides the lower directory contents",
            fs.lookup("/o/a")[0] is None and fs.lookup("/o/sub/b")[0] is None and content(fs, "/o/c") == b"C")
    fs, evs = fs_of([("t/", "dir"), ("t/k", "file", b"k")], [("t", "file", b"now a file")])
    s.check("file over a directory removes the subtree and reports a type change",
            content(fs, "/t") == b"now a file" and fs.lookup("/t/k")[0] is None
            and any(e["event"] == "type-change" and e["removed"] == 2 for e in evs[1]))
    fs, evs = fs_of([("usr/bin/", "dir"), ("bin", "symlink", "usr/bin")], [("bin/tool", "file", b"T")])
    s.check("an entry under a symlinked parent lands at the resolved path",
            content(fs, "/usr/bin/tool") == b"T" and fs.lookup("/bin/tool")[1].startswith("/bin is a symlink")
            and any(e["event"] == "parent-via-symlink" for e in evs[1]))
    fs, _ = fs_of([("bin/busybox", "file", b"BB"), ("bin/sh", "hardlink", "bin/busybox")],
                  [("bin/sh", "file", b"X")])
    s.check("hardlink copies its target; overwriting one name leaves the other",
            content(fs, "/bin/busybox") == b"BB" and content(fs, "/bin/sh") == b"X")
    try:
        fs_of([("f", "file", b"1")], [("f/x", "file", b"2")])
        s.check("an entry under a regular file is ENOTDIR", False, "applied without error")
    except od.ApplyError as e:
        s.check("an entry under a regular file is ENOTDIR", "not a directory" in str(e), str(e))
    s.check("independent applier agrees on the whiteout/opaque fixture",
            apply_simple([make_layer_raw(BASE_L1), make_layer_raw(BASE_L2)]).get("/etc/removed") is None
            and apply_simple([make_layer_raw(BASE_L1), make_layer_raw(BASE_L2)]).get("/opt/c") == b"C")


def section_synthetic(scratch, reg):
    s = Sec("1 SYNTHETIC")
    amd = make_image([BASE_L1, BASE_L2])
    arm = make_image([[("usr/local/bin/flint-sync", "file", b"ARM")]], arch="arm64")
    att_cfg = jb({"architecture": "unknown", "os": "unknown", "rootfs": {"type": "layers", "diff_ids": []}})
    att_man = jb({"schemaVersion": 2, "mediaType": od.MT_OCI_MANIFEST,
                  "config": {"mediaType": od.MT_OCI_CONFIG, "digest": dg(att_cfg), "size": len(att_cfg)},
                  "layers": []})
    att = {"manifest": att_man, "mt": od.MT_OCI_MANIFEST, "digest": dg(att_man), "blobs": {dg(att_cfg): att_cfg}}
    idx = make_index([(arm, {"architecture": "arm64", "os": "linux"}, None),
                      (amd, {"architecture": "amd64", "os": "linux"}, None),
                      (att, {"architecture": "unknown", "os": "unknown"},
                       {"vnd.docker.reference.type": "attestation-manifest",
                        "vnd.docker.reference.digest": amd["digest"]})])
    reg.add("test/fake-base", {"1.0": idx}, [amd, arm, att, idx])
    damd = make_image([BASE_L1, BASE_L2], flavor="docker")
    didx = make_index([(damd, {"architecture": "amd64", "os": "linux"}, None)], flavor="docker")
    reg.add("test/docker-base", {"1.0": didx}, [damd, didx])

    new = os.path.join(scratch, "new-flint-sync")
    with open(new, "wb") as f:
        f.write(b"NEW")
    base_ref = f"127.0.0.1:{reg.port}/test/fake-base:1.0"
    work = os.path.join(scratch, "work")

    def derive(out, extra=(), env=None, base=base_ref, src=new, fmt="oci"):
        return run_tool(["--base", base, "--platform", "linux/amd64", "--add", f"{FLINT}={src}",
                         "--tag", TAG, "--out", out, "--plain-http", "--work-dir", work, "--min-free-gb", "1",
                         "--format", fmt] + list(extra), env)

    out1 = os.path.join(scratch, "run1.tar")
    rc, s1, err = derive(out1, ["--env", "FLINT_DRILL=1", "--label", "drill=writers"])
    if not s.check("derive exits 0 through the real pull code", rc == 0 and s1, err.strip()[-800:]):
        return None
    s.check("the amd64 manifest was chosen (not arm64, not the attestation)",
            s1["base"]["manifest_digest"] == amd["digest"] and s1["base"]["index_digest"] == idx["digest"])
    a = read_archive_independent(out1)
    s.check("independent re-read: every digest, size and diff_id matches the bytes", not a["problems"],
            "; ".join(a["problems"]))
    fs = apply_simple(a["raw_layers"])
    s.check(f"independent apply: {FLINT} == NEW", fs.get(FLINT) == b"NEW", repr(fs.get(FLINT)))
    s.check("independent apply: base whiteout and opaque dir still in force, other files intact",
            "/etc/removed" not in fs and "/opt/a" not in fs and fs.get("/opt/c") == b"C"
            and fs.get("/etc/keep") == b"KEEP" and fs.get("/usr/bin/tool") == b"TOOL")
    v = od.verify_archive(out1, expect={FLINT: sha(b"NEW")}, platform="linux/amd64", strict_top=True)
    s.check("tool verifier: ok with --expect and --strict-top-layer", v.report.ok, "; ".join(v.report.errors))
    mine = {p: e.sha256 for p, e in (v.fs.entries.items() if v.fs else []) if e.kind == "file"}
    theirs = {p: sha(b) for p, b in fs.items() if isinstance(b, bytes)}
    s.check("tool verifier and independent applier agree on every regular file", mine == theirs and mine,
            f"only tool: {sorted(set(mine) - set(theirs))}, only independent: {sorted(set(theirs) - set(mine))}")
    cfg = a["config"]
    d = cfg["rootfs"]["diff_ids"]
    s.check("config diff_ids = base (2) + 1, base diff_ids unchanged as a prefix",
            len(d) == len(amd["diff_ids"]) + 1 and d[:-1] == amd["diff_ids"])
    s.check("manifest keeps the base layer descriptors, in order",
            [ld["digest"] for ld in a["manifest"]["layers"][:-1]] == amd["layer_digests"])
    bc, nc = amd["config"]["config"], cfg["config"]
    s.check("entrypoint/cmd/user/workdir unchanged",
            all(bc[k] == nc.get(k) for k in ("Entrypoint", "Cmd", "User", "WorkingDir")))
    s.check("--env appended, --label merged, base label kept",
            "FLINT_DRILL=1" in nc["Env"] and bc["Env"][0] in nc["Env"]
            and nc["Labels"] == {"org.example.base": "yes", "drill": "writers"})
    s.check("history has one entry more", len(cfg["history"]) == len(amd["config"]["history"]) + 1)
    top = new_layer_members(a)
    fm = top.get("usr/local/bin/flint-sync")
    s.check("new layer: parent dirs as entries + the file 0755 root mtime 0",
            {"usr", "usr/local", "usr/local/bin"} <= {n.rstrip("/") for n, m in top.items() if m.isdir()}
            and fm is not None and (fm.mode, fm.uid, fm.gid, fm.mtime) == (0o755, 0, 0, 0)
            and all(m.mtime == 0 for m in top.values()), repr({n: (oct(m.mode), m.uid, m.mtime) for n, m in top.items()}))
    ann = a["index_desc"].get("annotations", {})
    s.check("index names the image docker.io/... and carries the tag and platform",
            ann.get(od.ANN_CONTAINERD_NAME) == TAG and ann.get(od.ANN_OCI_REF_NAME) == "writers-selftest"
            and a["index_desc"].get("platform") == {"architecture": "amd64", "os": "linux"})
    s.check("summary: diff_ids, manifest digest, added sha256 and what it replaced",
            s1["diff_ids"] == d and s1["image"]["manifest_digest"] == a["index_desc"]["digest"]
            and s1["added"][0]["sha256"] == sha(b"NEW") and (s1["added"][0]["replaces"] or {}).get("sha256") == sha(b"OLD")
            and (s1["added"][0]["replaces"] or {}).get("layer") == 1, repr(s1["added"]))
    st = dict(reg.stats)
    s.check("double: auth challenge answered, blobs via redirect, no Authorization leaked to the CDN",
            st.get("unauth_401", 0) >= 1 and st.get("token", 0) >= 1 and st.get("cdn_get", 0) >= 3
            and st.get("cdn_auth_rejected", 0) == 0 and st.get("accept_miss", 0) == 0
            and st.get("handler_exception", 0) == 0, repr(st))
    s.check("work dir cleaned up", not os.listdir(work) if os.path.isdir(work) else True,
            repr(os.listdir(work) if os.path.isdir(work) else None))

    rc, s2, err = derive(os.path.join(scratch, "run2.tar"), ["--env", "FLINT_DRILL=1", "--label", "drill=writers"])
    s.check("determinism: same inputs -> identical layer, config, manifest and archive digests",
            rc == 0 and s2["new_layer"]["digest"] == s1["new_layer"]["digest"]
            and s2["new_layer"]["diff_id"] == s1["new_layer"]["diff_id"]
            and s2["image"]["config_digest"] == s1["image"]["config_digest"]
            and s2["image"]["manifest_digest"] == s1["image"]["manifest_digest"]
            and s2["out"]["sha256"] == s1["out"]["sha256"], err.strip()[-400:])
    new2 = os.path.join(scratch, "new2")
    with open(new2, "wb") as f:
        f.write(b"NEW2")
    rc, s3, err = derive(os.path.join(scratch, "run3.tar"), src=new2)
    s.check("control: different content -> different layer digest",
            rc == 0 and s3["new_layer"]["digest"] != s1["new_layer"]["digest"], err.strip()[-400:])
    rc, s4, err = derive(os.path.join(scratch, "run4.tar"), ["--env", "FLINT_DRILL=1", "--label", "drill=writers"],
                         env={"SOURCE_DATE_EPOCH": "1700000000"})
    a4 = read_archive_independent(os.path.join(scratch, "run4.tar")) if rc == 0 else None
    s.check("control: SOURCE_DATE_EPOCH -> different layer digest, same diff_id count, created set",
            rc == 0 and s4["new_layer"]["digest"] != s1["new_layer"]["digest"]
            and a4["config"]["created"] == "2023-11-14T22:13:20Z"
            and all(m.mtime == 1700000000 for m in new_layer_members(a4).values()), err.strip()[-400:])

    rc, sd, err = derive(os.path.join(scratch, "docker-base.tar"),
                         base=f"127.0.0.1:{reg.port}/test/docker-base:1.0")
    ad = read_archive_independent(os.path.join(scratch, "docker-base.tar")) if rc == 0 else None
    s.check("docker-media-type base: manifest stays docker v2, new layer is docker gzip, content NEW",
            rc == 0 and ad["manifest"]["mediaType"] == od.MT_DOCKER_MANIFEST
            and ad["manifest"]["layers"][-1]["mediaType"] == od.MT_DOCKER_LAYER_GZIP
            and not ad["problems"] and apply_simple(ad["raw_layers"]).get(FLINT) == b"NEW"
            and sd["verify"]["ok"], err.strip()[-400:])
    outd = os.path.join(scratch, "run1-docker.tar")
    rc, sdk, err = derive(outd, ["--env", "FLINT_DRILL=1", "--label", "drill=writers"], fmt="docker")
    adk = read_archive_independent(outd) if rc == 0 else None
    s.check("--format docker: manifest.json Config/RepoTags/Layers, all base layers + new, content NEW",
            rc == 0 and adk["format"] == "docker" and adk["manifest_json"]["RepoTags"] == [TAG]
            and len(adk["manifest_json"]["Layers"]) == 3 and not adk["problems"]
            and apply_simple(adk["raw_layers"]).get(FLINT) == b"NEW"
            and od.verify_archive(outd, expect={FLINT: sha(b"NEW")}).report.ok
            and sdk["image"]["config_digest"] == s1["image"]["config_digest"], err.strip()[-400:])
    rc, ss, err = run_tool(["--base", base_ref, "--add", f"{FLINT}={new}", "--tag",
                            "dilipdalton/flint-s3-worker-lean:writers-short", "--out",
                            os.path.join(scratch, "short.tar"), "--plain-http", "--work-dir", work,
                            "--min-free-gb", "1"])
    s.check("a short --tag is normalized to docker.io/... in the archive",
            rc == 0 and read_archive_independent(os.path.join(scratch, "short.tar"))["name"]
            == "docker.io/dilipdalton/flint-s3-worker-lean:writers-short", err.strip()[-300:])
    rc, out, err = (lambda p: (p.returncode, p.stdout, p.stderr))(subprocess.run(
        [sys.executable, TOOL, "verify", out1, "--expect", f"{FLINT}={sha(b'NEW')}", "--strict-top-layer"],
        capture_output=True, text=True))
    s.check("`oci_derive.py verify` CLI exits 0 on the good archive", rc == 0, err.strip()[-300:])
    return {"out_oci": out1, "out_docker": outd, "amd": amd, "idx": idx, "base_ref": base_ref, "new": new,
            "work": work}


CONTROLS = [
    ("noop re-sign (positive control)", m_noop, set()),
    ("a1 added layer first in manifest only", m_a1, {"diff-id", "expect"}),
    ("a2 added layer first in manifest AND config, re-signed", m_a2, {"expect"}),
    ("b  one byte flipped in the added layer blob", m_b, {"blob-digest"}),
    ("b2 layer content changed + re-signed, diff_id kept", m_b2, {"diff-id", "expect"}),
    ("c  last diff_id dropped, config re-signed", m_c, {"diff-id-count"}),
    ("e  image name without docker.io/", m_e, {"image-name"}),
]


def section_controls(scratch, reg, syn):
    s = Sec("2 CONTROLS")
    if syn is None:
        s.skip("all", "section 1 produced no output")
        return
    for fmt, good in (("oci", syn["out_oci"]), ("docker", syn["out_docker"])):
        v0 = od.verify_archive(good, expect={FLINT: sha(b"NEW")})
        s.check(f"[{fmt}] unmutated output passes (else every control is meaningless)", v0.report.ok,
                "; ".join(v0.report.errors))
        for name, fn, want_failed in CONTROLS:
            if fn is m_e and fmt != "oci":
                continue
            m = Mut(good)
            fn(m)
            path = m.save(os.path.join(scratch, f"mut-{fmt}-{fn.__name__}.tar"))
            v = od.verify_archive(path, expect={FLINT: sha(b"NEW")})
            got = set(v.report.failed_checks())
            if not want_failed:
                s.check(f"[{fmt}] {name}: still PASSES", v.report.ok and not got, "; ".join(v.report.errors))
                continue
            s.check(f"[{fmt}] {name}: verification FAILED on exactly {sorted(want_failed)}",
                    not v.report.ok and got == want_failed,
                    f"failed={sorted(got)} errors={v.report.errors[:3]}", note=(v.report.errors[-1][:150] if v.report.errors else ""))
            if fn is m_a2:
                e, _ = v.fs.lookup(FLINT) if v.fs else (None, None)
                s.check(f"[{fmt}] a2: every digest re-signed (blob-digest, diff-id, diff-id-count pass) and the "
                        f"topmost {FLINT} is the base's OLD",
                        all(v.report.status.get(c) == "pass" for c in ("blob-digest", "diff-id", "diff-id-count"))
                        and e is not None and e.content == b"OLD",
                        repr({c: v.report.status.get(c) for c in ("blob-digest", "diff-id", "diff-id-count")}))
                rc = subprocess.run([sys.executable, TOOL, "verify", path, "--expect", f"{FLINT}={sha(b'NEW')}"],
                                    capture_output=True, text=True).returncode
                s.check(f"[{fmt}] a2: `oci_derive.py verify` CLI exits 1", rc == 1, f"exit {rc}")

        m = Mut(good)
        m_f(m)
        path = m.save(os.path.join(scratch, f"mut-{fmt}-m_f.tar"))
        vs = od.verify_archive(path, expect={FLINT: sha(b"NEW")}, strict_top=True)
        vn = od.verify_archive(path, expect={FLINT: sha(b"NEW")})
        s.check(f"[{fmt}] f  top layer's dir replaces the base's /bin symlink, re-signed: --strict-top-layer "
                f"FAILED on exactly ['top-layer']", set(vs.report.failed_checks()) == {"top-layer"},
                f"failed={vs.report.failed_checks()} errors={vs.report.errors}",
                note=(vs.report.errors[0][:150] if vs.report.errors else ""))
        s.check(f"[{fmt}] f  the same archive WITHOUT --strict-top-layer passes (that arm alone catches it)",
                vn.report.ok, "; ".join(vn.report.errors))

    # derive-side refusals
    amd, idx, work, new = syn["amd"], syn["idx"], syn["work"], syn["new"]

    def refused(label, repo, needle, extra=(), stat=None, add=None):
        before = reg.stats.get(stat, 0) if stat else 0
        out = os.path.join(scratch, f"refused-{repo.replace('/', '-')}.tar")
        rc, _, err = run_tool(["--base", f"127.0.0.1:{reg.port}/{repo}:1.0", "--add", add or f"{FLINT}={new}",
                               "--tag", TAG, "--out", out, "--plain-http", "--work-dir", work,
                               "--min-free-gb", "1"] + list(extra))
        caused = (reg.stats.get(stat, 0) > before) if stat else True
        s.check(f"derive REFUSES {label}",
                rc == 1 and needle in err and caused and not os.path.exists(out)
                and not os.path.exists(out + ".partial"),
                f"exit {rc}, fault served={caused}, stderr: {err.strip()[-300:]}")

    reg.add("test/corrupt-blob", {"1.0": idx}, [amd, idx], faults={"corrupt_blob": amd["layer_digests"][1]})
    refused("a corrupted layer blob from the registry", "test/corrupt-blob", "blob digest mismatch",
            stat="corrupt_served")
    lie_body = jb({**json.loads(idx["manifest"]), "annotations": {"x": "tampered"}})
    reg.add("test/lie-tag", {"1.0": idx}, [amd, idx],
            faults={"lie_manifest": {"ref": "1.0", "body": lie_body, "header": "original"}})
    refused("a manifest body that differs from Docker-Content-Digest", "test/lie-tag", "Docker-Content-Digest",
            stat="lie_served")
    other = make_image([[("usr/local/bin/flint-sync", "file", b"EVIL")]])
    reg.add("test/lie-digest", {"1.0": idx}, [amd, idx, other],
            faults={"lie_manifest": {"ref": amd["digest"], "body": other["manifest"], "header": "consistent"}})
    refused("a manifest that is not the digest it was requested by (header consistent with the lie)",
            "test/lie-digest", "asked for", stat="lie_served")
    bad = make_image([BASE_L1, BASE_L2], cfg_mutate=lambda c: c["rootfs"]["diff_ids"].__setitem__(0, dg(b"x")))
    reg.add("test/bad-diffid", {"1.0": bad}, [bad])
    refused("a base whose config diff_id does not match its layer", "test/bad-diffid", "config diff_id")
    sym = make_image([[("usr/", "dir"), ("usr/local/", "dir"), ("opt/", "dir"), ("opt/bin/", "dir"),
                       ("opt/bin/flint-sync", "file", b"OLD"), ("usr/local/bin", "symlink", "/opt/bin")]])
    reg.add("test/symlink-parent", {"1.0": sym}, [sym])
    refused("an added path whose parent is a symlink in the base", "test/symlink-parent", "is a symlink")
    rc, ok, err = run_tool(["--base", f"127.0.0.1:{reg.port}/test/symlink-parent:1.0", "--add",
                            f"/opt/bin/flint-sync={new}", "--tag", TAG, "--out",
                            os.path.join(scratch, "symlink-resolved.tar"), "--plain-http", "--work-dir", work,
                            "--min-free-gb", "1"])
    s.check("twin: the same base accepts the RESOLVED path (the refusal is the symlink's, not the repo's)",
            rc == 0 and ok["verify"]["ok"], err.strip()[-300:])
    refused("a platform the index does not offer", "test/fake-base", "no manifest for linux/s390x",
            extra=["--platform", "linux/s390x"])


def section_live(scratch, require):
    s = Sec("3 LIVE")
    try:
        urllib.request.urlopen("https://registry-1.docker.io/v2/", timeout=8)
        reachable = True
    except urllib.error.HTTPError as e:
        reachable = e.code == 401
    except (urllib.error.URLError, OSError):
        reachable = False
    if not reachable:
        if require:
            s.check("Docker Hub reachable (--require-network)", False)
        else:
            s.skip("busybox derive", "registry-1.docker.io unreachable")
        return
    nonce = secrets.token_hex(16).encode() + b"\n"
    marker, group = os.path.join(scratch, "marker"), os.path.join(scratch, "group")
    with open(marker, "wb") as f:
        f.write(nonce)
    with open(group, "wb") as f:
        f.write(b"root:x:0:\ndrill:x:4242:\n")
    out = os.path.join(scratch, "busybox-derived.tar")
    rc, sm, err = run_tool(["--base", "library/busybox:1.36", "--platform", "linux/amd64",
                            "--add", f"/drill-marker={marker}", "--add", f"/etc/group={group}",
                            "--tag", "docker.io/dilipdalton/busybox-drill:selftest", "--out", out,
                            "--work-dir", os.path.join(scratch, "work-live"), "--min-free-gb", "1"])
    if rc == 1 and "429" in err:
        if require:
            s.check("busybox derive (--require-network)", False, f"rate limited: {err.strip()[-300:]}")
        else:
            s.skip("busybox derive", f"Docker Hub rate limited: {err.strip()[-200:]}")
        return
    if not s.check("derive from library/busybox:1.36 linux/amd64 exits 0", rc == 0 and sm, err.strip()[-600:]):
        return
    a = read_archive_independent(out)
    s.check("independent re-read: digests and diff_ids match", not a["problems"], "; ".join(a["problems"]))
    fs = apply_simple(a["raw_layers"])
    s.check("independent apply: /drill-marker is the nonce, /etc/group is ours",
            fs.get("/drill-marker") == nonce and fs.get("/etc/group") == b"root:x:0:\ndrill:x:4242:\n")
    v = od.verify_archive(out, expect={"/drill-marker": sha(nonce)}, platform="linux/amd64", strict_top=True)
    s.check("tool verifier ok", v.report.ok, "; ".join(v.report.errors))
    top = len(v.info["diff_ids"]) - 1
    e, _ = v.fs.lookup("/drill-marker") if v.fs else (None, None)
    s.check("the marker's topmost version is in the added (top) layer, and the base had none",
            e is not None and e.layer == top and sm["added"][0]["path"] == "/drill-marker"
            and sm["added"][0]["replaces"] is None)
    g = [x for x in sm["added"] if x["path"] == "/etc/group"][0]
    s.check("/etc/group existed in the base with other bytes and is now the top layer's",
            g["replaces"] is not None and g["replaces"]["sha256"] != g["sha256"]
            and v.fs.lookup("/etc/group")[0].layer == top)
    mine = {p: x.sha256 for p, x in v.fs.entries.items() if x.kind == "file"}
    theirs = {p: sha(b) for p, b in fs.items() if isinstance(b, bytes)}
    s.check(f"tool verifier and independent applier agree on all {len(mine)} regular files of a REAL image",
            mine == theirs, f"diff: {sorted(set(mine.items()) ^ set(theirs.items()))[:5]}")
    bb, sh_ = v.fs.lookup("/bin/busybox")[0], v.fs.lookup("/bin/sh")[0]
    s.check("real layer's hardlinks modelled: /bin/sh has /bin/busybox's bytes",
            bb is not None and sh_ is not None and bb.sha256 == sh_.sha256)
    m = Mut(out)
    m_a2(m)
    a2 = m.save(os.path.join(scratch, "busybox-a2.tar"))
    # A reorder is only observable on a path the BASE also has: /drill-marker
    # has one version whatever the order, so the filesystem is unchanged and
    # the verifier must PASS it; /etc/group has two, and the base's must win.
    va = od.verify_archive(a2, expect={"/drill-marker": sha(nonce)})
    s.check("live a2 (added layer first, re-signed), expecting only the NEW path: PASSES (one version, same fs)",
            va.report.ok, "; ".join(va.report.errors))
    vb = od.verify_archive(a2, expect={"/drill-marker": sha(nonce), "/etc/group": g["sha256"]})
    s.check("live a2, also expecting the OVERWRITTEN /etc/group: FAILED on exactly ['expect'], naming /etc/group",
            set(vb.report.failed_checks()) == {"expect"} and len(vb.report.errors) == 1
            and vb.report.errors[0].startswith("expect: /etc/group"),
            f"failed={vb.report.failed_checks()} errors={vb.report.errors}", note=vb.report.errors[0] if vb.report.errors else "")
    s.check("live summary digests: base index, base manifest, new manifest",
            bool(sm["base"]["index_digest"] and sm["base"]["manifest_digest"] and sm["image"]["manifest_digest"]),
            note=f"index {sm['base']['index_digest']} manifest {sm['base']['manifest_digest']} "
                 f"new {sm['image']['manifest_digest']} diff_ids {sm['diff_ids']}")


CTR_DOUBLE = r"""#!/bin/sh
# ctr DOUBLE for verify_image.sh control-flow tests. It is NOT containerd.
echo "ctr $*" >> "$FAKE_CTR_LOG"
ns=default
while [ $# -gt 0 ]; do
  case "$1" in -n|--namespace) ns=$2; shift 2 ;; *) break ;; esac
done
[ "$ns" = "$FAKE_CTR_NS" ] || { echo "ctr double: namespace $ns has no images" >&2; [ "$1 $2" = "images ls" ] && exit 0; exit 1; }
sub="$1 $2"; shift 2
case "$sub" in
  "images ls")
    if [ "${1:-}" = "-q" ]; then cut -f1 "$FAKE_CTR_IMAGES"
    else printf 'REF TYPE DIGEST SIZE PLATFORMS LABELS\n'
         awk -F'\t' '{ print $1 " application/vnd.oci.image.manifest.v1+json " $3 " 12.8 MiB linux/amd64 -" }' "$FAKE_CTR_IMAGES"
    fi ;;
  "images mount")
    while [ $# -gt 0 ]; do case "$1" in --platform|--snapshotter) shift 2 ;; --*) shift ;; *) break ;; esac; done
    root=$(awk -F'\t' -v r="$1" '$1 == r { print $2 }' "$FAKE_CTR_IMAGES")
    [ -n "$root" ] || { echo "ctr: image \"$1\": not found" >&2; exit 1; }
    [ -z "${FAKE_CTR_MOUNT_FAIL:-}" ] || { echo "ctr: mount failed (double)" >&2; exit 1; }
    cp -R "$root/." "$2/" && echo "sha256:fakechain" && echo "$2" ;;
  "images unmount")
    [ "$1" = "--rm" ] && shift
    find "$1" -mindepth 1 -delete; echo "$1" ;;
  *) echo "ctr double: unsupported: $sub" >&2; exit 9 ;;
esac
"""


def section_node_script(scratch):
    s = Sec("4 NODE SCRIPT")
    if not os.path.exists(VERIFY_SH):
        s.check("verify_image.sh exists", False)
        return
    base = os.path.join(scratch, "node")
    fakebin, tmp = os.path.join(base, "bin"), os.path.join(base, "tmp")
    root = os.path.join(base, "image-root")
    for d in (fakebin, tmp, os.path.join(root, "usr/local/bin"), os.path.join(root, "usr/bin")):
        os.makedirs(d, exist_ok=True)
    with open(os.path.join(root, "usr/local/bin/flint-sync"), "wb") as f:
        f.write(b"NEW")
    with open(os.path.join(root, "usr/bin/tool"), "wb") as f:
        f.write(b"TOOL")
    if not os.path.lexists(os.path.join(root, "bin")):
        os.symlink("usr/bin", os.path.join(root, "bin"))
    with open(os.path.join(fakebin, "ctr"), "w") as f:
        f.write(CTR_DOUBLE)
    os.chmod(os.path.join(fakebin, "ctr"), 0o755)
    images = os.path.join(base, "images.tsv")
    digest = "sha256:" + "ab" * 32
    with open(images, "w") as f:
        f.write(f"{TAG}\t{root}\t{digest}\n")
    crictl_fail = os.path.join(base, "crictl-fail")
    os.makedirs(crictl_fail, exist_ok=True)
    with open(os.path.join(crictl_fail, "crictl"), "w") as f:
        f.write("#!/bin/sh\necho 'no such image' >&2\nexit 1\n")
    os.chmod(os.path.join(crictl_fail, "crictl"), 0o755)
    crictl_ok = os.path.join(base, "crictl-ok")
    os.makedirs(crictl_ok, exist_ok=True)
    with open(os.path.join(crictl_ok, "crictl"), "w") as f:
        f.write('#!/bin/sh\necho "crictl $*" >> "$FAKE_CTR_LOG"\nexit 0\n')
    os.chmod(os.path.join(crictl_ok, "crictl"), 0o755)
    sys_path = "/usr/bin:/bin:/usr/sbin:/sbin"
    shells = [sh for sh in ("/bin/sh", "/bin/dash") if os.path.exists(sh)]
    if not shutil.which("sha256sum", path=sys_path):
        s.skip("all", "no sha256sum on this host")
        return
    good, old = sha(b"NEW"), sha(b"OLD")

    cases = [
        ("match", [TAG, FLINT, good], 0, [], True),
        ("short ref normalized to docker.io/...", ["dilipdalton/flint-s3-worker-lean:writers-selftest", FLINT, good],
         0, [], True),
        ("sha256 mismatch", [TAG, FLINT, old], 1, [], True),
        ("image absent", [TAG.replace("selftest", "nope"), FLINT, good], 3, [], False),
        ("wrong namespace sees nothing", ["-n", "default", TAG, FLINT, good], 3, [], False),
        ("path absent", [TAG, "/usr/local/bin/missing", good], 4, [], True),
        ("path through a symlink refused", [TAG, "/bin/tool", sha(b"TOOL")], 4, [], True),
        ("resolved path of that symlink verifies", [TAG, "/usr/bin/tool", sha(b"TOOL")], 0, [], True),
        ("two pairs, one mismatch", [TAG, FLINT, good, "/usr/bin/tool", old], 1, [], True),
        ("--digest matches", ["--digest", digest, TAG, FLINT, good], 0, [], True),
        ("--digest differs", ["--digest", "sha256:" + "cd" * 32, TAG, FLINT, good], 7, [], False),
        ("crictl present and CRI does not see it", [TAG, FLINT, good], 6, [crictl_fail], False),
        ("crictl present and CRI sees it", [TAG, FLINT, good], 0, [crictl_ok], True),
        ("mount fails", [TAG, FLINT, good], 5, ["MOUNT_FAIL"], True),
        ("not a sha256", [TAG, FLINT, "xyz"], 2, [], False),
        ("relative path", [TAG, "usr/local/bin/flint-sync", good], 2, [], False),
    ]
    for shell in shells:
        for name, args, want_rc, extra, expect_mount in cases:
            log = os.path.join(base, "ctr.log")
            if os.path.exists(log):
                os.unlink(log)
            path_dirs = [fakebin] + [e for e in extra if e != "MOUNT_FAIL"]
            env = {"PATH": ":".join(path_dirs + [sys_path]), "TMPDIR": tmp, "FAKE_CTR_LOG": log,
                   "FAKE_CTR_IMAGES": images, "FAKE_CTR_NS": "k8s.io", "HOME": base}
            if "MOUNT_FAIL" in extra:
                env["FAKE_CTR_MOUNT_FAIL"] = "1"
            p = subprocess.run([shell, VERIFY_SH] + args, capture_output=True, text=True, env=env, timeout=60)
            calls = open(log).read() if os.path.exists(log) else ""
            leftovers = os.listdir(tmp)
            mounted = "images mount" in calls
            cleaned = ("images unmount --rm" in calls) if mounted else True
            ok = p.returncode == want_rc and not leftovers and cleaned and (mounted == expect_mount)
            s.check(f"[{os.path.basename(shell)}] {name}: exit {want_rc}"
                    + (", unmounted --rm and dir removed" if expect_mount else ", never mounted"),
                    ok, f"exit {p.returncode}, mounted={mounted}, cleaned={cleaned}, leftovers={leftovers}, "
                        f"out={p.stdout.strip()[-300:]!r} err={p.stderr.strip()[-200:]!r}")
            for d in leftovers:
                shutil.rmtree(os.path.join(tmp, d), ignore_errors=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--require-network", action="store_true", help="a skipped live section is a FAIL")
    ap.add_argument("--keep", action="store_true", help="keep the scratch dir")
    ap.add_argument("--scratch", help="parent dir for scratch (never inside the repo)")
    ap.add_argument("--only", help="comma list of sections to run: model,synthetic,controls,live,node")
    args = ap.parse_args()
    parent = args.scratch or os.environ.get("OCI_DERIVE_SCRATCH") or \
        ("/private/tmp/claude-503" if os.path.isdir("/private/tmp/claude-503") else tempfile.gettempdir())
    parent = os.path.realpath(parent)
    if os.path.commonpath([parent, os.path.realpath(REPO_ROOT)]) == os.path.realpath(REPO_ROOT):
        print(f"refusing to put scratch inside the repo: {parent}", file=sys.stderr)
        return 2
    only = set((args.only or "model,synthetic,controls,live,node").split(","))
    scratch = tempfile.mkdtemp(prefix="oci-derive-selftest-", dir=parent)
    print(f"scratch: {scratch}")
    reg = FakeRegistry().start()

    def guarded(label, fn, *a):
        """A crash in a section is a recorded FAIL, not an abort that exits 1 for the wrong reason."""
        try:
            return fn(*a)
        except Exception as e:  # noqa: BLE001
            record(label, "section crashed", "FAIL", f"{type(e).__name__}: {e}\n{traceback.format_exc()[-1200:]}")
            return None

    try:
        if "model" in only:
            guarded("0 MODEL", section_model)
        syn = guarded("1 SYNTHETIC", section_synthetic, scratch, reg) if only & {"synthetic", "controls"} else None
        if "controls" in only:
            guarded("2 CONTROLS", section_controls, scratch, reg, syn)
        if "live" in only:
            guarded("3 LIVE", section_live, scratch, args.require_network)
        if "node" in only:
            guarded("4 NODE SCRIPT", section_node_script, scratch)
    finally:
        reg.stop()
        if args.keep:
            print(f"kept: {scratch}")
        else:
            shutil.rmtree(scratch, ignore_errors=True)
    n = {k: sum(1 for r in RESULTS if r[2] == k) for k in ("PASS", "FAIL", "SKIP")}
    print(f"\n{n['PASS']} passed, {n['FAIL']} failed, {n['SKIP']} skipped")
    for sec, name, st, detail in RESULTS:
        if st != "PASS":
            print(f"  {st} [{sec}] {name}: {detail}")
    return 1 if n["FAIL"] else 0


if __name__ == "__main__":
    sys.exit(main())
