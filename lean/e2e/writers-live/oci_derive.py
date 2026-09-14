#!/usr/bin/env python3
"""oci_derive.py — derive a container image from a PUBLISHED one by adding ONE
layer of files, with no Docker daemon, and write a tarball that containerd on a
node imports with

    ctr -n k8s.io images import --platform linux/amd64 <out.tar>

Every drill image is, in its Dockerfile, "FROM <published image> + COPY a few
static musl binaries" (spdk-csi-driver/docker/Dockerfile.s3worker-lean,
.sync.prebuilt, .s3csi.prebuilt, .operator.prebuilt). So a derived image is the
published image, byte for byte, plus one layer that overwrites those binaries.

  derive (the default verb):
    oci_derive.py --base dilipdalton/flint-s3-worker-lean:1.51.0 \\
        --platform linux/amd64 \\
        --add /usr/local/bin/flint-sync=/path/to/flint-sync [--add ...] \\
        --tag docker.io/dilipdalton/flint-s3-worker-lean:writers-<sha8> \\
        --out /private/tmp/claude-503/worker-lean.tar \\
        [--env K=V] [--label K=V] [--format oci|docker] [--work-dir DIR]

  verify an archive (what derive runs on its own output before renaming it):
    oci_derive.py verify <out.tar> [--expect /path=<sha256>] [--platform os/arch]
        [--strict-top-layer]

Python 3 stdlib only.

PULL. Anonymous Docker Hub (or any registry speaking the distribution API):
the manifest GET answers 401 with a Bearer challenge; for docker.io that is a
token from auth.docker.io for `repository:<name>:pull`. Manifests are fetched
with Accept for the OCI index, docker manifest list, OCI manifest and docker v2
manifest; an index is resolved to the requested platform (attestation
manifests skipped). Every manifest body is hashed against the digest it was
requested by and against Docker-Content-Digest; every blob is STREAMED to a
file under the work dir while hashed, and a sha256 or size mismatch is a
refusal. The Authorization header is never forwarded to the blob redirect
(S3-backed CDNs reject a second auth mechanism). Optional credentials:
OCI_DERIVE_REGISTRY_USER / OCI_DERIVE_REGISTRY_PASSWORD.

BASE SCAN. Before building anything the base layers are decompressed and
applied in order (overwrites, whiteouts, symlinked parents — the overlay
snapshotter's semantics), each layer's uncompressed sha256 is checked against
the base config's rootfs.diff_ids, and every --add is checked against the
result: a parent of an added path that is a SYMLINK or a FILE in the base is a
refusal (the layer's directory entry would REPLACE it and hide what it pointed
to — e.g. /bin -> usr/bin on ubuntu), as is an added path that is a directory
or symlink in the base. Existing parent directories keep the base's mode and
owner in the new layer.

THE LAYER. A tar of the parent directories and the added files (mode 0755,
uid/gid 0), sorted, with mtime = SOURCE_DATE_EPOCH or 0, gzipped with mtime 0
and no file name: the same inputs give the same diff_id and digest (and, since
the config's `created` is the base's unless SOURCE_DATE_EPOCH is set, the same
config, manifest and archive digests). The gzip bytes depend on the zlib
version, the diff_id does not.

THE CONFIG. The base config, with the diff_id appended to rootfs.diff_ids, a
history entry appended (when the base has history), --env replacing or
appending Env entries, --label merged into Labels. Entrypoint, Cmd, User and
WorkingDir are untouched.

FORMAT. `--format oci` (default): an OCI image layout in a tar
(`oci-layout`, `index.json`, `blobs/sha256/<hex>`), with the index entry
annotated `io.containerd.image.name=<full ref>` and
`org.opencontainers.image.ref.name=<tag>`. `--format docker`: the `docker save`
layout (`manifest.json` with Config/RepoTags/Layers, blobs by name).
containerd's importer (core/images/archive/importer.go ImportIndex) accepts
both — it prefers OCI when `oci-layout` is present — and OCI is the default
because (1) the manifest digest printed here IS the digest containerd stores
(for the docker layout containerd writes its own manifest, so no digest is
known in advance), (2) media types and platform are declared, not sniffed, and
(3) `io.containerd.image.name` is used verbatim as the image name by both the
transfer-service import (2.x default) and the local import (1.7 default).
VERBATIM means it is not normalized, and the kubelet's CRI looks images up by
the normalized name — so the tag is normalized here to `docker.io/...`.

VERIFY. Every check is named, so a caller (and the self-test's negative
controls) can see WHICH check failed:
  archive         the outer tar reads; no duplicate member names
  format          oci-layout 1.0.0 + index.json, or manifest.json
  index           exactly one image; manifest media type is a manifest
  image-name      OCI: io.containerd.image.name is a full, tagged, normalized
                  ref (what CRI resolves); docker: RepoTags parse
  blob-present    every referenced blob is in the archive
  blob-digest     sha256 and size of each blob match its descriptor (or the
                  digest its name encodes)
  config          the config parses; rootfs.type == layers
  platform        config os/architecture == index platform (and --platform)
  diff-id-count   len(rootfs.diff_ids) == len(layers)
  layer-media-type  declared compression matches the bytes
  layer-apply     every layer decompresses and applies
  diff-id         sha256(uncompressed layer i) == rootfs.diff_ids[i]
  expect          after applying ALL layers in manifest order, each expected
                  path is a regular file, reached through no symlink, with
                  that sha256 — i.e. the TOPMOST version is the expected one
  top-layer       (--strict-top-layer) the top layer changes no entry's type,
                  whites nothing out and writes through no symlinked parent
A check that could not be evaluated because an earlier one failed is SKIP,
and a report with any FAIL or SKIP is not ok.

Exit codes: 0 ok, 1 refusal/verification failure, 2 usage.
"""

from __future__ import annotations

import argparse
import copy
import gzip
import hashlib
import http.client
import io
import json
import os
import posixpath
import re
import shutil
import socket
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import zlib

CHUNK = 1 << 20
MAX_JSON = 20 << 20  # containerd's jsonLimit for manifest.json / index.json
MAX_MANIFEST = 4 << 20

MT_OCI_INDEX = "application/vnd.oci.image.index.v1+json"
MT_DOCKER_LIST = "application/vnd.docker.distribution.manifest.list.v2+json"
MT_OCI_MANIFEST = "application/vnd.oci.image.manifest.v1+json"
MT_DOCKER_MANIFEST = "application/vnd.docker.distribution.manifest.v2+json"
MT_OCI_CONFIG = "application/vnd.oci.image.config.v1+json"
MT_DOCKER_CONFIG = "application/vnd.docker.container.image.v1+json"
MT_OCI_LAYER_GZIP = "application/vnd.oci.image.layer.v1.tar+gzip"
MT_DOCKER_LAYER_GZIP = "application/vnd.docker.image.rootfs.diff.tar.gzip"
MT_OCI_LAYER_TAR = "application/vnd.oci.image.layer.v1.tar"
MT_DOCKER_LAYER_TAR = "application/vnd.docker.image.rootfs.diff.tar"

INDEX_TYPES = (MT_OCI_INDEX, MT_DOCKER_LIST)
MANIFEST_TYPES = (MT_OCI_MANIFEST, MT_DOCKER_MANIFEST)
LAYER_GZIP_TYPES = (MT_OCI_LAYER_GZIP, MT_DOCKER_LAYER_GZIP)
LAYER_TAR_TYPES = (MT_OCI_LAYER_TAR, MT_DOCKER_LAYER_TAR)
ACCEPT_MANIFESTS = ", ".join(INDEX_TYPES + MANIFEST_TYPES)

ANN_CONTAINERD_NAME = "io.containerd.image.name"
ANN_OCI_REF_NAME = "org.opencontainers.image.ref.name"

