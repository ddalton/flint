#!/usr/bin/env python3
"""Corrupt real traces so the trace check must REJECT them.

A trace checker that accepts every real trace proves nothing until it
rejects traces the code could not have produced. Each mutation below edits
ONE fact in a committed trace — a tree effect, an etag, a count, an ack —
and trace-check.sh requires TLC to fail to follow it.

  mutate.py <traces-dir> <out-dir>   writes <out-dir>/<trace>.<name>.ndjson
"""
import json
import sys
from pathlib import Path


def first(evs, pred):
    for i, e in enumerate(evs):
        if pred(e):
            return i
    raise SystemExit(f"mutation anchor not found: {pred.__doc__}")


def tombstone_removed(evs):
    """the queued deletion the fix supersedes is reported as applied"""
    i = first(evs, lambda e: e["ev"] == "tombstone" and e["action"] == "superseded")
    evs[i]["action"] = "removed"


def upload_wrong_etag(evs):
    """an upload is reported with the bytes the path had before the edit"""
    start = next(e for e in evs if e["ev"] == "conf_start")
    i = first(evs, lambda e: e["ev"] == "upload" and e.get("outcome") == "put" and e["path"] in start["entries"])
    evs[i]["etag"] = start["entries"][evs[i]["path"]]


def merge_foreign_plus_one(evs):
    """a merge reports one more foreign change than the manifest carried"""
    i = first(evs, lambda e: e["ev"] == "merge" and e["foreign"] + e["gone"] > 0)
    evs[i]["foreign"] += 1


def consume_not_adopted(evs):
    """a consume reports a UI write superseded that it actually adopted"""
    i = first(evs, lambda e: e["ev"] == "consume" and e["action"] == "adopted")
    evs[i]["action"] = "superseded"


def ack_ok(evs):
    """the ack over the outranked delete says ok"""
    i = first(evs, lambda e: e["ev"] == "ack" and e["status"] == "partial")
    evs[i]["status"] = "ok"


MUTATIONS = [
    ("ui_write_over_a_queued_delete", "tombstone-removed", tombstone_removed),
    ("both_edit_one_file", "upload-wrong-etag", upload_wrong_etag),
    ("edit_and_delete_cross", "merge-foreign-plus-one", merge_foreign_plus_one),
    ("ui_write_cited", "consume-not-adopted", consume_not_adopted),
    ("outranked_delete_publish", "ack-ok", ack_ok),
]


def main():
    d, out = Path(sys.argv[1]), Path(sys.argv[2])
    out.mkdir(parents=True, exist_ok=True)
    for trace, name, f in MUTATIONS:
        evs = [json.loads(l) for l in (d / f"{trace}.ndjson").read_text().splitlines() if l.strip()]
        f(evs)
        (out / f"{trace}.{name}.ndjson").write_text("".join(json.dumps(e) + "\n" for e in evs))
        print(f"{trace}.{name}: {f.__doc__}")


if __name__ == "__main__":
    main()
