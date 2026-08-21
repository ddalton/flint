#!/usr/bin/env python3
"""The gateway's RBAC, asserted against the rendered chart.

Why this is its own file rather than a heredoc: it is the assertion that
keeps the derived-token design honest, and it should be readable on its
own. `docs/plans/file-api-fleet-auth.md` §5 refuses `get secrets` in the
workspace namespaces for the front door, and the same reasoning binds
the gateway harder — those namespaces hold each tenant's S3 credentials
in the same place as the per-share API token. A gateway that could read
Secrets would not need derived tokens at all, and would hold every
tenant's bucket credentials as a side effect.

Also refused: `create` and `delete` on flintshares. Provisioning and
deleting a project are the front door's decisions; this component is a
proxy that reaches every project's files and handles untrusted input.
"""
import sys
import yaml

FORBIDDEN_RESOURCES = {"secrets", "pods", "persistentvolumeclaims", "deployments"}
FORBIDDEN_VERBS = {"create", "delete", "deletecollection", "update", "*"}

path = sys.argv[1]
docs = [d for d in yaml.safe_load_all(open(path)) if d]
roles = [
    d
    for d in docs
    if d.get("kind") in ("ClusterRole", "Role")
    and "gateway" in (d.get("metadata", {}).get("name") or "")
]
if not roles:
    print("FAIL: no gateway role rendered — this check would pass vacuously")
    sys.exit(1)

ok = True
for r in roles:
    name = r["metadata"]["name"]
    rules = r.get("rules") or []
    if not rules:
        print(f"FAIL: {name} has no rules; nothing is being checked")
        sys.exit(1)
    for rule in rules:
        res = set(rule.get("resources") or [])
        verbs = set(rule.get("verbs") or [])
        bad_res = res & FORBIDDEN_RESOURCES
        bad_verbs = verbs & FORBIDDEN_VERBS
        if bad_res:
            print(f"FAIL: {name} grants {sorted(bad_res)}: {rule}")
            ok = False
        if bad_verbs:
            print(f"FAIL: {name} grants {sorted(bad_verbs)} on {sorted(res)}: {rule}")
            ok = False
        if "*" in res:
            print(f"FAIL: {name} grants a wildcard resource: {rule}")
            ok = False

if not ok:
    sys.exit(1)

# Anti-vacuity: the role must actually grant the things the gateway
# NEEDS, or a role stripped to nothing would sail through the checks
# above while the gateway CrashLooped on every request.
granted = {
    (res, verb)
    for r in roles
    for rule in (r.get("rules") or [])
    for res in (rule.get("resources") or [])
    for verb in (rule.get("verbs") or [])
}
required = {("flintshares", v) for v in ("get", "list", "watch", "patch")}
missing = required - granted
if missing:
    print(f"FAIL: the gateway role is missing {sorted(missing)} — it could not serve")
    sys.exit(1)

print(f"OK: gateway role grants exactly {sorted(v for (_, v) in granted)} on flintshares, and nothing else")