DIGEST_RE = re.compile(r"^sha256:([0-9a-f]{64})$")
BLOB_NAME_RE = re.compile(r"^blobs/sha256/([0-9a-f]{64})$")
DOCKER_CFG_NAME_RE = re.compile(r"^([0-9a-f]{64})\.json$")
REPO_PATH_RE = re.compile(
    r"^[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*(?:/[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*)*$")
TAG_RE = re.compile(r"^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$")

VERIFY_CHECKS = ("archive", "format", "index", "image-name", "blob-present",
                 "blob-digest", "config", "platform", "diff-id-count",
                 "layer-media-type", "layer-apply", "diff-id", "expect",
                 "top-layer")


class DeriveError(Exception):
    """A refusal. Nothing is left at --out."""


class ApplyError(Exception):
    """A layer that a runtime could not apply (bad compression, ENOTDIR, ...)."""


def log(msg):
    print(f"oci_derive: {msg}", file=sys.stderr, flush=True)


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for b in iter(lambda: f.read(CHUNK), b""):
            h.update(b)
    return h.hexdigest()


class HashReader:
    """A read-through wrapper that hashes and counts every byte read."""

    def __init__(self, f):
        self.f, self.h, self.n = f, hashlib.sha256(), 0

    def read(self, n=-1):
        b = self.f.read(n)
        self.h.update(b)
        self.n += len(b)
        return b

    def drain(self):
        while self.read(CHUNK):
            pass

    def hexdigest(self):
        return self.h.hexdigest()


class HashWriter:
    """A write-through wrapper that hashes and counts every byte written."""

    def __init__(self, f):
        self.f, self.h, self.n = f, hashlib.sha256(), 0

    def write(self, b):
        self.f.write(b)
        self.h.update(b)
        self.n += len(b)
        return len(b)

    def tell(self):
        return self.n

    def flush(self):
        self.f.flush()

    def hexdigest(self):
        return self.h.hexdigest()


# ---------------------------------------------------------------- references

class Ref:
    """A docker-style image reference, normalized the way containerd/CRI do."""

    def __init__(self, domain, path, tag, digest):
        self.domain, self.path, self.tag, self.digest = domain, path, tag, digest

    @classmethod
    def parse(cls, s):
        if not s or s != s.strip() or any(c.isspace() for c in s):
            raise DeriveError(f"invalid image reference {s!r}")
        name, digest = s, None
        if "@" in name:
            name, digest = name.split("@", 1)
            if not DIGEST_RE.match(digest):
                raise DeriveError(f"unsupported digest in {s!r} (sha256 only)")
        tag = None
        colon, slash = name.rfind(":"), name.rfind("/")
        if colon > slash:
            name, tag = name[:colon], name[colon + 1:]
            if not TAG_RE.match(tag):
                raise DeriveError(f"invalid tag {tag!r} in {s!r}")
        first, sep, rest = name.partition("/")
        if sep and ("." in first or ":" in first or first == "localhost"):
            domain, path = first, rest
        else:
            domain, path = "docker.io", name
        if domain == "index.docker.io":
            domain = "docker.io"
        if domain == "docker.io" and "/" not in path:
            path = "library/" + path
        if not REPO_PATH_RE.match(path):
            raise DeriveError(f"invalid repository path {path!r} in {s!r}")
        return cls(domain, path, tag, digest)

    @property
    def api_host(self):
        return "registry-1.docker.io" if self.domain == "docker.io" else self.domain

    @property
    def reference(self):
        return self.digest or self.tag or "latest"

    def full(self):
        s = f"{self.domain}/{self.path}"
        if self.tag:
            s += f":{self.tag}"
        if self.digest:
            s += f"@{self.digest}"
        return s


def parse_platform(s):
    parts = s.split("/")
    if len(parts) not in (2, 3) or not all(parts):
        raise DeriveError(f"invalid platform {s!r} (want os/arch[/variant])")
    p = {"os": parts[0], "architecture": parts[1]}
    if len(parts) == 3:
        p["variant"] = parts[2]
    return p


def platform_str(p):
    if not p:
        return "<none>"
    s = f"{p.get('os', '?')}/{p.get('architecture', '?')}"
    return s + (f"/{p['variant']}" if p.get("variant") else "")


def platform_matches(have, want):
    if not have or have.get("os") != want["os"] or have.get("architecture") != want["architecture"]:
        return False
    wv, hv = want.get("variant"), have.get("variant")
    if want["architecture"] == "arm64":
        wv, hv = wv or "v8", hv or "v8"
    return wv is None or wv == hv


# ------------------------------------------------------------------ registry

class Registry:
    def __init__(self, ref, plain_http=False, timeout=60):
        self.ref = ref
        self.base = f"{'http' if plain_http else 'https'}://{ref.api_host}/v2/{ref.path}"
        self.timeout = timeout
        self.token = None
        self.user = os.environ.get("OCI_DERIVE_REGISTRY_USER")
        self.password = os.environ.get("OCI_DERIVE_REGISTRY_PASSWORD")

    def _authenticate(self, challenge):
        if not challenge or not challenge.lower().startswith("bearer "):
            raise DeriveError(f"registry demanded auth with an unsupported challenge: {challenge!r}")
        params = dict(re.findall(r'(\w+)="([^"]*)"', challenge))
        realm = params.get("realm")
        if not realm:
            raise DeriveError(f"bearer challenge without a realm: {challenge!r}")
        q = {"scope": params.get("scope") or f"repository:{self.ref.path}:pull"}
        if params.get("service"):
            q["service"] = params["service"]
        req = urllib.request.Request(realm + ("&" if "?" in realm else "?") + urllib.parse.urlencode(q))
        if self.user and self.password:
            import base64
            cred = base64.b64encode(f"{self.user}:{self.password}".encode()).decode()
            req.add_header("Authorization", f"Basic {cred}")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as r:
                body = json.loads(r.read(MAX_MANIFEST))
        except (urllib.error.URLError, ValueError, OSError) as e:
            raise DeriveError(f"token request to {realm} failed: {e}") from e
        self.token = body.get("token") or body.get("access_token")
        if not self.token:
            raise DeriveError(f"token response from {realm} carried no token")

    def open(self, url, accept=None):
        """GET url; handles the 401 challenge, retries transient failures.
        The token is an UNREDIRECTED header: a blob's 307 to a CDN never
        carries it."""
        auths, delay = 0, 1.0
        for attempt in range(1, 6):
            req = urllib.request.Request(url)
            if accept:
                req.add_header("Accept", accept)
            if self.token:
                req.add_unredirected_header("Authorization", f"Bearer {self.token}")
            try:
                return urllib.request.urlopen(req, timeout=self.timeout)
            except urllib.error.HTTPError as e:
                body = e.read(600).decode("utf-8", "replace")
                if e.code == 401 and auths < 2:
                    auths += 1
                    self._authenticate(e.headers.get("WWW-Authenticate"))
                    continue
                if e.code == 429:
                    rl = {k: v for k, v in e.headers.items() if "ratelimit" in k.lower()}
                    raise DeriveError(f"GET {url}: 429 rate limited {rl} {body.strip()}") from e
                if 500 <= e.code < 600 and attempt < 5:
                    log(f"GET {url}: HTTP {e.code}, retry {attempt}")
                    time.sleep(delay)
                    delay *= 2
                    continue
                raise DeriveError(f"GET {url}: HTTP {e.code} {e.reason}: {body.strip()}") from e
            except (urllib.error.URLError, socket.timeout, ConnectionError, TimeoutError) as e:
                if attempt < 5:
                    log(f"GET {url}: {e}, retry {attempt}")
                    time.sleep(delay)
                    delay *= 2
                    continue
                raise DeriveError(f"GET {url}: {e}") from e
        raise DeriveError(f"GET {url}: gave up")

    def get_manifest(self, reference):
        """-> (media_type, body, sha256 digest, parsed). Refuses a body whose
        sha256 differs from the digest it was asked for or from
        Docker-Content-Digest."""
        url = f"{self.base}/manifests/{reference}"
        with self.open(url, accept=ACCEPT_MANIFESTS) as r:
            body = r.read(MAX_MANIFEST + 1)
            ctype = (r.headers.get("Content-Type") or "").split(";")[0].strip()
            dcd = r.headers.get("Docker-Content-Digest")
            rl = r.headers.get("ratelimit-remaining")
        if rl:
            log(f"registry ratelimit-remaining: {rl}")
        if len(body) > MAX_MANIFEST:
            raise DeriveError(f"{url}: manifest larger than {MAX_MANIFEST} bytes")
        actual = "sha256:" + hashlib.sha256(body).hexdigest()
        if DIGEST_RE.match(reference) and reference != actual:
            raise DeriveError(f"{url}: manifest digest mismatch: asked for {reference}, body is {actual}")
        if dcd and dcd.startswith("sha256:") and dcd != actual:
            raise DeriveError(f"{url}: manifest digest mismatch: Docker-Content-Digest {dcd}, body is {actual}")
        try:
            doc = json.loads(body)
        except ValueError as e:
            raise DeriveError(f"{url}: manifest is not JSON: {e}") from e
        if doc.get("schemaVersion") != 2:
            raise DeriveError(f"{url}: schemaVersion {doc.get('schemaVersion')!r} unsupported (schema 1 is not)")
        mt = doc.get("mediaType") or ctype
        if not mt or mt not in INDEX_TYPES + MANIFEST_TYPES:
            if "manifests" in doc:
                mt = MT_OCI_INDEX
            elif "layers" in doc:
                mt = MT_OCI_MANIFEST
            else:
                raise DeriveError(f"{url}: unrecognized manifest media type {mt!r}")
        return mt, body, actual, doc

    def fetch_blob(self, digest, size, dest_dir):
        """Stream a blob to dest_dir/<hex>, hashing as it goes. A sha256 or
        size mismatch is a refusal and leaves no file."""
        m = DIGEST_RE.match(digest or "")
        if not m:
            raise DeriveError(f"unsupported blob digest {digest!r} (sha256 only)")
        dest = os.path.join(dest_dir, m.group(1))
        if os.path.exists(dest):
            return dest
        url = f"{self.base}/blobs/{digest}"
        partial = dest + ".partial"
        for attempt in range(1, 5):
            h, n = hashlib.sha256(), 0
            try:
                with self.open(url) as r, open(partial, "wb") as out:
                    for b in iter(lambda: r.read(CHUNK), b""):
                        h.update(b)
                        n += len(b)
                        out.write(b)
            except (http.client.IncompleteRead, ConnectionError, socket.timeout, TimeoutError) as e:
                if attempt < 4:
                    log(f"blob {digest}: {e}, restarting download ({attempt})")
                    time.sleep(attempt)
                    continue
                os.unlink(partial)
                raise DeriveError(f"blob {digest}: {e}") from e
            break
        actual = "sha256:" + h.hexdigest()
        if actual != digest or (size is not None and n != size):
            os.unlink(partial)
            raise DeriveError(f"blob digest mismatch from {url}: descriptor {digest} size {size}, "
                              f"received sha256:{h.hexdigest()} size {n}")
        os.replace(partial, dest)
        return dest


