#!/usr/bin/env python3
"""Corrupt the current code's traces so the CORE trace check must reject them.

mutate.py's twin for traces-core/ (the P1-lite traces TraceCore.tla
reads, against LeanP1.tla). Each mutation edits ONE fact the converter maps to a check; a
checker that accepted any of them would be checking less than it says.
(mutate.py's `ack-ok` has no twin: the core has no ack.)

  mutate_core.py <traces-dir> <out-dir>   writes <out-dir>/<trace>.<name>.ndjson
"""
import json
import sys
from pathlib import Path


def nth(evs, pred, n=0):
    hits = [i for i, e in enumerate(evs) if pred(e)]
    if len(hits) <= n:
        raise SystemExit(f"mutation anchor not found: {pred.__doc__ or pred}")
    return hits[n]


def over_theirs_dropped(evs):
    """the merge that deleted over B's edit reports a plain delete (M3's fact)"""
    i = nth(evs, lambda e: e["ev"] == "merge" and e.get("over_theirs"))
    evs[i]["over_theirs"] = []


def over_delete_unrecorded(evs):
    """the record of B's version, which A's delete went over, is never written"""
    del evs[nth(evs, lambda e: e["ev"] == "surface" and e.get("over_delete"))]


def owed_never_taken(evs):
    """the save B's merge saw mid-barrier is reported never adopted"""
    ws = [i for i, e in enumerate(evs) if e["ev"] == "conf_hitl_write"]
    i = nth(evs, lambda e: e["ev"] == "consume" and e["action"] == "adopted" and e["path"] == "doc.txt")
    if not ws or i < ws[0]:
        raise SystemExit("mutation anchor not found: an adoption after the mid-barrier save")
    del evs[i]


def outranked_invented(evs):
    """the merge that applied A's delete reports it outranked"""
    i = nth(evs, lambda e: e["ev"] == "merge" and e["deletes"] == 1 and not e["outranked"])
    evs[i]["outranked"] = ["shared.txt"]


def upload_wrong_etag(evs):
    """an upload is reported with the bytes the path had before the edit"""
    start = next(e for e in evs if e["ev"] == "conf_start")
    i = nth(evs, lambda e: e["ev"] == "upload" and e["path"] in start["entries"])
    evs[i]["etag"] = start["entries"][evs[i]["path"]]


def merge_foreign_plus_one(evs):
    """a merge reports one more foreign change than the document carried"""
    i = nth(evs, lambda e: e["ev"] == "merge" and e["foreign"] + e["gone"] > 0)
    evs[i]["foreign"] += 1


def consume_not_adopted(evs):
    """a consume that adopted a UI write reports nothing"""
    del evs[nth(evs, lambda e: e["ev"] == "consume" and e["action"] == "adopted")]


def tombstone_kept(evs):
    """a queued deletion the tree applied is reported kept (dirty)"""
    i = nth(evs, lambda e: e["ev"] == "tombstone" and e["action"] == "removed")
    evs[i]["action"] = "kept-dirty"


def gc_retired_plus_one(evs):
    """the collector reports one more retired handle than the install retired"""
    i = nth(evs, lambda e: e["ev"] == "gc" and "retired" in e)
    evs[i]["retired"] += 1


def scan_delete_dropped(evs):
    """a scan that found the agent's delete reports none"""
    i = nth(evs, lambda e: e["ev"] == "scan" and e["deletes"] > 0)
    evs[i]["deletes"] -= 1


def surface_dropped(evs):
    """the R7 record of theirs' version is never written"""
    del evs[nth(evs, lambda e: e["ev"] == "surface")]


def delete_override_unrecorded(evs):
    """the record of the delete an edit published over is never written"""
    del evs[nth(evs, lambda e: e["ev"] == "surface" and e.get("deleted"))]


def ui_commit_seq_skips(evs):
    """the gateway's commit reports a generation one past the one it installed"""
    i = nth(evs, lambda e: e["ev"] == "conf_hitl_write")
    evs[i]["seq"] += 1


def ui_delete_seq_skips(evs):
    """the gateway's delete reports a generation one past the one it installed"""
    i = nth(evs, lambda e: e["ev"] == "conf_hitl_delete")
    evs[i]["seq"] += 1


def ui_delete_not_unlinked(evs):
    """the tree's removal of a UI-deleted file is reported as already absent"""
    i = nth(evs, lambda e: e["ev"] == "tombstone" and e["action"] == "removed")
    evs[i]["action"] = "absent"


def reupload_same_key(evs):
    """a withheld upload is re-published at the key it was first written to"""
    ups = [i for i, e in enumerate(evs) if e["ev"] == "upload" and e["path"] == "p.txt"]
    if len(ups) < 2:
        raise SystemExit("mutation anchor not found: a re-upload of p.txt")
    evs[ups[1]]["key"] = evs[ups[0]]["key"]


def rename_destination_not_adopted(evs):
    """the rename's destination is never reported adopted"""
    del evs[nth(evs, lambda e: e["ev"] == "consume" and e["action"] == "adopted" and e["path"] == "y.txt")]


def sweep_takes_the_cited_write(evs):
    """the retire reap names the UI write the document cites"""
    ws = [e for e in evs if e["ev"] == "conf_hitl_write"]
    i = nth(evs, lambda e: e["ev"] == "sweep" and e.get("what") == "retired")
    evs[i]["keys"] = [ws[-1]["key"]]


def cas_seq_skips(evs):
    """a CAS reports a seq one past the one it installed"""
    i = nth(evs, lambda e: e["ev"] == "cas" and e["result"] == "ok")
    evs[i]["seq"] += 1


MUTATIONS = [
    ("delete_over_a_peers_edit", "over-theirs-dropped", over_theirs_dropped),
    ("delete_over_a_peers_edit", "outranked-invented", outranked_invented),
    ("delete_over_a_peers_edit", "over-delete-unrecorded", over_delete_unrecorded),
    ("both_edit_one_file", "upload-wrong-etag", upload_wrong_etag),
    ("ui_save_lands_inside_a_barrier", "merge-foreign-plus-one", merge_foreign_plus_one),
    ("edit_and_delete_cross", "tombstone-kept", tombstone_kept),
    ("edit_and_delete_cross", "scan-delete-dropped", scan_delete_dropped),
    ("ui_write_cited", "consume-not-adopted", consume_not_adopted),
    ("both_edit_one_file", "gc-retired-plus-one", gc_retired_plus_one),
    ("ui_write_cited", "ui-commit-seq-skips", ui_commit_seq_skips),
    ("both_edit_one_file", "cas-seq-skips", cas_seq_skips),
    ("both_edit_one_file", "surface-dropped", surface_dropped),
    ("ui_delete_applied", "ui-delete-seq-skips", ui_delete_seq_skips),
    ("ui_delete_applied", "ui-delete-not-unlinked", ui_delete_not_unlinked),
    ("ui_rename", "rename-destination-not-adopted", rename_destination_not_adopted),
    ("superseded_ui_write_swept", "reap-takes-the-cited-write", sweep_takes_the_cited_write),
    ("withheld_republished_over_a_ui_save", "reupload-same-key", reupload_same_key),
    ("agent_edit_over_a_ui_delete", "delete-override-unrecorded", delete_override_unrecorded),
    ("ui_save_lands_inside_a_barrier", "owed-never-taken", owed_never_taken),
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
