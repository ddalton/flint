#!/usr/bin/env python3
"""make_control.py — build the source tree of ONE control arm for the host legs.

    make_control.py <name> <src-root> <out-root>
    make_control.py --unpatched <src-root> <out-root>   # the same copy, no mutation
    make_control.py --list
    make_control.py --digest <root>                     # source digest only

<src-root> is a flint checkout (or a snapshot of one); <out-root> receives
`lean/syncer` and `crates/flint-store` (the syncer's one path dependency),
without any `target/`. Then exactly one mutation from
`control-patches/<name>.txt` is applied by EXACT STRING replacement:

  * the OLD string must occur exactly once in the named file, else nothing
    is written and the script exits 1;
  * the file's sha256 is printed before and after;
  * `<out-root>/CONTROL.json` records the patch, both hashes, and a digest
    of every OTHER source file, so a control can be checked to move ONE
    dimension against the fixed build (`--digest` on the fixed tree,
    compared with `digest_excluding_mutated`).

Safety: <out-root> must be under /private/tmp/claude-503/ and must not be
inside <src-root>. The real sources are only ever read.
"""
import hashlib
import json
import os
import shutil
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
PATCH_DIR = os.path.join(HERE, "control-patches")
SCRATCH = "/private/tmp/claude-503/"
COPIED = ["lean/syncer", "crates/flint-store"]
SKIP_DIRS = {"target", ".git", "__pycache__"}


def die(msg):
    print(f"make_control: {msg}", file=sys.stderr)
    sys.exit(1)