# ----------------------------------------------------------- the rootfs model

class Entry:
    __slots__ = ("kind", "mode", "uid", "gid", "size", "sha256", "content", "linkname", "layer")

    def __init__(self, kind, mode=0o755, uid=0, gid=0, size=0, sha256=None,
                 content=None, linkname=None, layer=-1):
        self.kind, self.mode, self.uid, self.gid = kind, mode, uid, gid
        self.size, self.sha256, self.content = size, sha256, content
        self.linkname, self.layer = linkname, layer

    def as_dict(self):
        d = {"kind": self.kind, "mode": f"{self.mode:04o}", "uid": self.uid, "gid": self.gid,
             "layer": self.layer}
        if self.kind == "file":
            d.update(size=self.size, sha256=self.sha256)
        if self.kind == "symlink":
            d["linkname"] = self.linkname
        return d


class Rootfs:
    """What a runtime's overlay snapshotter makes of layers applied in order.

    Mirrors containerd's archive.Apply over overlayfs: the PARENT of every
    entry is resolved through symlinks inside the root (fs.RootPath), missing
    parents are created 0755, an existing path is removed and replaced unless
    both are directories (then only metadata changes), a hardlink copies its
    target, `.wh.<name>` removes <name> and everything under it and
    `.wh..wh..opq` hides every lower entry of its directory — whiteouts apply
    to LOWER layers only (OCI image-spec layer.md), so they are applied before
    the layer's own entries.
    """

    MAX_CONTENT = 64 * 1024

    def __init__(self):
        self.entries = {"/": Entry("dir")}
        self.kids = {}

    # -- structure
    def _set(self, p, e):
        if p != "/":
            self.kids.setdefault(posixpath.dirname(p), set()).add(p)
        self.entries[p] = e

    def _remove_all(self, p):
        if p == "/" or p not in self.entries:
            return 0
        n, stack = 0, [p]
        while stack:
            q = stack.pop()
            if self.entries.pop(q, None) is not None:
                n += 1
            stack.extend(self.kids.pop(q, ()))
        self.kids.get(posixpath.dirname(p), set()).discard(p)
        return n

    def resolve(self, path):
        """Resolve every component through symlinks, clamped at the root."""
        comps = [c for c in path.split("/") if c and c != "."]
        out, links = "/", 0
        while comps:
            c = comps.pop(0)
            if c == "..":
                out = posixpath.dirname(out)
                continue
            cand = posixpath.join(out, c)
            e = self.entries.get(cand)
            if e is not None and e.kind == "symlink":
                links += 1
                if links > 255:
                    raise ApplyError(f"too many levels of symlinks resolving {path}")
                if e.linkname.startswith("/"):
                    out = "/"
                comps = [x for x in e.linkname.split("/") if x and x != "."] + comps
                continue
            out = cand
        return out

    def _mkdir_all(self, d, layer):
        cur = "/"
        for c in [x for x in d.split("/") if x]:
            cur = posixpath.join(cur, c)
            e = self.entries.get(cur)
            if e is None:
                self._set(cur, Entry("dir", 0o755, 0, 0, layer=layer))
            elif e.kind != "dir":
                raise ApplyError(f"{cur} is a {e.kind}, not a directory")

    def lookup(self, path):
        """-> (entry, problem). Follows NO symlink: a symlink anywhere on the
        path is a problem, because a check through it proves nothing about the
        bytes a runtime resolves."""
        p = posixpath.normpath("/" + path.lstrip("/"))
        comps = [c for c in p.split("/") if c]
        cur = "/"
        e = self.entries["/"]
        for i, c in enumerate(comps):
            cur = posixpath.join(cur, c)
            e = self.entries.get(cur)
            if e is None:
                return None, f"{cur} is absent"
            if e.kind == "symlink":
                return None, f"{cur} is a symlink -> {e.linkname}"
            if i < len(comps) - 1 and e.kind != "dir":
                return None, f"{cur} is a {e.kind}, not a directory"
        return e, None

    # -- applying
    def apply_tar(self, stream, layer):
        """Apply one uncompressed layer tar read from `stream`. -> events."""
        whiteouts, staged, events = [], [], []
        try:
            tf = tarfile.open(fileobj=stream, mode="r|")
        except tarfile.ReadError as e:
            if getattr(stream, "n", None) == 0:
                return events  # a zero-byte layer
            raise ApplyError(f"layer {layer}: not a tar: {e}") from e
        try:
            for ti in tf:
                p = posixpath.normpath("/" + ti.name.lstrip("/"))
                if p == "/":
                    continue
                base = posixpath.basename(p)
                if base.startswith(".wh."):
                    whiteouts.append((posixpath.dirname(p), base))
                    continue
                if ti.isreg():
                    h, content, f = hashlib.sha256(), bytearray(), tf.extractfile(ti)
                    for b in iter(lambda: f.read(CHUNK), b""):
                        h.update(b)
                        if ti.size <= self.MAX_CONTENT:
                            content += b
                    e = Entry("file", ti.mode & 0o7777, ti.uid, ti.gid, ti.size, h.hexdigest(),
                              bytes(content) if ti.size <= self.MAX_CONTENT else None, layer=layer)
                elif ti.isdir():
                    e = Entry("dir", ti.mode & 0o7777, ti.uid, ti.gid, layer=layer)
                elif ti.issym():
                    e = Entry("symlink", ti.mode & 0o7777, ti.uid, ti.gid, linkname=ti.linkname, layer=layer)
                elif ti.islnk():
                    e = Entry("hardlink", ti.mode & 0o7777, ti.uid, ti.gid, linkname=ti.linkname, layer=layer)
                else:
                    e = Entry("other", ti.mode & 0o7777, ti.uid, ti.gid, layer=layer)
                staged.append((p, e))
        except (tarfile.TarError, EOFError, OSError, zlib.error) as ex:
            raise ApplyError(f"layer {layer}: {type(ex).__name__}: {ex}") from ex
        finally:
            tf.close()

        for d, base in whiteouts:
            real_d = self.resolve(d)
            self._mkdir_all(real_d, layer)
            if base == ".wh..wh..opq":
                removed = sum(self._remove_all(k) for k in list(self.kids.get(real_d, ())))
                events.append({"event": "opaque", "path": real_d, "removed": removed})
            else:
                target = posixpath.join(real_d, base[len(".wh."):])
                events.append({"event": "whiteout", "path": target, "removed": self._remove_all(target)})

        for p, e in staged:
            parent = posixpath.dirname(p)
            real_parent = self.resolve(parent)
            rp = posixpath.join(real_parent, posixpath.basename(p))
            if real_parent != parent:
                events.append({"event": "parent-via-symlink", "path": p, "resolved": rp})
            self._mkdir_all(real_parent, layer)
            if e.kind == "hardlink":
                target = self.resolve(posixpath.normpath("/" + e.linkname.lstrip("/")))
                te = self.entries.get(target)
                if te is None or te.kind in ("dir", "symlink"):
                    raise ApplyError(f"layer {layer}: hardlink {p} -> {e.linkname}: target is "
                                     f"{'absent' if te is None else 'a ' + te.kind}")
                e = Entry(te.kind, te.mode, te.uid, te.gid, te.size, te.sha256, te.content,
                          te.linkname, layer=layer)
            existing = self.entries.get(rp)
            if existing is not None:
                if existing.kind == "dir" and e.kind == "dir":
                    existing.mode, existing.uid, existing.gid, existing.layer = e.mode, e.uid, e.gid, layer
                    continue
                removed = self._remove_all(rp)
                if existing.kind != e.kind:
                    events.append({"event": "type-change", "path": rp, "from": existing.kind,
                                   "to": e.kind, "removed": removed})
            self._set(rp, e)
        return events

    def apply_blob(self, fileobj, layer):
        """Sniff compression, decompress, apply, and hash the uncompressed
        stream. -> (compression, 'sha256:<diff_id>', events)."""
        magic = fileobj.read(4)
        fileobj.seek(0)
        if magic[:2] == b"\x1f\x8b":
            kind, raw = "gzip", gzip.GzipFile(fileobj=fileobj, mode="rb")
        elif magic == b"\x28\xb5\x2f\xfd":
            raise ApplyError(f"layer {layer}: zstd-compressed; the python stdlib cannot decompress it")
        else:
            kind, raw = "tar", fileobj
        hr = HashReader(raw)
        try:
            events = self.apply_tar(hr, layer)
            hr.drain()
        except (OSError, EOFError, zlib.error) as ex:
            raise ApplyError(f"layer {layer}: {type(ex).__name__}: {ex}") from ex
        return kind, "sha256:" + hr.hexdigest(), events


