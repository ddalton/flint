#!/usr/bin/env python3
"""mock-rig.py — the F29 restage drill's two fake dependencies in one
process, run as root inside the Lima VM.

1. A mock spdk-tgt RPC socket (newline-delimited JSON-RPC 2.0, the
   spdk_native.rs framing). Only the ublk family is materialized:
   `ublk_start_disk` attaches a loop device over the bdev's backing
   file and symlinks /dev/ublkb<id> to it, so the driver's staging
   path (wipefs/blkid/mkfs/mount) runs against a REAL block device
   whose bytes persist across device teardown — the property the drill
   depends on, and the one thing spdk itself contributes to it. The
   F29 fix is driver orchestration; spdk semantics are untouched by
   it, which is why mocking here does not weaken the drill's verdict.
   `ublk_get_disks` answers from LIVE system state (symlink present +
   loop attached), so the drill's out-of-band teardowns are visible
   with no control channel. Unknown methods answer "Method not found",
   which every agent call site tolerates by design.

2. A fake Kubernetes API (plain HTTP) serving the ONE lookup the
   staging path hard-requires — the node-agent pod list, answered with
   podIP 127.0.0.1 so the driver's agent call loops back into its own
   process — plus the Node uid. Everything else 404s: every PV
   read/write on this path is non-fatal by design, and the drill's
   data-intact assertions prove the blkid signature guard covers the
   missing PV marker exactly as it must in production when the API is
   unreachable.

Fault injection: the JSON file named by $F29_FAULTS is re-read on
every RPC; {"ublk_start_disk": "error"} makes that method fail — the
drill's stage-cannot-succeed leg.
"""

import http.server
import json
import os
import re
import socketserver
import subprocess
import sys
import threading

RIG = os.environ.get("F29_RIG", "/var/tmp/f29rig")
SPDK_SOCK = os.environ.get("F29_SPDK_SOCK", RIG + "/spdk.sock")
HTTP_PORT = int(os.environ.get("F29_HTTP_PORT", "18811"))
FAULTS = os.environ.get("F29_FAULTS", RIG + "/faults.json")
NODE_NAME = os.environ.get("F29_NODE_NAME", "f29-node")
BDEV_BYTES = 512 * 1024 * 1024


def log(msg):
    print(msg, flush=True)


def faults():
    try:
        with open(FAULTS) as f:
            return json.load(f)
    except Exception:
        return {}


def backing_path(bdev_name):
    # One file per bdev name: the mock's stand-in for an lvol's
    # persistence. Sanitized so a hostile bdev name cannot escape RIG.
    safe = re.sub(r"[^A-Za-z0-9._-]", "_", bdev_name)
    return os.path.join(RIG, "bdev-%s.img" % safe)


def run(cmd):
    return subprocess.run(cmd, capture_output=True, text=True)


def loop_for(backing):
    out = run(["losetup", "-j", backing]).stdout.strip()
    # "/dev/loop3: [64769]:131077 (/var/tmp/f29rig/bdev-x.img)"
    return out.split(":", 1)[0] if out else None


def live_disks():
    disks = []
    for name in sorted(os.listdir("/dev")):
        if not name.startswith("ublkb"):
            continue
        dev = "/dev/" + name
        if not os.path.islink(dev):
            continue  # never touch a real ublk device
        target = os.path.realpath(dev)
        if not os.path.exists(target):
            os.unlink(dev)  # stale symlink: loop detached out-of-band
            continue
        out = run(["losetup", target]).stdout
        m = re.search(r"\((.*?)\)", out)
        if not m or os.path.dirname(m.group(1)) != RIG:
            continue
        bdev = re.sub(r"^bdev-|\.img$", "", os.path.basename(m.group(1)))
        disks.append({
            "id": int(name[len("ublkb"):]),
            "bdev_name": bdev,
            "ublk_device": dev,
        })
    return disks


def rpc_error(code, message):
    return {"error": {"code": code, "message": message}}