def sha256_file(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def source_files(root):
    """Every file that feeds the build: Cargo manifests, lockfile, src/**."""
    out = []
    for rel in COPIED:
        base = os.path.join(root, rel)
        for name in ("Cargo.toml", "Cargo.lock", "build.rs"):
            if os.path.isfile(os.path.join(base, name)):
                out.append(f"{rel}/{name}")
        for d, dirs, files in os.walk(os.path.join(base, "src")):
            dirs[:] = sorted(x for x in dirs if x not in SKIP_DIRS)
            for f in sorted(files):
                out.append(os.path.relpath(os.path.join(d, f), root))
    return sorted(out)


def digest(root, exclude=None):
    h = hashlib.sha256()
    n = 0
    for rel in source_files(root):
        if rel == exclude:
            continue
        h.update(rel.encode() + b"\0" + sha256_file(os.path.join(root, rel)).encode() + b"\n")
        n += 1
    return h.hexdigest(), n


def parse_patch(name):
    path = os.path.join(PATCH_DIR, f"{name}.txt")
    if not os.path.isfile(path):
        die(f"no patch {path} (have: {', '.join(list_patches())})")
    text = open(path, encoding="utf-8").read()
    head, sep, rest = text.partition("=== OLD ===\n")
    if not sep:
        die(f"{path}: no '=== OLD ===' marker")
    old, sep, rest = rest.partition("\n=== NEW ===\n")
    if not sep:
        die(f"{path}: no '=== NEW ===' marker")
    new, sep, _ = rest.partition("\n=== END ===")
    if not sep:
        die(f"{path}: no '=== END ===' marker")
    meta = {}
    for line in head.splitlines():
        if ":" in line:
            k, v = line.split(":", 1)
            meta[k.strip()] = v.strip()
    for k in ("name", "file", "unit_test", "leg"):
        if k not in meta:
            die(f"{path}: header lacks '{k}:'")
    if meta["name"] != name:
        die(f"{path}: header name {meta['name']!r} != {name!r}")
    if old == new:
        die(f"{path}: OLD == NEW, not a mutation")
    meta["old"], meta["new"], meta["patch_file"] = old, new, path
    return meta


def list_patches():
    return sorted(f[:-4] for f in os.listdir(PATCH_DIR) if f.endswith(".txt"))


def copy_tree(src_root, out_root):
    real_src = os.path.realpath(src_root)
    real_out = os.path.realpath(os.path.dirname(out_root.rstrip("/")) or "/")
    real_out = os.path.join(real_out, os.path.basename(out_root.rstrip("/")))
    if not (real_out + "/").startswith(os.path.realpath(SCRATCH)):
        die(f"out-root {out_root} ({real_out}) is not under {SCRATCH}")
    if (real_out + "/").startswith(real_src + "/") or real_out == real_src:
        die(f"out-root {real_out} is inside src-root {real_src}")
    for rel in COPIED:
        if not os.path.isdir(os.path.join(src_root, rel)):
            die(f"{src_root} has no {rel}")
    if os.path.exists(out_root):
        # Only a tree this script made may be replaced.
        if not os.path.isfile(os.path.join(out_root, "CONTROL.json")):
            die(f"{out_root} exists and has no CONTROL.json; refusing to overwrite it")
        shutil.rmtree(out_root)
    for rel in COPIED:
        shutil.copytree(
            os.path.join(src_root, rel),
            os.path.join(out_root, rel),
            ignore=shutil.ignore_patterns(*SKIP_DIRS),
            symlinks=True,
        )


def main(argv):
    if argv[1:2] == ["--list"]:
        for n in list_patches():
            m = parse_patch(n)
            print(f"{n}\t{m['leg']}\t{m['file']}\t{m['unit_test']}")
        return 0
    if argv[1:2] == ["--digest"] and len(argv) == 3:
        d, n = digest(argv[2])
        print(f"{d}  {n} files  {argv[2]}")
        return 0
    if len(argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2
    name, src_root, out_root = argv[1], argv[2], argv[3]
    unpatched = name == "--unpatched"
    meta = None if unpatched else parse_patch(name)

    src_digest, nsrc = digest(src_root)
    copy_tree(src_root, out_root)
    record = {
        "control": "unpatched" if unpatched else name,
        "src_root": os.path.realpath(src_root),
        "src_digest": src_digest,
        "src_files": nsrc,
    }
    if unpatched:
        d, _ = digest(out_root)
        if d != src_digest:
            die(f"copy digest {d} != source digest {src_digest}: the source changed while copying")
        record["digest_excluding_mutated"] = d
        print(f"make_control: unpatched copy of {src_root} at {out_root}")
        print(f"  source digest {src_digest} ({nsrc} files)")
    else:
        target = os.path.join(out_root, meta["file"])
        body = open(target, encoding="utf-8").read()
        before = sha256_file(target)
        count = body.count(meta["old"])
        if count != 1:
            shutil.rmtree(out_root)
            die(f"{meta['file']}: OLD occurs {count} times (must be exactly 1); nothing applied, copy removed")
        if meta["new"] in body:
            shutil.rmtree(out_root)
            die(f"{meta['file']}: NEW already present before the mutation; nothing applied, copy removed")
        with open(target, "w", encoding="utf-8") as f:
            f.write(body.replace(meta["old"], meta["new"], 1))
        after = sha256_file(target)
        if after == before:
            die("sha256 unchanged after the replacement")
        check = open(target, encoding="utf-8").read()
        if check.count(meta["old"]) != 0 or check.count(meta["new"]) != 1:
            die("post-condition failed: OLD still present or NEW not present exactly once")
        rest, _ = digest(out_root, exclude=meta["file"])
        src_rest, _ = digest(src_root, exclude=meta["file"])
        if rest != src_rest:
            die("the copy differs from the source in a file other than the mutated one")
        record.update({
            "file": meta["file"],
            "leg": meta["leg"],
            "unit_test": meta["unit_test"],
            "expect_fail": meta.get("expect_fail"),
            "patch_file": meta["patch_file"],
            "sha256_before": before,
            "sha256_after": after,
            "digest_excluding_mutated": rest,
            "old": meta["old"],
            "new": meta["new"],
        })
        print(f"make_control: {name} ({meta['leg']}) at {out_root}")
        print(f"  file    {meta['file']}")
        print(f"  before  sha256 {before}")
        print(f"  after   sha256 {after}")
        print(f"  others  digest {rest} (identical to the source's)")
        print(f"  test    cargo test --lib {meta['unit_test']}")
    with open(os.path.join(out_root, "CONTROL.json"), "w") as f:
        json.dump(record, f, indent=2)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