# -------------------------------------------------------------------- verify

class Report:
    _RANK = {"pass": 0, "skip": 1, "fail": 2}

    def __init__(self):
        self.status, self.errors, self.notes = {}, [], []

    def _mark(self, check, level):
        """A check's status only ever gets worse: pass < skip < fail."""
        cur = self.status.get(check)
        if cur is None or self._RANK[level] > self._RANK[cur]:
            self.status[check] = level

    def passed(self, check):
        self._mark(check, "pass")

    def failed(self, check, msg):
        self._mark(check, "fail")
        self.errors.append(f"{check}: {msg}")

    def skipped(self, check, why):
        self._mark(check, "skip")
        self.notes.append(f"{check} skipped: {why}")

    @property
    def ok(self):
        return bool(self.status) and all(v == "pass" for v in self.status.values())

    def failed_checks(self):
        return sorted(k for k, v in self.status.items() if v == "fail")

    def as_dict(self):
        return {"ok": self.ok, "checks": dict(self.status), "errors": self.errors, "notes": self.notes}


class Verification:
    def __init__(self):
        self.report = Report()
        self.fs = None
        self.info = {"format": None, "name": None, "manifest_digest": None, "config_digest": None,
                     "layers": [], "diff_ids": [], "top_layer_events": [], "expect": {}}

    def as_dict(self):
        return {**self.report.as_dict(), **self.info}


