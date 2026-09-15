#!/usr/bin/env python3
"""storm_collect.py — build oracle.py's collect layout (README §3) for one storm leg and judge it.

    storm_collect.py <leg> <outdir>

Environment: BUCKET, BIN (flint-sync), NODES (3), KILLS (0; > 0 passes
--faults-declared), AWS_REGION. Reads s3://$BUCKET/_rig/storm/<leg>/node-<n>.tgz
(storm.sh), and from the bucket itself: a fresh checkout, the manifest with a
HEAD of every citation, the listing, and every preserved copy's sha256. Writes
<outdir>/verdict.json (oracle.py's output plus the leg's facts); exit 0 iff it
passed.
"""
import glob, hashlib, json, os, shutil, subprocess, sys, tarfile, time

leg, out = sys.argv[1], sys.argv[2]
BUCKET, BIN = os.environ["BUCKET"], os.environ["BIN"]
NODES = int(os.environ.get("NODES", "3"))
KILLS = int(os.environ.get("KILLS", "0"))
REGION = os.environ.get("AWS_REGION", "us-west-1")
PFX = f"storm/{leg}"
HERE = os.path.dirname(os.path.abspath(__file__))
S3 = f"s3://{BUCKET}/_rig/storm/{leg}"
env = dict(os.environ, AWS_REGION=REGION, FLINT_SYNC_BUCKET=BUCKET, FLINT_SYNC_PREFIX=PFX)

def sh(*a, **kw):
    return subprocess.run(list(a), capture_output=True, text=True, **kw)

shutil.rmtree(out, ignore_errors=True)
for d in ("agents", "traces", "bucket", "checkout", "nodes"):
    os.makedirs(os.path.join(out, d), exist_ok=True)
facts = {"missing_nodes": []}

# 1. every node's evidence
phases = []
node_metas = []
for n in range(NODES):
    tgz = os.path.join(out, "nodes", f"node-{n}.tgz")
    r = sh("aws", "s3", "cp", "--quiet", f"{S3}/node-{n}.tgz", tgz, env=env)
    if r.returncode != 0:
        facts["missing_nodes"].append(n)
        continue
    nd = os.path.join(out, "nodes", f"node-{n}")
    with tarfile.open(tgz) as t:
        t.extractall(nd)
    ev = os.path.join(nd, "evidence")
    for a in glob.glob(os.path.join(ev, "agents", "*")):
        shutil.copytree(a, os.path.join(out, "agents", os.path.basename(a)), dirs_exist_ok=True)
    for tr in glob.glob(os.path.join(ev, "traces", "*.jsonl")):
        shutil.copy(tr, os.path.join(out, "traces", os.path.basename(tr)))
    for l in open(os.path.join(ev, "phases.jsonl")):
        if l.strip():
            phases.append(json.loads(l))
    node_metas.append(json.load(open(os.path.join(ev, "meta.node.json"))))
with open(os.path.join(out, "phases.jsonl"), "w") as f:
    for p in sorted(phases, key=lambda p: p["ts_ms"]):
        f.write(json.dumps(p) + "\n")
faults = [p for p in phases if p["ev"] == "fault"]
facts["faults"] = faults
facts["undrained"] = [p for p in phases if p["ev"] in ("undrained", "writer_died")]
facts["unquiet"] = [p for p in phases if p["ev"] == "unquiet"]  # a node drained before every node's actors stopped

agents = sorted(os.path.basename(a) for a in glob.glob(os.path.join(out, "agents", "*")))
def t(ev, pick):
    v = [p["ts_ms"] for p in phases if p["ev"] == ev]
    return pick(v) if v else None
meta = {"leg": leg, "prefix": PFX, "floor_secs": (node_metas[0]["floor"] if node_metas else None),
        "agents": agents, "t_start_ms": t("load_start", min), "t_quiesce_ms": t("load_end", max),
        "t_end_ms": t("drained", max), "nodes": node_metas}
json.dump(meta, open(os.path.join(out, "meta.json"), "w"), indent=1)

# 2. a fresh checkout
ct = os.path.join(out, "checkout-tree")
os.makedirs(ct, exist_ok=True)
r = sh(BIN, "checkout", env=dict(env, FLINT_SYNC_ROOT=ct))
json.dump({"code": r.returncode, "stderr_tail": r.stderr[-2000:]}, open(os.path.join(out, "checkout", "exit.json"), "w"))
rows = []
for d, dirs, files in os.walk(ct):
    rel_d = os.path.relpath(d, ct)
    dirs[:] = [x for x in dirs if not x.startswith(".flint")]
    for fn in files:
        if fn.endswith(".flint-sync-tmp"):
            continue
        p = os.path.join(d, fn)
        if os.path.islink(p) or not os.path.isfile(p):
            continue
        rel = os.path.normpath(os.path.join(rel_d, fn))
        rows.append((rel, hashlib.sha256(open(p, "rb").read()).hexdigest()))
