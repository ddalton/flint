#!/usr/bin/env python3
"""Mock spdk-tgt for the kind RACE-test tier.

The race tier tests flint's CONTROL PLANE under concurrency — CAS contention
(R1), claim arbitration / F43 starvation (R2), RPC deadlines / hung-socket
(R5), handler-vs-detector interleavings (R2 node lock). None of that needs a
real data path, so this replaces the amd64-only, hugepage-hungry spdk-tgt
with a tiny arch-neutral stand-in that:

  * speaks flint's socket protocol — newline-delimited JSON-RPC, one fresh
    connection per call (spdk_native.rs:269-275);
  * keeps just enough STATE (lvstores / bdevs / raids / subsystems) that the
    driver's get_*/create/delete calls stay self-consistent across a session;
  * injects FAULTS per method — the whole point of the tier. A JSON control
    file (SPDK_MOCK_FAULTS, re-read every request so tests inject at runtime)
    maps method -> {action, ...}:
        {"bdev_get_bdevs": {"action": "hang"}}          # never respond (R5 deadline)
        {"bdev_raid_create": {"action": "delay", "secs": 3}}
        {"nvmf_subsystem_add_ns": {"action": "error", "code": -32001, "msg": "EBUSY"}}
        {"bdev_raid_get_bdevs": {"action": "state", "operational": 1, "total": 2}}  # degraded (F43)

Not a data plane: no bytes move. Coherence over fidelity — enough for the
control-plane races, deliberately NOT the integration tier (that stays on a
real-spdk, amd64, hugepage host).
"""
import asyncio
import json
import os
import time

SOCK = os.environ.get("SPDK_MOCK_SOCK", "/var/tmp/spdk.sock")
FAULTS = os.environ.get("SPDK_MOCK_FAULTS", "/var/tmp/spdk-mock-faults.json")

# ── minimal coherent state ──────────────────────────────────────────────
# Seeded so a freshly-staged node looks like a healthy 2-leg raid volume the
# F43 scenario can then degrade via a "state" fault. Tests reshape via faults
# or the /reset control rather than reaching in here.
STATE = {
    "version": {"major": 26, "minor": 5, "patch": 0, "suffix": "", "commit": "mock"},
    "lvstores": [{"uuid": "lvs-mock-0", "name": "lvs_kind", "base_bdev": "uring_nvme1n1",
                  "free_clusters": 8000, "cluster_size": 4194304, "total_data_clusters": 8000}],
    "bdevs": {},       # name -> bdev dict
    "raids": {},       # name -> raid dict
    "subsystems": {},  # nqn -> subsystem dict
    "controllers": {}, # name -> controller dict
}


def load_faults():
    try:
        with open(FAULTS) as f:
            return json.load(f)
    except Exception:
        return {}


def result_ok(_p):
    return True


def m_spdk_get_version(_p):
    return {"version": "SPDK v26.05 (mock)", "fields": STATE["version"]}


def m_bdev_get_bdevs(p):
    if p and p.get("name"):
        b = STATE["bdevs"].get(p["name"])
        return [b] if b else []
    return list(STATE["bdevs"].values())


def m_bdev_lvol_get_lvstores(_p):
    return STATE["lvstores"]


def m_bdev_lvol_get_lvols(_p):
    return [b for b in STATE["bdevs"].values() if b.get("_kind") == "lvol"]


def m_bdev_raid_get_bdevs(_p):
    return list(STATE["raids"].values())


def m_nvmf_get_subsystems(_p):
    return list(STATE["subsystems"].values())


def m_bdev_nvme_get_controllers(p):
    if p and p.get("name"):
        c = STATE["controllers"].get(p["name"])
        return [c] if c else []
    return list(STATE["controllers"].values())


def _mk_lvol(name, uuid=None):
    d = {"name": name, "uuid": uuid or name, "_kind": "lvol",
         "aliases": [f"lvs_kind/{name}"], "block_size": 4096, "num_blocks": 262144,
         "driver_specific": {"lvol": {"lvol_store_uuid": "lvs-mock-0"}}}
    STATE["bdevs"][name] = d
    return d


def m_bdev_lvol_create(p):
    name = p.get("lvol_name") or f"lvol-{len(STATE['bdevs'])}"
    _mk_lvol(name)
    return name


def m_bdev_malloc_create(p):
    name = p.get("name") or f"Malloc{len(STATE['bdevs'])}"
    STATE["bdevs"][name] = {"name": name, "uuid": name, "_kind": "malloc",
                            "block_size": 4096, "num_blocks": 262144}
    return name


def m_bdev_uring_create(p):
    name = p.get("name") or f"uring{len(STATE['bdevs'])}"
    STATE["bdevs"][name] = {"name": name, "uuid": name, "_kind": "uring",
                            "block_size": 4096, "num_blocks": 262144}
    return name


def m_bdev_nvme_attach_controller(p):
    name = p["name"]
    ns = f"{name}n1"
    STATE["controllers"][name] = {"name": name, "ctrlrs": [{"state": "live",
        "trid": {"traddr": p.get("traddr", ""), "subnqn": p.get("subnqn", "")}}]}
    STATE["bdevs"][ns] = {"name": ns, "uuid": ns, "_kind": "nvme", "block_size": 4096, "num_blocks": 262144}
    return [ns]