def verify_archive(path, expect=None, platform=None, strict_top=False):
    """Read an image archive the way containerd's importer would and check it.
    `expect` maps an image path to the sha256 its TOPMOST version must have."""
    v = Verification()
    rep = v.report
    expect = dict(expect or {})
    want = parse_platform(platform) if isinstance(platform, str) else platform
    downstream = ["config", "platform", "diff-id-count", "layer-media-type", "layer-apply",
                  "diff-id"] + (["expect"] if expect else []) + (["top-layer"] if strict_top else [])

    def skip_rest(why, checks=None):
        for c in checks or downstream:
            rep.skipped(c, why)

    try:
        tf = tarfile.open(path, "r:")
    except (tarfile.TarError, OSError) as e:
        rep.failed("archive", f"cannot read {path} as an uncompressed tar: {e}")
        skip_rest("archive unreadable", ["format", "index", "blob-present", "blob-digest"] + downstream)
        return v
    with tf:
        members = {}
        for ti in tf.getmembers():
            if not ti.isreg():
                continue
            n = posixpath.normpath(ti.name)
            if n in members:
                rep.failed("archive", f"duplicate member {n}")
            members[n] = ti
        rep.passed("archive")
        hashes = {}

        def member_sha(name):
            if name not in hashes:
                h, f = hashlib.sha256(), tf.extractfile(members[name])
                for b in iter(lambda: f.read(CHUNK), b""):
                    h.update(b)
                hashes[name] = "sha256:" + h.hexdigest()
            return hashes[name]

        def member_json(name, what):
            ti = members.get(name)
            if ti is None:
                return None, f"{what}: {name} is not in the archive"
            if ti.size > MAX_JSON:
                return None, f"{what}: {name} is larger than {MAX_JSON} bytes"
            try:
                return json.loads(tf.extractfile(ti).read()), None
            except ValueError as e:
                return None, f"{what}: {name} is not JSON: {e}"

        def check_blob(name, digest, size, what):
            if name not in members:
                rep.failed("blob-present", f"{what}: {name} is not in the archive")
                return False
            rep.passed("blob-present")
            if digest is None:
                v.report.notes.append(f"{what}: {name} carries no digest; its integrity rests on diff-id")
                return True
            if size is not None and members[name].size != size:
                rep.failed("blob-digest", f"{what}: {name} is {members[name].size} bytes, descriptor says {size}")
                return False
            actual = member_sha(name)
            if actual != digest:
                rep.failed("blob-digest", f"{what}: {name} hashes to {actual}, expected {digest}")
                return False
            rep.passed("blob-digest")
            return True

        # -- format, index, manifest: -> (config name/digest/size, layers [(name, digest, size, mt)])
        if "oci-layout" in members:
            v.info["format"] = "oci"
            layout, err = member_json("oci-layout", "oci-layout")
            if err or not isinstance(layout, dict) or layout.get("imageLayoutVersion") != "1.0.0":
                rep.failed("format", err or f"oci-layout version {layout!r} is not 1.0.0")
                skip_rest("format", ["index", "image-name", "blob-present", "blob-digest"] + downstream)
                return v
            index, err = member_json("index.json", "index")
            if err or not isinstance(index, dict):
                rep.failed("format", err or "index.json is not an object")
                skip_rest("format", ["index", "image-name", "blob-present", "blob-digest"] + downstream)
                return v
            rep.passed("format")
            descs = index.get("manifests") or []
            if len(descs) != 1:
                rep.failed("index", f"index.json lists {len(descs)} manifests, expected exactly 1")
                skip_rest("index", ["image-name", "blob-present", "blob-digest"] + downstream)
                return v
            d = descs[0]
            if d.get("mediaType") not in MANIFEST_TYPES or not DIGEST_RE.match(str(d.get("digest"))):
                rep.failed("index", f"index entry is not an image manifest descriptor: {d!r}")
                skip_rest("index", ["image-name", "blob-present", "blob-digest"] + downstream)
                return v
            rep.passed("index")
            ann = d.get("annotations") or {}
            name = ann.get(ANN_CONTAINERD_NAME)
            v.info["name"] = name
            try:
                r = Ref.parse(name) if name else None
            except DeriveError:
                r = None
            if not name:
                rep.failed("image-name", f"no {ANN_CONTAINERD_NAME} annotation: ctr would name the image "
                                         f"import-<date>:{ann.get(ANN_OCI_REF_NAME)}")
            elif r is None or r.full() != name or not r.tag:
                rep.failed("image-name", f"{name!r} is not a full, tagged, normalized reference; ctr stores it "
                                         f"verbatim and the CRI looks up the normalized name")
            else:
                rep.passed("image-name")
            m = DIGEST_RE.match(d["digest"])
            if not check_blob(f"blobs/sha256/{m.group(1)}", d["digest"], d.get("size"), "manifest"):
                skip_rest("manifest blob failed")
                return v
            v.info["manifest_digest"] = d["digest"]
            man, err = member_json(f"blobs/sha256/{m.group(1)}", "manifest")
            if err or not isinstance(man, dict) or man.get("schemaVersion") != 2:
                rep.failed("index", err or "manifest is not a schemaVersion 2 object")
                skip_rest("manifest unreadable")
                return v
            if man.get("mediaType") and man["mediaType"] != d["mediaType"]:
                rep.failed("index", f"manifest mediaType {man['mediaType']} != descriptor {d['mediaType']}")
            index_platform = d.get("platform")
            cd = man.get("config") or {}
            cm = DIGEST_RE.match(str(cd.get("digest")))
            if not cm:
                rep.failed("config", f"manifest config descriptor has no sha256 digest: {cd!r}")
                skip_rest("no config digest")
                return v
            config_ref = (f"blobs/sha256/{cm.group(1)}", cd["digest"], cd.get("size"))
            layer_refs = []
            for i, ld in enumerate(man.get("layers") or []):
                lm = DIGEST_RE.match(str(ld.get("digest")))
                if not lm:
                    rep.failed("blob-present", f"layer {i} descriptor has no sha256 digest: {ld!r}")
                    layer_refs.append((None, None, None, ld.get("mediaType")))
                else:
                    layer_refs.append((f"blobs/sha256/{lm.group(1)}", ld["digest"], ld.get("size"),
                                       ld.get("mediaType")))
        elif "manifest.json" in members:
            v.info["format"] = "docker"
            mfst, err = member_json("manifest.json", "manifest.json")
            if err or not isinstance(mfst, list):
                rep.failed("format", err or "manifest.json is not a list")
                skip_rest("format", ["index", "image-name", "blob-present", "blob-digest"] + downstream)
                return v
            rep.passed("format")
            if len(mfst) != 1 or not isinstance(mfst[0], dict):
                rep.failed("index", f"manifest.json lists {len(mfst)} images, expected exactly 1")
                skip_rest("index", ["image-name", "blob-present", "blob-digest"] + downstream)
                return v
            rep.passed("index")
            e0 = mfst[0]
            tags = e0.get("RepoTags") or []
            v.info["name"] = tags[0] if tags else None
            bad = []
            for t in tags:
                try:
                    if not Ref.parse(t).tag:
                        bad.append(t)
                except DeriveError:
                    bad.append(t)
            if not tags or bad:
                rep.failed("image-name", f"RepoTags {tags!r} (unparseable or untagged: {bad!r})")
            else:
                rep.passed("image-name")
            index_platform = None
            cname = posixpath.normpath(str(e0.get("Config")))
            cm = BLOB_NAME_RE.match(cname) or DOCKER_CFG_NAME_RE.match(cname)
            config_ref = (cname, f"sha256:{cm.group(1)}" if cm else None, None)
            layer_refs = []
            for ln in e0.get("Layers") or []:
                ln = posixpath.normpath(str(ln))
                lm = BLOB_NAME_RE.match(ln)
                layer_refs.append((ln, f"sha256:{lm.group(1)}" if lm else None, None, None))
        else:
            rep.failed("format", "neither oci-layout nor manifest.json: containerd says 'unrecognized image format'")
            skip_rest("format", ["index", "image-name", "blob-present", "blob-digest"] + downstream)
            return v

        # -- config
        cfg_ok = check_blob(*config_ref, "config")
        v.info["config_digest"] = config_ref[1]
        layer_ok = []
        for i, (ln, ld, ls, lmt) in enumerate(layer_refs):
            layer_ok.append(ln is not None and check_blob(ln, ld, ls, f"layer {i}"))
            v.info["layers"].append({"name": ln, "digest": ld, "size": ls, "media_type": lmt})
        if not cfg_ok:
            skip_rest("config blob failed")
            return v
        cfg, err = member_json(config_ref[0], "config")
        rootfs = cfg.get("rootfs") if isinstance(cfg, dict) else None
        if err or not isinstance(rootfs, dict) or rootfs.get("type") != "layers" \
                or not isinstance(rootfs.get("diff_ids"), list):
            rep.failed("config", err or f"config rootfs is not {{type: layers, diff_ids: [...]}}: {rootfs!r}")
            skip_rest("config unreadable", [c for c in downstream if c != "config"])
            return v
        rep.passed("config")
        diff_ids = rootfs["diff_ids"]
        v.info["diff_ids"] = list(diff_ids)
        cfg_platform = {"os": cfg.get("os"), "architecture": cfg.get("architecture")}
        if cfg.get("variant"):
            cfg_platform["variant"] = cfg["variant"]
        if index_platform and not platform_matches(cfg_platform, index_platform):
            rep.failed("platform", f"config is {platform_str(cfg_platform)}, index says {platform_str(index_platform)}")
        elif want and not platform_matches(cfg_platform, want):
            rep.failed("platform", f"config is {platform_str(cfg_platform)}, wanted {platform_str(want)}")
        else:
            rep.passed("platform")
        if len(diff_ids) != len(layer_refs):
            rep.failed("diff-id-count", f"config has {len(diff_ids)} diff_ids for {len(layer_refs)} layers")
        else:
            rep.passed("diff-id-count")

        # -- apply the layers in order, as the snapshotter would
        fs, broken = Rootfs(), None
        for i, (ln, ld, ls, lmt) in enumerate(layer_refs):
            if not layer_ok[i]:
                broken = f"layer {i} unavailable"
                break
            f = tf.extractfile(members[ln])
            magic = f.read(2)
            f.seek(0)
            if lmt in LAYER_GZIP_TYPES and magic != b"\x1f\x8b":
                rep.failed("layer-media-type", f"layer {i} declared {lmt} but is not gzip")
            elif lmt in LAYER_TAR_TYPES and magic == b"\x1f\x8b":
                rep.failed("layer-media-type", f"layer {i} declared {lmt} but is gzip")
            elif lmt is not None and lmt not in LAYER_GZIP_TYPES + LAYER_TAR_TYPES:
                rep.failed("layer-media-type", f"layer {i} has unsupported media type {lmt}")
            else:
                rep.passed("layer-media-type")
            try:
                _, diff, events = fs.apply_blob(f, i)
            except ApplyError as e:
                rep.failed("layer-apply", str(e))
                broken = f"layer {i} did not apply"
                break
            rep.passed("layer-apply")
            v.info["layers"][i]["diff_id"] = diff
            if i < len(diff_ids):
                if diff_ids[i] == diff:
                    rep.passed("diff-id")
                else:
                    rep.failed("diff-id", f"layer {i} ({ld or ln}) uncompressed is {diff}, config says {diff_ids[i]}")
            if i == len(layer_refs) - 1:
                v.info["top_layer_events"] = events
        if broken:
            skip_rest(broken, ["layer-media-type", "layer-apply", "diff-id"] + (["expect"] if expect else [])
                      + (["top-layer"] if strict_top else []))
            return v
        if not layer_refs:
            rep.failed("layer-apply", "the image has no layers")
        v.fs = fs

        for p, sha in expect.items():
            want_sha = sha.lower().removeprefix("sha256:")
            e, problem = fs.lookup(p)
            got = e.as_dict() if e is not None else {"problem": problem}
            v.info["expect"][p] = {"expected": want_sha, **got}
            if e is None:
                rep.failed("expect", f"{p}: {problem}")
            elif e.kind != "file":
                rep.failed("expect", f"{p} is a {e.kind}, not a regular file")
            elif e.sha256 != want_sha:
                rep.failed("expect", f"{p}: topmost version (layer {e.layer}) is sha256 {e.sha256}, expected {want_sha}")
            else:
                rep.passed("expect")
        if strict_top:
            if v.info["top_layer_events"]:
                rep.failed("top-layer", f"top layer events: {v.info['top_layer_events']!r}")
            else:
                rep.passed("top-layer")
    return v