def handle_method(method, params):
    if faults().get(method) == "error":
        log("RPC %s -> INJECTED FAULT" % method)
        return rpc_error(-32602, "injected fault: %s refused by the rig" % method)

    if method == "ublk_create_target" or method == "ublk_destroy_target":
        return {"result": True}
    if method == "bdev_nvme_set_options":
        return {"result": True}
    if method == "spdk_get_version":
        return {"result": {"version": "f29-mock", "fields": {}}}
    if method == "bdev_get_bdevs":
        return {"result": []}
    if method == "ublk_get_disks":
        return {"result": live_disks()}
    if method == "ublk_start_disk":
        ublk_id = params.get("ublk_id")
        bdev = params.get("bdev_name", "")
        backing = backing_path(bdev)
        if not os.path.exists(backing):
            with open(backing, "wb") as f:
                f.truncate(BDEV_BYTES)
        loop = loop_for(backing)
        if loop is None:
            r = run(["losetup", "--find", "--show", backing])
            if r.returncode != 0:
                return rpc_error(-32602, "losetup failed: %s" % r.stderr.strip())
            loop = r.stdout.strip()
        dev = "/dev/ublkb%d" % ublk_id
        tmp = dev + ".tmp"
        os.symlink(loop, tmp)
        os.replace(tmp, dev)
        log("RPC ublk_start_disk id=%s bdev=%s -> %s (%s)" % (ublk_id, bdev, dev, loop))
        return {"result": dev}
    if method == "ublk_stop_disk":
        ublk_id = params.get("ublk_id")
        dev = "/dev/ublkb%s" % ublk_id
        if not os.path.islink(dev):
            return rpc_error(-19, "No such device")
        loop = os.path.realpath(dev)
        run(["losetup", "-d", loop])
        os.unlink(dev)
        log("RPC ublk_stop_disk id=%s -> detached %s" % (ublk_id, loop))
        return {"result": True}

    return rpc_error(-32601, "Method not found")


class SpdkRpc(socketserver.StreamRequestHandler):
    def handle(self):
        while True:
            line = self.rfile.readline()
            if not line:
                return
            try:
                req = json.loads(line)
            except Exception:
                return
            method = req.get("method", "")
            params = req.get("params") or {}
            resp = handle_method(method, params)
            resp.setdefault("jsonrpc", "2.0")
            resp["id"] = req.get("id", 1)
            self.wfile.write((json.dumps(resp) + "\n").encode())
            self.wfile.flush()


class ThreadingUnixServer(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True


class FakeK8s(http.server.BaseHTTPRequestHandler):
    def _json(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        log("K8S %s" % (fmt % args))

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        m = re.fullmatch(r"/api/v1/namespaces/([^/]+)/pods", path)
        if m:
            ns = m.group(1)
            return self._json(200, {
                "kind": "PodList", "apiVersion": "v1",
                "metadata": {"resourceVersion": "1"},
                "items": [{
                    "metadata": {
                        "name": "flint-csi-node-drill", "namespace": ns,
                        "uid": "f29-pod-uid", "resourceVersion": "1",
                        "labels": {"app": "flint-csi-node"},
                    },
                    "spec": {"nodeName": NODE_NAME},
                    "status": {"podIP": "127.0.0.1", "phase": "Running"},
                }],
            })
        m = re.fullmatch(r"/api/v1/nodes/([^/]+)", path)
        if m:
            return self._json(200, {
                "kind": "Node", "apiVersion": "v1",
                "metadata": {"name": m.group(1), "uid": "f29-node-uid",
                             "resourceVersion": "1"},
            })
        if path == "/version":
            return self._json(200, {"major": "1", "minor": "31",
                                    "gitVersion": "v1.31.0-f29-mock"})
        return self._json(404, {
            "kind": "Status", "apiVersion": "v1", "status": "Failure",
            "reason": "NotFound", "code": 404,
            "message": "f29 mock: %s not served" % path,
        })

    # PV annotation patches and everything else: NotFound. Their call
    # sites are non-fatal, and proving that is part of the drill.
    do_PATCH = do_GET
    do_POST = do_GET
    do_PUT = do_GET


def main():
    os.makedirs(RIG, exist_ok=True)
    if os.path.exists(SPDK_SOCK):
        os.unlink(SPDK_SOCK)
    rpc = ThreadingUnixServer(SPDK_SOCK, SpdkRpc)
    os.chmod(SPDK_SOCK, 0o777)
    # allow_reuse_address: without it, TIME_WAIT residue from the
    # previous drill run refuses the bind and the whole rig dies with
    # the unix socket already created — every connection then reads as
    # "Connection refused" from the driver's side.
    socketserver.ThreadingTCPServer.allow_reuse_address = True
    api = socketserver.ThreadingTCPServer(("127.0.0.1", HTTP_PORT), FakeK8s)
    api.daemon_threads = True
    threading.Thread(target=rpc.serve_forever, daemon=True).start()
    threading.Thread(target=api.serve_forever, daemon=True).start()
    log("f29 mock rig up: spdk=%s k8s=http://127.0.0.1:%d node=%s"
        % (SPDK_SOCK, HTTP_PORT, NODE_NAME))
    threading.Event().wait()


if __name__ == "__main__":
    sys.exit(main())