with open(os.path.join(out, "checkout", "tree.sha256"), "w") as f:
    for rel, h in sorted(rows):
        f.write(f"{h}  {rel}\n")

# 3. the manifest, a HEAD of every citation
mroot = os.path.join(out, "manifest-root"); os.makedirs(mroot, exist_ok=True)
r = sh(BIN, "manifest", env=dict(env, FLINT_SYNC_ROOT=mroot))
try:
    m = json.loads(r.stdout.strip().splitlines()[-1])
except (ValueError, IndexError):
    m = {"seq": None, "entries": {}, "heads": {}}
    facts["manifest_error"] = r.stderr[-1000:]
json.dump({"seq": m.get("seq"), "entries": m.get("entries") or {}}, open(os.path.join(out, "bucket", "manifest.json"), "w"))
json.dump({p: {"etag": e} for p, e in (m.get("heads") or {}).items()}, open(os.path.join(out, "bucket", "heads.json"), "w"))

# 4. the listing and every preserved copy
r = sh("aws", "s3api", "list-objects-v2", "--bucket", BUCKET, "--prefix", PFX + "/", "--output", "json", env=env)
objs = (json.loads(r.stdout).get("Contents") or []) if r.returncode == 0 and r.stdout.strip() else []
if r.returncode != 0:
    facts["listing_error"] = r.stderr[-1000:]  # an unread listing is not an empty bucket
listing = [{"key": o["Key"], "etag": o["ETag"], "size": o["Size"], "last_modified": o["LastModified"]} for o in objs]
json.dump(listing, open(os.path.join(out, "bucket", "listing.json"), "w"))
preserved = []
pdir = os.path.join(out, "preserved"); os.makedirs(pdir, exist_ok=True)
# ONE sync of the conflicts prefix: a CLI process per copy took node 0 over
# eight minutes on a 300 s hot leg, past the other nodes' wait for the next go.
CONFLICTS = f"{PFX}/.flint/lean/conflicts/"
r = sh("aws", "s3", "sync", "--quiet", f"s3://{BUCKET}/{CONFLICTS}", pdir, env=env)
if r.returncode != 0:
    facts["preserved_sync_error"] = r.stderr[-1000:]
for o in listing:
    if not o["key"].startswith(CONFLICTS):
        continue
    dst = os.path.join(pdir, o["key"][len(CONFLICTS):])
    h = hashlib.sha256(open(dst, "rb").read()).hexdigest() if os.path.isfile(dst) else None
    preserved.append({"key": o["key"], "etag": o["etag"], "sha256": h})
json.dump(preserved, open(os.path.join(out, "bucket", "preserved.json"), "w"))

# 5. judge
args = [sys.executable, os.path.join(HERE, "oracle.py"), out, "--oracles", "O1,O2,O3,O4,O5", "--wall-slack-ms", "200"]
if KILLS > 0:
    args.append("--faults-declared")
r = sh(*args)
try:
    verdict = json.loads(r.stdout)
except ValueError:
    verdict = {"pass": False, "oracle_stdout": r.stdout[-3000:], "oracle_stderr": r.stderr[-3000:]}
if facts["missing_nodes"] or facts["unquiet"]:
    verdict["pass"] = False
# An unread copy is not an absent one: O3 and O4 would judge a hole.
if facts.get("listing_error") or facts.get("preserved_sync_error") or any(p["sha256"] is None for p in preserved):
    facts["preserved_unread"] = [p["key"] for p in preserved if p["sha256"] is None]
    verdict["pass"] = False
verdict["facts"] = facts
verdict["counts"] = {"agents": len(agents), "citations": len(m.get("entries") or {}), "objects": len(listing),
                     "preserved": len(preserved), "faults": len(faults)}
json.dump(verdict, open(os.path.join(out, "verdict.json"), "w"), indent=1)
summary = {k: (v.get("pass") if isinstance(v, dict) else v) for k, v in (verdict.get("oracles") or {}).items()}
print(f"STORM {leg} {'PASS' if verdict.get('pass') else 'FAIL'} oracles={summary} counts={verdict['counts']} missing_nodes={facts['missing_nodes']}")
sys.exit(0 if verdict.get("pass") else 1)