# -------------------------------------------------------------------- derive

class Add:
    def __init__(self, path, src):
        self.path, self.src = path, src
        self.size = os.path.getsize(src)
        self.sha256 = sha256_file(src)
        self.replaces = None


def parse_add(s):
    if "=" not in s:
        raise DeriveError(f"--add {s!r}: want /path/in/image=<local file>")
    p, src = s.split("=", 1)
    if not p.startswith("/") or posixpath.normpath(p) != p or p == "/" or \
            any(c in (".", "..") for c in p.split("/")):
        raise DeriveError(f"--add {s!r}: the image path must be absolute and normalized")
    if posixpath.basename(p).startswith(".wh."):
        raise DeriveError(f"--add {s!r}: a .wh. name is a whiteout, not a file")
    if not os.path.isfile(src):
        raise DeriveError(f"--add {s!r}: {src} is not a regular file")
    return Add(p, src)


def parse_kv(items, flag):
    out = []
    for s in items or []:
        if "=" not in s or not s.split("=", 1)[0]:
            raise DeriveError(f"{flag} {s!r}: want K=V")
        out.append(tuple(s.split("=", 1)))
    return out


def source_date_epoch():
    s = os.environ.get("SOURCE_DATE_EPOCH")
    if s is None or s == "":
        return 0, None
    if not s.isdigit():
        raise DeriveError(f"SOURCE_DATE_EPOCH={s!r} is not a non-negative integer")
    t = int(s)
    return t, time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t))


def check_free(path, need, min_free_gb):
    free = shutil.disk_usage(path).free
    if free - need < min_free_gb * (1 << 30):
        raise DeriveError(f"{path}: {free / 2**30:.1f} GiB free; this needs {need / 2**30:.2f} GiB "
                          f"and the floor is {min_free_gb} GiB — refusing")


def pull_base(reg, ref, want, blobdir, out_dir, min_free_gb, add_bytes):
    mt, body, dgst, doc = reg.get_manifest(ref.reference)
    base = {"ref": ref.full(), "index_digest": None, "index_platform": None}
    if mt in INDEX_TYPES:
        base["index_digest"] = dgst
        cands, offered = [], []
        for m in doc.get("manifests") or []:
            ann = m.get("annotations") or {}
            offered.append(platform_str(m.get("platform")))
            if ann.get("vnd.docker.reference.type") == "attestation-manifest":
                continue
            if m.get("mediaType") in MANIFEST_TYPES and platform_matches(m.get("platform"), want):
                cands.append(m)
        if not cands:
            raise DeriveError(f"{ref.full()}: no manifest for {platform_str(want)} "
                              f"(index offers: {', '.join(sorted(set(offered)))})")
        if len(cands) > 1:
            raise DeriveError(f"{ref.full()}: {len(cands)} manifests match {platform_str(want)}; ambiguous")
        pd = cands[0]
        base["index_platform"] = pd.get("platform")
        mt, body, dgst, doc = reg.get_manifest(pd["digest"])
        if pd.get("size") is not None and len(body) != pd["size"]:
            raise DeriveError(f"{ref.full()}: manifest {pd['digest']} is {len(body)} bytes, index says {pd['size']}")
        if mt not in MANIFEST_TYPES:
            raise DeriveError(f"{ref.full()}: index entry {pd['digest']} is {mt}, not an image manifest")
    elif mt not in MANIFEST_TYPES:
        raise DeriveError(f"{ref.full()}: unsupported manifest media type {mt}")
    base.update(manifest_digest=dgst, manifest_media_type=mt, manifest=doc)

    cd = doc.get("config") or {}
    layers = doc.get("layers") or []
    for i, ld in enumerate(layers):
        if ld.get("urls"):
            raise DeriveError(f"{ref.full()}: layer {i} is a foreign layer (urls); refusing")
        if ld.get("mediaType") not in LAYER_GZIP_TYPES + LAYER_TAR_TYPES:
            raise DeriveError(f"{ref.full()}: layer {i} media type {ld.get('mediaType')!r} unsupported "
                              f"(gzip or uncompressed tar only; zstd cannot be verified with the stdlib)")
    total = sum(int(ld.get("size") or 0) for ld in layers) + int(cd.get("size") or 0)
    work_need, out_need = total + 2 * add_bytes, total + add_bytes
    same_dev = os.stat(blobdir).st_dev == os.stat(out_dir).st_dev
    check_free(blobdir, work_need + (out_need if same_dev else 0), min_free_gb)
    if not same_dev:
        check_free(out_dir, out_need, min_free_gb)
    log(f"base {ref.full()} {platform_str(want)}: manifest {dgst}, {len(layers)} layers, "
        f"{total / 2**20:.1f} MiB to fetch")

    cpath = reg.fetch_blob(cd.get("digest"), cd.get("size"), blobdir)
    with open(cpath, "rb") as f:
        cfg = json.loads(f.read(MAX_JSON))
    cfg_platform = {"os": cfg.get("os"), "architecture": cfg.get("architecture")}
    if cfg.get("variant"):
        cfg_platform["variant"] = cfg["variant"]
    if not platform_matches(cfg_platform, want):
        raise DeriveError(f"{ref.full()}: config says {platform_str(cfg_platform)}, wanted {platform_str(want)}")
    diff_ids = (cfg.get("rootfs") or {}).get("diff_ids")
    if (cfg.get("rootfs") or {}).get("type") != "layers" or not isinstance(diff_ids, list) \
            or len(diff_ids) != len(layers):
        raise DeriveError(f"{ref.full()}: config rootfs {cfg.get('rootfs')!r} does not match {len(layers)} layers")
    paths = []
    for i, ld in enumerate(layers):
        paths.append(reg.fetch_blob(ld["digest"], ld.get("size"), blobdir))
        log(f"  layer {i + 1}/{len(layers)} {ld['digest'][:19]} {int(ld.get('size') or 0) / 2**20:.1f} MiB ok")
    base.update(config=cfg, config_digest=cd["digest"], config_media_type=cd.get("mediaType"),
                config_path=cpath, layer_paths=paths)
    return base


