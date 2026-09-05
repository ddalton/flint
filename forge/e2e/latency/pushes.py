#!/usr/bin/env python3
"""Interleaved, position-alternating push timing for the latency leg.

    pushes.py --rtt MS --pushes N --arm NAME CLONE SOCKET [--arm NAME CLONE SOCKET]

Prints one CSV line per measured push: rtt,rep,arm,pos,ms — wall clock
of `git push` as the client sees it, which for forge is the whole
batch: the hook waits on the syncer's report, and the report is sent
only after the snapshot CAS and the derived files (batch.rs step 6-7).

The warm-up push per arm — which creates `main` and publishes HEAD
once — is run and not printed. Position alternates per rep: odd reps
run the arms in the given order, even reps reversed, because
"interleaving is only interleaving if the position changes"
(tests/k8s/oci-ab/drive-ab.sh).
"""
import argparse
import os
import subprocess
import sys
import time

ap = argparse.ArgumentParser()
ap.add_argument("--rtt", type=int, required=True)
ap.add_argument("--pushes", type=int, default=10)
ap.add_argument("--arm", nargs=3, action="append", metavar=("NAME", "CLONE", "SOCKET"),
                required=True)
a = ap.parse_args()
arms = [tuple(x) for x in a.arm]

ENV = dict(
    os.environ,
    GIT_AUTHOR_NAME="driller", GIT_AUTHOR_EMAIL="driller@invalid",
    GIT_COMMITTER_NAME="driller", GIT_COMMITTER_EMAIL="driller@invalid",
    REMOTE_USER="driller",
)


def git(clone, *args, sock=None):
    env = dict(ENV)
    if sock:
        env["FLINT_FORGE_SOCKET"] = sock
    return subprocess.run(["git", "-C", clone, *args], env=env, capture_output=True, text=True)


def one_push(name, clone, sock, i):
    r = git(clone, "commit", "--allow-empty", "-q", "-m", f"{name} push {i}")
    if r.returncode:
        sys.exit(f"{name}: commit {i} failed: {r.stderr.strip()}")
    t0 = time.perf_counter()
    r = git(clone, "push", "-q", "origin", "HEAD:refs/heads/main", sock=sock)
    ms = (time.perf_counter() - t0) * 1000
    if r.returncode:
        sys.exit(f"{name}: push {i} failed rc={r.returncode}: {r.stdout.strip()} {r.stderr.strip()}")
    return ms


for name, clone, sock in arms:
    one_push(name, clone, sock, 0)
for rep in range(1, a.pushes + 1):
    order = arms if rep % 2 == 1 else list(reversed(arms))
    for pos, (name, clone, sock) in enumerate(order):
        ms = one_push(name, clone, sock, rep)
        print(f"{a.rtt},{rep},{name},{pos},{ms:.1f}", flush=True)