def m_bdev_raid_create(p):
    name = p["name"]
    bases = p.get("base_bdevs", [])
    STATE["raids"][name] = {"name": name, "state": "online", "raid_level": "raid1",
        "num_base_bdevs": len(bases), "num_base_bdevs_operational": len(bases),
        "base_bdevs_list": [{"name": b, "is_configured": True, "uuid": b} for b in bases]}
    STATE["bdevs"][name] = {"name": name, "uuid": name, "_kind": "raid", "block_size": 4096, "num_blocks": 262144}
    return True


def m_nvmf_create_subsystem(p):
    nqn = p["nqn"]
    STATE["subsystems"].setdefault(nqn, {"nqn": nqn, "namespaces": [], "listen_addresses": [], "hosts": []})
    return True


def m_nvmf_subsystem_add_ns(p):
    s = STATE["subsystems"].setdefault(p["nqn"], {"nqn": p["nqn"], "namespaces": [], "listen_addresses": [], "hosts": []})
    s["namespaces"].append({"nsid": len(s["namespaces"]) + 1})
    return len(s["namespaces"])


def m_nvmf_subsystem_add_listener(p):
    s = STATE["subsystems"].setdefault(p["nqn"], {"nqn": p["nqn"], "namespaces": [], "listen_addresses": [], "hosts": []})
    s["listen_addresses"].append(p.get("listen_address", {}))
    return True


# Everything not listed returns a bare `true` (create/delete/quiesce/etc.).
HANDLERS = {
    "spdk_get_version": m_spdk_get_version,
    "bdev_get_bdevs": m_bdev_get_bdevs,
    "bdev_lvol_get_lvstores": m_bdev_lvol_get_lvstores,
    "bdev_lvol_get_lvols": m_bdev_lvol_get_lvols,
    "bdev_raid_get_bdevs": m_bdev_raid_get_bdevs,
    "nvmf_get_subsystems": m_nvmf_get_subsystems,
    "bdev_nvme_get_controllers": m_bdev_nvme_get_controllers,
    "bdev_lvol_create": m_bdev_lvol_create,
    "bdev_malloc_create": m_bdev_malloc_create,
    "bdev_uring_create": m_bdev_uring_create,
    "bdev_nvme_attach_controller": m_bdev_nvme_attach_controller,
    "bdev_raid_create": m_bdev_raid_create,
    "nvmf_create_subsystem": m_nvmf_create_subsystem,
    "nvmf_subsystem_add_ns": m_nvmf_subsystem_add_ns,
    "nvmf_subsystem_add_listener": m_nvmf_subsystem_add_listener,
}


async def dispatch(req):
    method = req.get("method", "")
    rid = req.get("id", 0)
    params = req.get("params") or {}

    fault = load_faults().get(method)
    if fault:
        action = fault.get("action")
        if action == "hang":
            await asyncio.sleep(fault.get("secs", 86400))  # effectively never (R5 deadline test)
        elif action == "delay":
            await asyncio.sleep(fault.get("secs", 1))
        elif action == "error":
            return {"jsonrpc": "2.0", "id": rid,
                    "error": {"code": fault.get("code", -32000), "message": fault.get("msg", "injected")}}
        elif action == "state" and method == "bdev_raid_get_bdevs":
            # degrade every raid to operational/total (F43 / degraded-serve scenarios)
            op, tot = fault.get("operational", 1), fault.get("total", 2)
            raids = []
            for r in STATE["raids"].values():
                rr = dict(r); rr["num_base_bdevs_operational"] = op; rr["num_base_bdevs"] = tot
                bl = rr.get("base_bdevs_list", [])
                for i, b in enumerate(bl):
                    b = dict(b); b["is_configured"] = i < op; bl[i] = b
                rr["base_bdevs_list"] = bl; raids.append(rr)
            return {"jsonrpc": "2.0", "id": rid, "result": raids}

    handler = HANDLERS.get(method, result_ok)
    try:
        return {"jsonrpc": "2.0", "id": rid, "result": handler(params)}
    except Exception as e:  # a mock bug must surface as an RPC error, not a hang
        return {"jsonrpc": "2.0", "id": rid, "error": {"code": -32603, "message": f"mock: {e}"}}


async def handle(reader, writer):
    try:
        while True:
            line = await reader.readline()
            if not line:
                break
            try:
                req = json.loads(line)
            except Exception:
                continue
            resp = await dispatch(req)
            writer.write((json.dumps(resp) + "\n").encode())
            await writer.drain()
    except (ConnectionResetError, BrokenPipeError):
        pass
    finally:
        try:
            writer.close()
        except Exception:
            pass


async def main():
    if os.path.exists(SOCK):
        os.unlink(SOCK)
    server = await asyncio.start_unix_server(handle, path=SOCK)
    os.chmod(SOCK, 0o666)
    print(f"[mock-spdk] listening on {SOCK}; faults<-{FAULTS} (t0={int(time.time())})", flush=True)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main())