def scan_base(base):
    fs = Rootfs()
    diff_ids = base["config"]["rootfs"]["diff_ids"]
    for i, (ld, p) in enumerate(zip(base["manifest"]["layers"], base["layer_paths"])):
        with open(p, "rb") as f:
            try:
                _, diff, _ = fs.apply_blob(f, i)
            except ApplyError as e:
                raise DeriveError(f"base layer {i} ({ld['digest']}): {e}") from e
        if diff != diff_ids[i]:
            raise DeriveError(f"base layer {i} ({ld['digest']}): uncompressed sha256 is {diff}, "
                              f"config diff_id is {diff_ids[i]}; the base image is inconsistent")
    return fs


def precheck_adds(adds, fs):
    targets = {a.path for a in adds}
    for a in adds:
        d = posixpath.dirname(a.path)
        ancestors = []
        while d != "/":
            ancestors.append(d)
            d = posixpath.dirname(d)
        for q in reversed(ancestors):
            if q in targets:
                raise DeriveError(f"--add {a.path}: {q} is itself added as a file")
            e = fs.entries.get(q)
            if e is not None and e.kind != "dir":
                what = f"a symlink -> {e.linkname}" if e.kind == "symlink" else f"a {e.kind}"
                raise DeriveError(f"--add {a.path}: {q} is {what} in the base; the layer's directory entry "
                                  f"would REPLACE it and hide what it holds. Add the file at its resolved path.")
        e = fs.entries.get(a.path)
        if e is not None and e.kind != "file":
            what = f"a symlink -> {e.linkname}" if e.kind == "symlink" else f"a {e.kind}"
            raise DeriveError(f"--add {a.path}: it is {what} in the base; refusing to replace it with a file")
        if e is not None:
            a.replaces = {"sha256": e.sha256, "size": e.size, "mode": f"{e.mode:04o}", "layer": e.layer}


def build_layer(adds, fs, mtime, workdir):
    tar_path = os.path.join(workdir, "new-layer.tar")
    gz_path = tar_path + ".gz"
    dirs = set()
    for a in adds:
        d = posixpath.dirname(a.path)
        while d != "/":
            dirs.add(d)
            d = posixpath.dirname(d)
    with open(tar_path, "wb") as raw:
        tw = HashWriter(raw)
        with tarfile.open(fileobj=tw, mode="w", format=tarfile.PAX_FORMAT) as tf:
            for d in sorted(dirs):
                ti = tarfile.TarInfo(d.lstrip("/"))
                ti.type = tarfile.DIRTYPE
                be = fs.entries.get(d) if fs is not None else None
                if be is not None and be.kind == "dir":
                    ti.mode, ti.uid, ti.gid = be.mode, be.uid, be.gid
                else:
                    ti.mode, ti.uid, ti.gid = 0o755, 0, 0
                ti.mtime, ti.uname, ti.gname = mtime, "", ""
                tf.addfile(ti)
            for a in sorted(adds, key=lambda x: x.path):
                ti = tarfile.TarInfo(a.path.lstrip("/"))
                ti.size, ti.mode, ti.uid, ti.gid = a.size, 0o755, 0, 0
                ti.mtime, ti.uname, ti.gname = mtime, "", ""
                with open(a.src, "rb") as f:
                    hr = HashReader(f)
                    tf.addfile(ti, hr)
                if hr.hexdigest() != a.sha256 or os.path.getsize(a.src) != a.size:
                    raise DeriveError(f"--add {a.path}: {a.src} changed while it was being added")
    diff_id = "sha256:" + tw.hexdigest()
    with open(tar_path, "rb") as src, open(gz_path, "wb") as raw:
        gw = HashWriter(raw)
        with gzip.GzipFile(filename="", mode="wb", fileobj=gw, mtime=0, compresslevel=6) as gz:
            shutil.copyfileobj(src, gz, CHUNK)
    os.unlink(tar_path)
    return {"path": gz_path, "diff_id": diff_id, "digest": "sha256:" + gw.hexdigest(), "size": gw.n}


def jbytes(obj):
    return json.dumps(obj, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def write_archive(dest, members, mtime):
    """members: [(name, bytes | local path | None for a directory)]."""
    with open(dest, "wb") as raw, tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as tf:
        for name, src in members:
            ti = tarfile.TarInfo(name)
            ti.mtime, ti.uid, ti.gid, ti.uname, ti.gname = mtime, 0, 0, "", ""
            if src is None:
                ti.type, ti.mode = tarfile.DIRTYPE, 0o755
                tf.addfile(ti)
            elif isinstance(src, bytes):
                ti.mode, ti.size = 0o644, len(src)
                tf.addfile(ti, io.BytesIO(src))
            else:
                ti.mode, ti.size = 0o644, os.path.getsize(src)
                with open(src, "rb") as f:
                    tf.addfile(ti, f)


def derive(args):
    want = parse_platform(args.platform)
    base_ref, tag_ref = Ref.parse(args.base), Ref.parse(args.tag)
    if tag_ref.digest or not tag_ref.tag:
        raise DeriveError(f"--tag {args.tag!r}: needs an explicit tag and no digest")
    if not args.add:
        raise DeriveError("at least one --add is required")
    adds = [parse_add(s) for s in args.add]
    if len({a.path for a in adds}) != len(adds):
        raise DeriveError("--add: the same image path is given twice")
    envs, labels = parse_kv(args.env, "--env"), parse_kv(args.label, "--label")
    mtime, created = source_date_epoch()
    out = os.path.abspath(args.out)
    out_dir = os.path.dirname(out)
    if not os.path.isdir(out_dir):
        raise DeriveError(f"--out {out}: {out_dir} is not a directory")
    if args.work_dir:
        os.makedirs(args.work_dir, exist_ok=True)
    workdir = tempfile.mkdtemp(prefix="oci-derive-", dir=args.work_dir or None)
    blobdir = os.path.join(workdir, "blobs")
    os.makedirs(blobdir)
    partial = out + ".partial"
    try:
        reg = Registry(base_ref, plain_http=args.plain_http)
        base = pull_base(reg, base_ref, want, blobdir, out_dir, args.min_free_gb, sum(a.size for a in adds))
        fs = scan_base(base)
        precheck_adds(adds, fs)
        layer = build_layer(adds, fs, mtime, workdir)
        log(f"new layer {layer['digest']} ({layer['size']} bytes), diff_id {layer['diff_id']}")

        man_mt = base["manifest_media_type"]
        oci = man_mt == MT_OCI_MANIFEST
        layer_mt = MT_OCI_LAYER_GZIP if oci else MT_DOCKER_LAYER_GZIP
        cfg = copy.deepcopy(base["config"])
        cfg["rootfs"]["diff_ids"].append(layer["diff_id"])
        if created:
            cfg["created"] = created
        if isinstance(cfg.get("history"), list):
            cfg["history"].append({
                "created": created or base["config"].get("created") or "1970-01-01T00:00:00Z",
                "created_by": "oci_derive.py " + " ".join(f"ADD {a.path} sha256:{a.sha256}"
                                                          for a in sorted(adds, key=lambda x: x.path)),
                "comment": f"one layer over {base_ref.full()}@{base['manifest_digest']}",
            })
        c = cfg.setdefault("config", {})
        if envs:
            env = list(c.get("Env") or [])
            for k, val in envs:
                env = [e for e in env if not e.startswith(k + "=")] + [f"{k}={val}"]
            c["Env"] = env
        if labels:
            lab = dict(c.get("Labels") or {})
            lab.update(labels)
            c["Labels"] = lab
        cfg_bytes = jbytes(cfg)
        cfg_digest = "sha256:" + hashlib.sha256(cfg_bytes).hexdigest()

        base_layers = base["manifest"]["layers"]
        man = {"schemaVersion": 2, "mediaType": man_mt,
               "config": {"mediaType": base["config_media_type"] or (MT_OCI_CONFIG if oci else MT_DOCKER_CONFIG),
                          "digest": cfg_digest, "size": len(cfg_bytes)},
               "layers": [{k: ld[k] for k in ("mediaType", "digest", "size", "annotations") if k in ld}
                          for ld in base_layers]
                         + [{"mediaType": layer_mt, "digest": layer["digest"], "size": layer["size"]}]}
        if oci:
            man["annotations"] = {"org.opencontainers.image.base.name": base_ref.full(),
                                  "org.opencontainers.image.base.digest": base["manifest_digest"]}
        man_bytes = jbytes(man)
        man_digest = "sha256:" + hashlib.sha256(man_bytes).hexdigest()

        blob_members, seen = [], set()

        def add_blob(digest, src):
            if digest not in seen:
                seen.add(digest)
                blob_members.append((f"blobs/sha256/{DIGEST_RE.match(digest).group(1)}", src))

        layer_srcs = [(ld["digest"], p) for ld, p in zip(base_layers, base["layer_paths"])]
        layer_srcs.append((layer["digest"], layer["path"]))
        if args.format == "oci":
            plat = dict(base["index_platform"] or want)
            index = {"schemaVersion": 2, "mediaType": MT_OCI_INDEX,
                     "manifests": [{"mediaType": man_mt, "digest": man_digest, "size": len(man_bytes),
                                    "platform": plat,
                                    "annotations": {ANN_CONTAINERD_NAME: tag_ref.full(),
                                                    ANN_OCI_REF_NAME: tag_ref.tag}}]}
            add_blob(man_digest, man_bytes)
            add_blob(cfg_digest, cfg_bytes)
            for d, src in layer_srcs:
                add_blob(d, src)
            members = [("oci-layout", jbytes({"imageLayoutVersion": "1.0.0"})),
                       ("index.json", jbytes(index)), ("blobs", None), ("blobs/sha256", None)] + blob_members
        else:
            add_blob(cfg_digest, cfg_bytes)
            for d, src in layer_srcs:
                add_blob(d, src)
            mj = [{"Config": f"blobs/sha256/{DIGEST_RE.match(cfg_digest).group(1)}",
                   "RepoTags": [tag_ref.full()],
                   "Layers": [f"blobs/sha256/{DIGEST_RE.match(d).group(1)}" for d, _ in layer_srcs]}]
            members = [("manifest.json", jbytes(mj)), ("blobs", None), ("blobs/sha256", None)] + blob_members

        write_archive(partial, members, mtime)
        ver = verify_archive(partial, expect={a.path: a.sha256 for a in adds}, platform=want, strict_top=True)
        rep = ver.report
        if not rep.ok:
            raise DeriveError("self-verification of the output FAILED: " + "; ".join(rep.errors + rep.notes))
        got = ver.info["diff_ids"]
        if got[:-1] != base["config"]["rootfs"]["diff_ids"] or got[-1] != layer["diff_id"]:
            raise DeriveError(f"self-verification: diff_ids {got} are not base + new layer")
        for a in adds:
            e, _ = ver.fs.lookup(a.path)
            if (e.mode, e.uid, e.gid, e.layer) != (0o755, 0, 0, len(got) - 1):
                raise DeriveError(f"self-verification: {a.path} is {e.as_dict()}, not 0755 root in the top layer")
        os.replace(partial, out)
        summary = {
            "base": {"ref": base_ref.full(), "platform": platform_str(want),
                     "index_digest": base["index_digest"], "manifest_digest": base["manifest_digest"],
                     "manifest_media_type": man_mt, "config_digest": base["config_digest"],
                     "layers": len(base_layers)},
            "image": {"ref": tag_ref.full(), "format": args.format,
                      "manifest_digest": man_digest if args.format == "oci" else None,
                      "manifest_digest_note": None if args.format == "oci" else
                      "docker layout: containerd writes its own manifest at import; read it with ctr images ls",
                      "config_digest": cfg_digest, "layers": len(got)},
            "diff_ids": got,
            "new_layer": {"digest": layer["digest"], "diff_id": layer["diff_id"], "size": layer["size"],
                          "media_type": layer_mt, "mtime": mtime},
            "added": [{"path": a.path, "source": os.path.abspath(a.src), "sha256": a.sha256, "size": a.size,
                       "mode": "0755", "replaces": a.replaces} for a in sorted(adds, key=lambda x: x.path)],
            "out": {"path": out, "size": os.path.getsize(out), "sha256": sha256_file(out)},
            "verify": rep.as_dict(),
            "import": f"ctr -n k8s.io images import --platform {platform_str(want)} {out}",
            "verify_on_node": [f"verify_image.sh {tag_ref.full()} {a.path} {a.sha256}"
                               for a in sorted(adds, key=lambda x: x.path)],
        }
        return summary
    finally:
        if os.path.exists(partial):
            os.unlink(partial)
        if args.keep_work:
            log(f"work dir kept: {workdir}")
        else:
            shutil.rmtree(workdir, ignore_errors=True)


# ----------------------------------------------------------------------- CLI

def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv and argv[0] == "verify":
        ap = argparse.ArgumentParser(prog="oci_derive.py verify",
                                     description="Verify an image archive the way containerd would read it.")
        ap.add_argument("archive")
        ap.add_argument("--expect", action="append", default=[], metavar="PATH=SHA256",
                        help="the topmost version of PATH must have this sha256 (repeatable)")
        ap.add_argument("--platform", help="the config must be this os/arch[/variant]")
        ap.add_argument("--strict-top-layer", action="store_true",
                        help="the top layer may not change an entry's type, white out, or write via a symlink")
        args = ap.parse_args(argv[1:])
        try:
            expect = dict(parse_kv(args.expect, "--expect"))
            v = verify_archive(args.archive, expect=expect, platform=args.platform,
                               strict_top=args.strict_top_layer)
        except DeriveError as e:
            print(f"oci_derive: {e}", file=sys.stderr)
            return 2
        print(json.dumps(v.as_dict(), indent=2))
        return 0 if v.report.ok else 1

    if argv and argv[0] == "derive":
        argv = argv[1:]
    ap = argparse.ArgumentParser(prog="oci_derive.py", description=__doc__.split("\n\n")[0],
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base", required=True, help="published image, e.g. dilipdalton/flint-s3-worker-lean:1.51.0")
    ap.add_argument("--platform", default="linux/amd64")
    ap.add_argument("--add", action="append", default=[], metavar="/PATH=LOCAL_FILE", required=True)
    ap.add_argument("--tag", required=True, help="name of the derived image (normalized to docker.io/...)")
    ap.add_argument("--out", required=True, help="the archive to write")
    ap.add_argument("--format", choices=("oci", "docker"), default="oci")
    ap.add_argument("--env", action="append", default=[], metavar="K=V")
    ap.add_argument("--label", action="append", default=[], metavar="K=V")
    ap.add_argument("--work-dir", help="parent of the temporary blob dir (removed at exit)")
    ap.add_argument("--keep-work", action="store_true")
    ap.add_argument("--plain-http", action="store_true", help="talk http, not https, to the registry")
    ap.add_argument("--min-free-gb", type=float, default=5.0,
                    help="refuse to download if free space would fall under this (default 5)")
    args = ap.parse_args(argv)
    try:
        summary = derive(args)
    except DeriveError as e:
        print(f"oci_derive: REFUSED: {e}", file=sys.stderr)
        return 1
    print(json.dumps(summary, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
