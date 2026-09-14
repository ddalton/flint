#!/usr/bin/env bash
# shipper.sh — E3, the node evidence shipper (plan §2a). Run on EVERY node as root.
#
#   BUCKET=<drill bucket> NODE=<k8s node name> shipper.sh start|flush|stop|status
#
# start   launches detached loops (setsid + nohup, pidfiles) that outlive the
#         SSM command; a loop already alive is left alone, a dead one restarted:
#   logs     every DISCOVER_SECS (2) find new container log files under
#            PODS_ROOT (/var/log/pods) for namespaces matching NS_RE, and give
#            each one `tail -c +<bytes already copied + 1> -f` (see LOG CAPTURE)
#            into $EVID/pods/<ns>_<pod>_<uid>/<container>/<N>.log
#   cgroups  every CGROUP_SECS (10) one JSON line per kubepods cgroup to
#            $EVID/cgroups.jsonl: {ts_ms, path, pod_uid, container_id,
#            memory_current, memory_peak, memory_max, cpu_usage_usec, oom, oom_kill}
#   chrony   every CHRONY_SECS (60) `chronyc -c tracking` to $EVID/chrony.jsonl
#            (see CLOCK)
#   dmesg    `dmesg -w` (restarts use -W, new messages only) to $EVID/dmesg.log
#   journal  `journalctl -b -f -u kubelet -u containerd -o short-iso-precise` to
#            $EVID/journal.log, resuming from a cursor file after a restart
#   upload   every UPLOAD_SECS (300) the flush below
# flush   snapshot $EVID into a stage dir (changed files copied, sha256 cached),
#         `aws s3 sync` the stage to s3://$BUCKET/_rig/evidence/$NODE/, then upload
#         SHA256SUMS last (so every name in it is already uploaded); print the
#         bucket's object count and total bytes for that prefix beside the
#         stage's. A failed sync or listing exits non-zero — never "0 objects".
# stop    stop discovery, wait for every tail to reach the end of its file
#         (CATCHUP_SECS, 30), stop the tails and the other loops, then flush.
#         The shipper's own loop logs and tail registry ride along under _shipper/.
# status  each loop alive or dead; tails registered / alive / whose file has
#         vanished (held open) / lost; the last flush.
#
# Environment: BUCKET, NODE (required at the first start; recorded in $STATE/node, and a later
# call under another NODE warns and uses the recorded name), EVID
# (/mnt/nvme/evidence), STATE (/mnt/nvme/shipper — pidfiles, tail registry,
# stage; never uploaded), REGION (us-west-1), NS_RE
# ('^(flint-workers|flint-system|wl-.*|flint-lean.*)$'), PODS_ROOT, CGROUP_ROOT
# (/sys/fs/cgroup), DISCOVER_SECS, CGROUP_SECS, CHRONY_SECS, UPLOAD_SECS,
# CATCHUP_SECS. Test seams: AWS_BIN, CHRONYC_BIN, DMESG_BIN, JOURNALCTL_BIN,
# TAIL_BIN, STDBUF_BIN, ALLOW_ROOTFS=1 (skip the /mnt/nvme device check).
#
# Writes: only under $EVID and $STATE (both must be off the root filesystem's
# device), and only to s3://$BUCKET/_rig/evidence/$NODE/.
#
# LOG CAPTURE. A tail follows the file's DESCRIPTOR, not its name: the file
# is opened by this script, its inode checked against the one discovered, and
# handed to tail as stdin (`tail -c +K -f`, polling once a second). Keyed by
# (device, inode, pod dir, container, N), so:
#  - rotation (kubelet renames N.log to N.log.<YYYYmmdd-HHMMSS>, then asks
#    containerd to reopen) loses nothing: the old tail keeps reading the renamed
#    inode, including lines containerd writes between the rename and the reopen,
#    and the new N.log is a new inode with its own tail and its own output
#    (<N>.log.i<inode>). `tail -F` with inotify (Linux) would lose exactly those
#    lines: coreutils tail.c handles IN_MOVE_SELF with recheck(), which closes
#    the old descriptor without reading its remainder, and says so in its own
#    FIXME. (Polling `tail -F`, as on macOS, drains first — so
#    shipper_localtest.sh on a Mac cannot show that loss; this is read from
#    the source, not observed.) extract_traces.py orders segments by their
#    first CRI timestamp.
#  - pod deletion (kubelet removes /var/log/pods/<pod>) loses nothing that was
#    written: the tail holds the inode open and reads it to the end. `tail -F`
#    closes on the unlink ("has become inaccessible") with whatever was unread.
#  - a tail that died is restarted at exactly the bytes already copied (the
#    output is a byte copy), if its file still has a name.
#  - N.log.<ts>.gz (kubelet compresses older rotations) is copied once, unless
#    its uncompressed name belonged to an inode already tailed.
# Possible loss, and its size:
#  - a log file created AND removed within one discovery interval (2 s) is
#    never seen: the whole file. (A container that lives under 2 s, in a pod
#    deleted within the same 2 s.)
#  - a tail killed abnormally (OOM, kill -9) after its file lost its last name:
#    the unread remainder, normally under one poll interval (1 s) of writes.
#    Recorded as `lost_after` (dead when the file vanished) or
#    `died_after_vanish` (died later; complete only if it had caught up) in the
#    registry, and counted by `status`.
#  - `stop` waits for every tail to reach its file's size before stopping it;
#    past CATCHUP_SECS it stops them anyway and prints the laggards.
#  - while the shipper is NOT running nothing is captured.
# Cost: vanished inodes stay open until `stop` (their blocks stay allocated on
# the root filesystem; worker logs are MBs), one tail process each.
#
# CLOCK. chrony.jsonl offset_ms = node clock − true time (positive: AHEAD), the
# convention of timeline.py. From `chronyc -c tracking`, field 5 (index 4,
# "System time"): chronyc prints current_correction there with no sign
# rewriting in CSV mode; the text form prints |value| and "slow" when the value
# is positive (client.c print_report, case 'O': (dbl > 0.0) ^ (spec != 'O') ?
# "slow" : "fast"; identical in chrony 4.0, 4.3 and master). Positive = the
# clock is slow = behind true time, so offset_ms = −field5 × 1000. Fields:
# ref_id, ref_name, stratum, ref_time, system_time, last_offset, rms_offset,
# freq_ppm, resid_freq_ppm, skew_ppm, root_delay, root_dispersion,
# update_interval, leap. Note last_offset has the OPPOSITE sense (positive =
# the clock was ahead); it is recorded raw, never used for offset_ms.

set -u

SELF_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SELF="$SELF_DIR/$(basename "${BASH_SOURCE[0]}")"

EVID=${EVID:-/mnt/nvme/evidence}
STATE=${STATE:-/mnt/nvme/shipper}
REGION=${REGION:-us-west-1}
NS_RE=${NS_RE:-'^(flint-workers|flint-system|wl-.*|flint-lean.*)$'}
PODS_ROOT=${PODS_ROOT:-/var/log/pods}
CGROUP_ROOT=${CGROUP_ROOT:-/sys/fs/cgroup}
DISCOVER_SECS=${DISCOVER_SECS:-2}
CGROUP_SECS=${CGROUP_SECS:-10}
CHRONY_SECS=${CHRONY_SECS:-60}
UPLOAD_SECS=${UPLOAD_SECS:-300}
CATCHUP_SECS=${CATCHUP_SECS:-30}
AWS_BIN=${AWS_BIN:-aws}
CHRONYC_BIN=${CHRONYC_BIN:-chronyc}
DMESG_BIN=${DMESG_BIN:-dmesg}
JOURNALCTL_BIN=${JOURNALCTL_BIN:-journalctl}
TAIL_BIN=${TAIL_BIN:-tail}
STDBUF_BIN=${STDBUF_BIN:-stdbuf}
ALLOW_ROOTFS=${ALLOW_ROOTFS:-0}
export EVID STATE REGION NS_RE PODS_ROOT CGROUP_ROOT DISCOVER_SECS CGROUP_SECS CHRONY_SECS \
    UPLOAD_SECS CATCHUP_SECS AWS_BIN CHRONYC_BIN DMESG_BIN JOURNALCTL_BIN TAIL_BIN STDBUF_BIN ALLOW_ROOTFS

SHIPPER_LOOPS="dmesg journal cgroups chrony logs upload"

say() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"; }
die() { say "ERROR: $*" >&2; exit 1; }

# ---------------------------------------------------------------- guards --

rig_require_offroot() { # dir... — each must live off the root filesystem's device
    [ "$ALLOW_ROOTFS" = 1 ] && return 0
    python3 - "$@" <<'PY' || exit 1
import os, sys
root = os.stat("/").st_dev
for d in sys.argv[1:]:
    p = os.path.abspath(d)
    while not os.path.exists(p):
        p = os.path.dirname(p)
    if os.stat(p).st_dev == root:
        sys.exit(f"refusing {d}: {p} is on the root filesystem's device (mount /mnt/nvme first; ALLOW_ROOTFS=1 for tests)")
PY
}

rig_require_name() { # value label
    case "$1" in
        ''|*[!A-Za-z0-9._-]*) die "$2 must be non-empty [A-Za-z0-9._-], got '$1'" ;;
    esac
}

# ------------------------------------------------------- process control --

rig_pidfile() { printf '%s/pids/%s.pid' "$STATE" "$1"; }

rig_alive() { # name
    local f pid
    f=$(rig_pidfile "$1")
    [ -s "$f" ] || return 1
    pid=$(cat "$f")
    kill -0 "$pid" 2>/dev/null
}

rig_detach() { # name cmd... — the loop leads its own session: no SSM pipe held, no SSM process group
    local name=$1
    shift
    mkdir -p "$STATE/pids" "$STATE/logs"
    if command -v setsid >/dev/null 2>&1; then
        setsid nohup "$@" </dev/null >>"$STATE/logs/$name.log" 2>&1 &
    else
        nohup "$@" </dev/null >>"$STATE/logs/$name.log" 2>&1 &
    fi
    echo $! >"$(rig_pidfile "$name")"
}

rig_kill_loop() { # name [pid-only] — TERM its process group when it leads one (else, or with
    # pid-only, just the pid: the logs loop's tails share its group and must outlive it until caught up)
    local f pid pgid i
    f=$(rig_pidfile "$1")
    [ -s "$f" ] || return 0
    pid=$(cat "$f")
    if kill -0 "$pid" 2>/dev/null; then
        pgid=$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ')
        if [ "${2:-}" != pid-only ] && [ -n "$pgid" ] && [ "$pgid" = "$pid" ]; then
            kill -TERM -- "-$pid" 2>/dev/null
        else
            kill -TERM "$pid" 2>/dev/null
        fi
        i=0
        while kill -0 "$pid" 2>/dev/null && [ $i -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
        kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null
    fi
    rm -f "$f"
}

rig_stdbuf() { # print the line-buffering prefix when stdbuf exists
    command -v "$STDBUF_BIN" >/dev/null 2>&1 && printf '%s' "$STDBUF_BIN"
}

# ---------------------------------------------------------------- flush --

rig_lock() { # dir — mkdir lock; a lock whose owner is dead is taken over
    local d=$1 i=0 owner
    while ! mkdir "$d" 2>/dev/null; do
        owner=$(cat "$d/pid" 2>/dev/null)
        if [ -n "$owner" ] && ! kill -0 "$owner" 2>/dev/null; then
            rm -rf "$d"
            continue
        fi
        i=$((i + 1))
        [ $i -gt 1200 ] && { say "lock $d held by ${owner:-?} for 10 min" >&2; return 1; }
        sleep 0.5
    done
    echo $$ >"$d/pid"
}

rig_unlock() { rm -rf "$1"; }

rig_flush() { # stage dest-url label src[=prefix]...
    local stage=$1 dest=$2 label=$3 snap rc listing objects bytes
    shift 3
    mkdir -p "$stage"
    rig_lock "$STATE/flush.lock" || return 1
    snap=$(python3 "$STATE/shipper_loops.py" snapshot "$stage" "$STATE/hashcache-$label.json" "$@")
    rc=$?
    if [ $rc -ne 0 ]; then rig_unlock "$STATE/flush.lock"; say "flush $label: snapshot failed" >&2; return 1; fi
    "$AWS_BIN" s3 sync "$stage/" "$dest" --exclude SHA256SUMS --no-progress --only-show-errors --region "$REGION"
    rc=$?
    if [ $rc -ne 0 ]; then rig_unlock "$STATE/flush.lock"; say "flush $label: aws s3 sync exit $rc" >&2; return 1; fi
    "$AWS_BIN" s3 cp "$stage/SHA256SUMS" "${dest}SHA256SUMS" --no-progress --only-show-errors --region "$REGION"
    rc=$?
    if [ $rc -ne 0 ]; then rig_unlock "$STATE/flush.lock"; say "flush $label: SHA256SUMS upload exit $rc" >&2; return 1; fi
    rig_unlock "$STATE/flush.lock"
    listing=$("$AWS_BIN" s3 ls "$dest" --recursive --summarize --region "$REGION")
    rc=$?
    if [ $rc -ne 0 ]; then say "flush $label: uploaded, but the bucket listing failed (exit $rc) — count UNKNOWN" >&2; return 1; fi
    objects=$(printf '%s\n' "$listing" | sed -n 's/^ *Total Objects: *\([0-9][0-9]*\).*/\1/p')
    bytes=$(printf '%s\n' "$listing" | sed -n 's/^ *Total Size: *\([0-9][0-9]*\).*/\1/p')
    [ -n "$objects" ] && [ -n "$bytes" ] || { say "flush $label: no Total Objects/Size in the listing — count UNKNOWN" >&2; return 1; }
    date -u +%Y-%m-%dT%H:%M:%SZ >"$STATE/last-flush-$label"
    say "flush $label: $dest objects=$objects bytes=$bytes (stage: $snap + SHA256SUMS)"
    python3 - "$snap" "$objects" <<'PY' || say "flush $label: WARNING the bucket holds fewer objects than the stage" >&2
import sys
files = int(dict(kv.split("=") for kv in sys.argv[1].split())["files"])
sys.exit(0 if int(sys.argv[2]) >= files + 1 else 1)
PY
    return 0
}

# ------------------------------------------------------------ the loops --

run_dmesg() {
    local sb first=1
    local -a pre=()
    sb=$(rig_stdbuf)
    [ -n "$sb" ] && pre=("$sb" -oL)
    while :; do
        if [ $first = 1 ]; then
            ${pre[@]+"${pre[@]}"} "$DMESG_BIN" -w --time-format iso >>"$EVID/dmesg.log"
        else
            ${pre[@]+"${pre[@]}"} "$DMESG_BIN" -W --time-format iso >>"$EVID/dmesg.log"
        fi
        first=0
        sleep 2
    done
}

run_journal() {
    local sb child=
    local -a pre=()
    sb=$(rig_stdbuf)
    [ -n "$sb" ] && pre=("$sb" -oL)
    # TERM (from stop) reaches journalctl too; it saves its cursor on the way out
    trap '[ -n "$child" ] && kill -TERM "$child" 2>/dev/null; wait; exit 0' TERM
    while :; do
        ${pre[@]+"${pre[@]}"} "$JOURNALCTL_BIN" -b -f --cursor-file="$STATE/journal.cursor" \
            -u kubelet -u containerd -o short-iso-precise >>"$EVID/journal.log" &
        child=$!
        wait "$child"
        sleep 2
    done
}

run_upload() {
    trap 'exit 0' TERM
    while :; do
        sleep "$UPLOAD_SECS" &
        wait $!
        shipper_flush || say "periodic flush failed; next in $UPLOAD_SECS s" >&2
    done
}

rig_write_loops_py() {
    mkdir -p "$STATE"
    cat >"$STATE/shipper_loops.py.tmp" <<'PYEOF'
#!/usr/bin/env python3
"""The python halves of shipper.sh and podwatch.sh (written by them at start)."""
import hashlib
import json
import os
import re
import shutil
import signal
import stat
import subprocess
import sys
import time


def now_ms():
    return int(time.time() * 1000)


def log(msg):
    print(f"{time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())} {msg}", file=sys.stderr, flush=True)


def write_atomic(path, data):
    tmp = f"{path}.tmp.{os.getpid()}"
    with open(tmp, "wb") as f:
        f.write(data)
    os.replace(tmp, path)


def pid_alive(pid, expect=None):
    if not pid:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    if os.path.isdir("/proc/self"):
        try:
            with open(f"/proc/{pid}/stat") as f:
                s = f.read()
            if s[s.rindex(")") + 2] == "Z":
                return False
            if expect:
                with open(f"/proc/{pid}/cmdline", "rb") as f:
                    argv0 = f.read().split(b"\0")[0]
                return expect.encode() in os.path.basename(argv0)
        except (OSError, ValueError):
            return False
    return True


# ------------------------------------------------------------ log tails --

NAME_LIVE = re.compile(r"^(\d+)\.log$")
NAME_ROT = re.compile(r"^(\d+)\.log\.(\d{8}-\d{6})$")
NAME_GZ = re.compile(r"^(\d+)\.log\.(\d{8}-\d{6})\.gz$")


class Registry:
    def __init__(self, state):
        self.dir = os.path.join(state, "tails")
        os.makedirs(self.dir, exist_ok=True)
        self.entries = {}
        for f in os.listdir(self.dir):
            if f.endswith(".json"):
                try:
                    with open(os.path.join(self.dir, f)) as fh:
                        e = json.load(fh)
                    self.entries[e["key"]] = e
                except (OSError, ValueError, KeyError):
                    log(f"registry: unreadable {f}")

    def save(self, e):
        write_atomic(os.path.join(self.dir, e["id"] + ".json"), json.dumps(e, sort_keys=True).encode())


def out_size(e):
    try:
        return os.path.getsize(e["out"])
    except OSError:
        return 0


class Logs:
    def __init__(self, pods_root, evid, state, ns_re, tail_bin, stdbuf_bin):
        self.pods_root, self.evid, self.state = pods_root, evid, state
        self.ns_re = re.compile(ns_re)
        self.reg = Registry(state)
        self.procs = {}
        sb = shutil.which(stdbuf_bin) if stdbuf_bin else None
        self.cmd = ([sb, "-o0"] if sb else []) + [tail_bin]

    def scan(self):
        live, gz = {}, []
        try:
            pods = list(os.scandir(self.pods_root))
        except FileNotFoundError:
            return live, gz
        for pd in pods:
            if not pd.is_dir(follow_symlinks=False) or not self.ns_re.match(pd.name.split("_", 1)[0]):
                continue
            try:
                containers = list(os.scandir(pd.path))
            except (FileNotFoundError, NotADirectoryError):
                continue
            for c in containers:
                if not c.is_dir(follow_symlinks=False):
                    continue
                try:
                    files = list(os.scandir(c.path))
                except (FileNotFoundError, NotADirectoryError):
                    continue
                for f in files:
                    try:
                        st = f.stat(follow_symlinks=False)
                    except FileNotFoundError:
                        continue
                    if not stat.S_ISREG(st.st_mode):
                        continue
                    m = NAME_LIVE.match(f.name) or NAME_ROT.match(f.name)
                    if m:
                        key = f"{st.st_dev}:{st.st_ino}:{pd.name}/{c.name}/{m.group(1)}"
                        live[key] = (f.path, f.name, st, pd.name, c.name, m.group(1))
                    elif NAME_GZ.match(f.name):
                        gz.append((f.path, f.name, st, pd.name, c.name, NAME_GZ.match(f.name).group(1)))
        return live, gz

    def alive(self, e):
        p = self.procs.get(e["key"])
        if p is not None:
            return p.poll() is None
        return pid_alive(e.get("pid"), expect="tail")

    def launch(self, e, path, st):
        offset = out_size(e)
        try:
            fd = os.open(path, os.O_RDONLY)
        except OSError as err:
            log(f"open {path}: {err}")
            return False
        try:
            fst = os.fstat(fd)
            if (fst.st_dev, fst.st_ino) != (st.st_dev, st.st_ino):
                return False  # the name moved on between the scan and the open; next pass
            if offset > fst.st_size:
                e["anomaly"] = f"output {offset} bytes > source {fst.st_size}; not resumed"
                self.reg.save(e)
                log(f"ANOMALY {e['out']}: {e['anomaly']}")
                return False
            os.makedirs(os.path.dirname(e["out"]), exist_ok=True)
            outfd = os.open(e["out"], os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
            errfd = os.open(os.path.join(self.state, "logs", "tails.err"), os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
            try:
                p = subprocess.Popen(self.cmd + ["-c", f"+{offset + 1}", "-f"], stdin=fd, stdout=outfd,
                                     stderr=errfd, close_fds=True)
            finally:
                os.close(outfd)
                os.close(errfd)
        finally:
            os.close(fd)
        self.procs[e["key"]] = p
        e["pid"] = p.pid
        e["stopped"] = False
        e["launches"].append({"ms": now_ms(), "offset": offset, "pid": p.pid})
        self.reg.save(e)
        log(f"tail pid {p.pid} {'resumed at ' + str(offset) if offset else 'from 0'}: {path} -> {e['out']}")
        return True

    def one_pass(self):
        live, gz = self.scan()
        entries = self.reg.entries
        for key, (path, name, st, pd, c, n) in live.items():
            e = entries.get(key)
            if e is None:
                out_dir = os.path.join(self.evid, "pods", pd, c)
                out = os.path.join(out_dir, f"{n}.log")
                if os.path.exists(out) or any(x.get("out") == out for x in entries.values()):
                    out = os.path.join(out_dir, f"{n}.log.i{st.st_ino}")
                e = {"id": hashlib.sha1(key.encode()).hexdigest()[:20], "key": key, "kind": "tail",
                     "pod_dir": pd, "container": c, "n": n, "names": [name], "out": out, "pid": None,
                     "first_seen_ms": now_ms(), "vanished_ms": None, "launches": [], "lost_after": None,
                     "stopped": False}
                entries[key] = e
                self.launch(e, path, st)
                continue
            if name not in e["names"]:
                e["names"].append(name)
                self.reg.save(e)
            if not self.alive(e) and not e.get("anomaly"):
                self.launch(e, path, st)
        for key, e in list(entries.items()):
            if e.get("kind") != "tail" or key in live:
                continue
            if e.get("vanished_ms"):
                if (not e.get("stopped") and e.get("lost_after") is None and e.get("died_after_vanish") is None
                        and not self.alive(e)):
                    e["died_after_vanish"] = out_size(e)
                    log(f"tail of a vanished file died; the copy ends at {out_size(e)} and is complete only "
                        f"if it had caught up: {e['out']}")
                    self.reg.save(e)
                continue
            e["vanished_ms"] = now_ms()
            if not self.alive(e):
                if e.get("stopped"):
                    e["vanished_while_stopped_at"] = out_size(e)
                    log(f"file vanished while the shipper was stopped; copy ends at {out_size(e)}: {e['out']}")
                else:
                    e["lost_after"] = out_size(e)
                    log(f"LOSS: tail was dead when its file vanished; bytes after {out_size(e)} unrecoverable: {e['out']}")
            self.reg.save(e)
        for path, name, st, pd, c, n in gz:
            gkey = f"gz:{st.st_dev}:{st.st_ino}:{pd}/{c}/{name}"
            if gkey in entries:
                continue
            base = name[:-3]
            tailed = any(x.get("kind") == "tail" and x["pod_dir"] == pd and x["container"] == c and base in x["names"]
                         for x in entries.values())
            e = {"id": hashlib.sha1(gkey.encode()).hexdigest()[:20], "key": gkey, "kind": "gz", "pod_dir": pd,
                 "container": c, "n": n, "names": [name], "first_seen_ms": now_ms(),
                 "out": None if tailed else os.path.join(self.evid, "pods", pd, c, name)}
            if not tailed:
                try:
                    os.makedirs(os.path.dirname(e["out"]), exist_ok=True)
                    shutil.copyfile(path, e["out"] + ".tmp")
                    os.replace(e["out"] + ".tmp", e["out"])
                    log(f"copied rotated {path}")
                except OSError as err:
                    log(f"copy {path}: {err}")
                    continue
            entries[gkey] = e
            self.reg.save(e)
        for k, p in list(self.procs.items()):
            if p.poll() is not None:
                del self.procs[k]


def cmd_logs(a):
    lg = Logs(a[0], a[1], a[2], a[3], a[4], a[5])
    interval = float(a[6])
    log(f"logs: {a[0]} ns~{a[3]} -> {a[1]}/pods every {interval}s; {len(lg.reg.entries)} registry entries")
    while True:
        t = time.monotonic()
        try:
            lg.one_pass()
        except Exception as e:  # never die on one bad pass
            log(f"logs pass failed: {type(e).__name__}: {e}")
        time.sleep(max(0.0, interval - (time.monotonic() - t)))


def fd0_position(pid):
    """(read position, size) of a tail's stdin on Linux, else None."""
    try:
        with open(f"/proc/{pid}/fdinfo/0") as f:
            pos = int(next(l for l in f if l.startswith("pos:")).split()[1])
        return pos, os.stat(f"/proc/{pid}/fd/0").st_size
    except (OSError, StopIteration, ValueError):
        return None


def cmd_catchup(a):
    reg = Registry(a[0])
    timeout = float(a[1])
    alive = [e for e in reg.entries.values() if e.get("kind") == "tail" and pid_alive(e.get("pid"), "tail")]
    deadline = time.monotonic() + timeout
    last = {}
    while True:
        pending = []
        for e in alive:
            if not pid_alive(e["pid"], "tail"):
                continue
            pz = fd0_position(e["pid"])
            size = out_size(e)
            if pz is not None:
                if not (pz[0] >= pz[1] and size == pz[0]):
                    pending.append((e["out"], f"read {pz[0]} of {pz[1]}, copied {size}"))
            else:  # no /proc: the copy must hold still across two tail polls
                prev = last.get(e["key"])
                last[e["key"]] = (size, time.monotonic())
                if prev is None or prev[0] != size or time.monotonic() - prev[1] < 2.2:
                    if prev is not None and prev[0] == size:
                        last[e["key"]] = prev
                    pending.append((e["out"], f"copied {size}, not yet stable"))
        if not pending:
            print(f"catchup: {len(alive)} tails at the end of their files")
            return 0
        if time.monotonic() > deadline:
            for out, why in pending:
                print(f"catchup: LAGGING {out}: {why}")
            return 1
        time.sleep(0.3)


def cmd_tails_kill(a):
    reg = Registry(a[0])
    killed = []
    for e in reg.entries.values():
        if e.get("kind") == "tail" and pid_alive(e.get("pid"), "tail"):
            try:
                os.kill(e["pid"], signal.SIGTERM)
                killed.append(e)
            except ProcessLookupError:
                pass
    end = time.monotonic() + 5
    while time.monotonic() < end and any(pid_alive(e["pid"], "tail") for e in killed):
        time.sleep(0.1)
    for e in killed:
        if pid_alive(e["pid"], "tail"):
            os.kill(e["pid"], signal.SIGKILL)
    for e in reg.entries.values():
        if e.get("kind") == "tail":
            e["stopped"] = True
            reg.save(e)
    print(f"tails stopped: {len(killed)}")
    return 0


def cmd_tails_status(a):
    reg = Registry(a[0])
    tails = [e for e in reg.entries.values() if e.get("kind") == "tail"]
    alive = [e for e in tails if pid_alive(e.get("pid"), "tail")]
    held = [e for e in alive if e.get("vanished_ms")]
    lost = [e for e in tails if e.get("lost_after") is not None]
    maybe = [e for e in tails if e.get("died_after_vanish") is not None]
    anomalies = [e for e in tails if e.get("anomaly")]
    gz = [e for e in reg.entries.values() if e.get("kind") == "gz" and e.get("out")]
    print(f"tails: registered={len(tails)} alive={len(alive)} vanished-held-open={len(held)} "
          f"lost={len(lost)} died-after-vanish={len(maybe)} anomalies={len(anomalies)} rotated-gz-copied={len(gz)}")
    for e in lost:
        print(f"  LOST after byte {e['lost_after']}: {e['out']}")
    for e in maybe:
        print(f"  DIED AFTER ITS FILE VANISHED (complete only if caught up) at byte {e['died_after_vanish']}: {e['out']}")
    for e in anomalies:
        print(f"  ANOMALY {e['out']}: {e['anomaly']}")
    return 0


# -------------------------------------------------------------- cgroups --

POD_UID = re.compile(r"pod([0-9a-f]{8}[-_][0-9a-f]{4}[-_][0-9a-f]{4}[-_][0-9a-f]{4}[-_][0-9a-f]{12})")
CID = re.compile(r"(?:cri-containerd-|docker-|crio-)?([0-9a-f]{64})(?:\.scope)?$")


def read_first(path):
    try:
        with open(path) as f:
            return f.read().strip()
    except OSError:
        return None


def as_int(v):
    try:
        return int(v)
    except (TypeError, ValueError):
        return None


def kv_file(path):
    out = {}
    txt = read_first(path)
    for line in (txt or "").splitlines():
        parts = line.split()
        if len(parts) == 2:
            out[parts[0]] = as_int(parts[1])
    return out


def cgroup_lines(root):
    t = now_ms()
    lines = []
    try:
        tops = [os.path.join(root, d) for d in os.listdir(root) if d.startswith("kubepods")]
    except OSError:
        tops = []
    for top in sorted(tops):
        for dirpath, dirnames, filenames in os.walk(top):
            dirnames.sort()
            if "memory.current" not in filenames:
                continue
            rel = os.path.relpath(dirpath, root)
            m = POD_UID.search(rel)
            c = CID.search(os.path.basename(dirpath))
            mx = read_first(os.path.join(dirpath, "memory.max"))
            ev = kv_file(os.path.join(dirpath, "memory.events"))
            lines.append({
                "ts_ms": t, "path": rel,
                "pod_uid": m.group(1).replace("_", "-") if m else None,
                "container_id": c.group(1) if c else None,
                "memory_current": as_int(read_first(os.path.join(dirpath, "memory.current"))),
                "memory_peak": as_int(read_first(os.path.join(dirpath, "memory.peak"))),
                "memory_max": None if mx in (None, "max") else as_int(mx),
                "cpu_usage_usec": kv_file(os.path.join(dirpath, "cpu.stat")).get("usage_usec"),
                "oom": ev.get("oom"), "oom_kill": ev.get("oom_kill"),
            })
    return lines


def cmd_cgroups(a):
    root, out, interval = a[0], a[1], float(a[2])
    warned = False
    log(f"cgroups: {root} -> {out} every {interval}s")
    while True:
        t = time.monotonic()
        lines = cgroup_lines(root)
        if lines:
            with open(out, "a") as f:
                for l in lines:
                    f.write(json.dumps(l, sort_keys=True) + "\n")
            warned = False
        elif not warned:
            log(f"cgroups: no kubepods cgroup with memory.current under {root}")
            warned = True
        time.sleep(max(0.0, interval - (time.monotonic() - t)))


# --------------------------------------------------------------- chrony --

CHRONY_FIELDS = ["ref_id", "ref_name", "stratum", "ref_time_s", "system_time_s", "last_offset_s", "rms_offset_s",
                 "freq_ppm", "resid_freq_ppm", "skew_ppm", "root_delay_s", "root_dispersion_s",
                 "update_interval_s", "leap"]


def parse_chrony_tracking(csv_line, ts_ms):
    """One `chronyc -c tracking` line -> {ts_ms, offset_ms, ...}. offset_ms is
    node clock minus true time: chronyc's field 5 is positive when the clock
    is SLOW, so offset_ms = -field5 * 1000."""
    parts = csv_line.strip().split(",")
    if len(parts) != len(CHRONY_FIELDS):
        raise ValueError(f"expected {len(CHRONY_FIELDS)} fields, got {len(parts)}: {csv_line.strip()!r}")
    o = {"ts_ms": ts_ms}
    for k, v in zip(CHRONY_FIELDS, parts):
        if k in ("ref_id", "ref_name", "leap"):
            o[k] = v
        elif k == "stratum":
            o[k] = int(v)
        else:
            o[k] = float(v)
    o["offset_ms"] = -o["system_time_s"] * 1000.0
    o["synced"] = o["leap"] != "Not synchronised"
    return o


def cmd_chrony(a):
    chronyc, out, errs, interval = a[0], a[1], a[2], float(a[3])
    log(f"chrony: {chronyc} -c tracking -> {out} every {interval}s")
    while True:
        t = time.monotonic()
        t0 = now_ms()
        try:
            r = subprocess.run([chronyc, "-c", "tracking"], capture_output=True, text=True, timeout=10)
            t1 = now_ms()
            if r.returncode != 0:
                raise RuntimeError(f"exit {r.returncode}: {r.stderr.strip()[:200]}")
            line = parse_chrony_tracking(r.stdout.strip().splitlines()[-1], (t0 + t1) // 2)
            with open(out, "a") as f:
                f.write(json.dumps(line, sort_keys=True) + "\n")
        except Exception as e:
            with open(errs, "a") as f:
                f.write(json.dumps({"ts_ms": now_ms(), "error": f"{type(e).__name__}: {e}"}) + "\n")
        time.sleep(max(0.0, interval - (time.monotonic() - t)))


# ------------------------------------------------------------- snapshot --

def cmd_snapshot(a):
    """snapshot STAGE CACHE SRC[=PREFIX]... — copy every changed file of each
    SRC into STAGE[/PREFIX] (atomically, hashing the bytes copied), keep the
    rest, and write STAGE/SHA256SUMS over every staged file. The sums describe
    the STAGE, which is exactly what `aws s3 sync` uploads — never a live file
    that grew between hashing and upload."""
    stage, cache_path = a[0], a[1]
    try:
        with open(cache_path) as f:
            cache = json.load(f)
    except (OSError, ValueError):
        cache = {}
    changed = 0
    for spec in a[2:]:
        src, _, prefix = spec.partition("=")
        if os.path.isdir(src):
            changed += snapshot_tree(src, prefix, stage, cache)
    total = 0
    lines = []
    for rel in sorted(cache):
        if os.path.exists(os.path.join(stage, rel)):
            lines.append(f"{cache[rel]['sha256']}  {rel}\n")
            total += cache[rel]["bytes"]
    write_atomic(os.path.join(stage, "SHA256SUMS"), "".join(lines).encode())
    write_atomic(cache_path, json.dumps(cache).encode())
    print(f"files={len(lines)} bytes={total} changed={changed}")
    return 0


def snapshot_tree(src, prefix, stage, cache):
    changed = 0
    for dirpath, dirnames, filenames in os.walk(src):
        dirnames.sort()
        for fn in filenames:
            p = os.path.join(dirpath, fn)
            rel = os.path.normpath(os.path.join(prefix, os.path.relpath(p, src)))
            if rel == "SHA256SUMS" or ".tmp." in fn or fn.endswith(".tmp"):
                continue
            try:
                st = os.stat(p)
            except FileNotFoundError:
                continue
            if not stat.S_ISREG(st.st_mode):
                continue
            c = cache.get(rel)
            dst = os.path.join(stage, rel)
            if c and c["src_size"] == st.st_size and c["src_mtime_ns"] == st.st_mtime_ns and os.path.exists(dst):
                continue
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            h = hashlib.sha256()
            n = 0
            tmp = dst + ".stage-tmp"
            with open(p, "rb") as fi, open(tmp, "wb") as fo:
                while True:
                    b = fi.read(1 << 20)
                    if not b:
                        break
                    h.update(b)
                    fo.write(b)
                    n += len(b)
            os.replace(tmp, dst)
            cache[rel] = {"src_size": st.st_size, "src_mtime_ns": st.st_mtime_ns, "sha256": h.hexdigest(), "bytes": n}
            changed += 1
    return changed


# ------------------------------------------------------ kubectl streams --

STRIP_ANN = ("kubectl.kubernetes.io/last-applied-configuration",)


def compact_container(cs):
    return {"name": cs.get("name"), "restarts": cs.get("restartCount"), "ready": cs.get("ready"),
            "state": cs.get("state"), "lastState": cs.get("lastState"), "image": cs.get("image"),
            "imageID": cs.get("imageID"), "containerID": cs.get("containerID")}


def compact_pod(p, src, ts):
    md, spec, st = p.get("metadata", {}), p.get("spec", {}), p.get("status", {})
    ann = {k: v for k, v in (md.get("annotations") or {}).items() if k not in STRIP_ANN}
    css = st.get("containerStatuses") or []
    return {"ts_ms": ts, "src": src, "ns": md.get("namespace"), "name": md.get("name"), "uid": md.get("uid"),
            "node": spec.get("nodeName"), "phase": st.get("phase"), "reason": st.get("reason"),
            "deleted": md.get("deletionTimestamp"), "created": md.get("creationTimestamp"),
            "restarts": sum((c.get("restartCount") or 0) for c in css),
            "lastState": {c.get("name"): c.get("lastState") for c in css if c.get("lastState")},
            "annotations": ann, "labels": md.get("labels") or {},
            "owner": [f"{o.get('kind')}/{o.get('name')}" for o in md.get("ownerReferences") or []],
            "containers": [compact_container(c) for c in css],
            "initContainers": [compact_container(c) for c in st.get("initContainerStatuses") or []]}


def emit(obj, kind, src):
    ts = now_ms()
    if isinstance(obj, dict) and obj.get("kind", "").endswith("List") and isinstance(obj.get("items"), list):
        for it in obj["items"]:
            emit(it, kind, src)
        return
    if isinstance(obj, dict) and "object" in obj and "type" in obj and isinstance(obj["object"], dict):
        wtype, obj = obj["type"], obj["object"]
    else:
        wtype = None
    if kind == "pod":
        line = compact_pod(obj, src, ts)
        if wtype:
            line["watch_type"] = wtype
    else:
        obj.get("metadata", {}).pop("managedFields", None)
        line = dict(obj, observed_ms=ts)
    sys.stdout.write(json.dumps(line, sort_keys=True) + "\n")
    sys.stdout.flush()


def cmd_jsonstream(a):
    """Concatenated (pretty-printed) JSON objects on stdin -> one compact line each."""
    kind, src = a[0], (a[1] if len(a) > 1 else "watch")
    dec = json.JSONDecoder()
    buf = ""
    while True:
        chunk = sys.stdin.readline()
        if not chunk:
            break
        buf += chunk
        while True:
            s = buf.lstrip()
            if not s:
                buf = ""
                break
            try:
                obj, end = dec.raw_decode(s)
            except ValueError:
                buf = s
                break
            emit(obj, kind, src)
            buf = s[end:]
    if buf.strip():
        log(f"jsonstream: {len(buf)} trailing bytes that never parsed")
    return 0


COMMANDS = {"logs": cmd_logs, "catchup": cmd_catchup, "tails-kill": cmd_tails_kill, "tails-status": cmd_tails_status,
            "cgroups": cmd_cgroups, "chrony": cmd_chrony, "snapshot": cmd_snapshot, "jsonstream": cmd_jsonstream}

if __name__ == "__main__":
    sys.exit(COMMANDS[sys.argv[1]](sys.argv[2:]) or 0)
PYEOF
    mv "$STATE/shipper_loops.py.tmp" "$STATE/shipper_loops.py"
}

# ------------------------------------------------------------ commands --

shipper_flush() { # the evidence, plus the shipper's own record of it: loop logs and the tail
    # registry (launch offsets, lost_after, died_after_vanish) under _shipper/
    rig_flush "$STATE/stage" "s3://$BUCKET/_rig/evidence/$NODE/" evidence \
        "$EVID" "$STATE/logs=_shipper/logs" "$STATE/tails=_shipper/tails"
}

shipper_env_check() {
    local recorded=""
    [ -s "$STATE/node" ] && recorded=$(cat "$STATE/node")
    if [ -n "$recorded" ]; then
        # One STATE, one stage, one name: a flush under another NODE would upload the same stage to a
        # second prefix, and every trace would be read twice. The name recorded at the first start wins.
        if [ -n "${NODE:-}" ] && [ "$NODE" != "$recorded" ]; then
            say "WARNING: NODE=$NODE differs from the name this shipper started under ($recorded); using $recorded" >&2
        fi
        NODE=$recorded
    fi
    rig_require_name "${NODE:-}" NODE
    rig_require_name "${BUCKET:-}" BUCKET
    rig_require_offroot "$EVID" "$STATE"
    mkdir -p "$EVID" "$STATE/pids" "$STATE/logs"
    [ -n "$recorded" ] || printf '%s\n' "$NODE" >"$STATE/node"
}

shipper_start_loop() { # name
    local py="$STATE/shipper_loops.py"
    if rig_alive "$1"; then
        say "$1: already running (pid $(cat "$(rig_pidfile "$1")"))"
        return 0
    fi
    case "$1" in
        dmesg) rig_detach dmesg bash "$SELF" _run dmesg ;;
        journal) rig_detach journal bash "$SELF" _run journal ;;
        upload) rig_detach upload bash "$SELF" _run upload ;;
        logs) rig_detach logs python3 "$py" logs "$PODS_ROOT" "$EVID" "$STATE" "$NS_RE" "$TAIL_BIN" "$STDBUF_BIN" "$DISCOVER_SECS" ;;
        cgroups) rig_detach cgroups python3 "$py" cgroups "$CGROUP_ROOT" "$EVID/cgroups.jsonl" "$CGROUP_SECS" ;;
        chrony) rig_detach chrony python3 "$py" chrony "$CHRONYC_BIN" "$EVID/chrony.jsonl" "$EVID/chrony-errors.jsonl" "$CHRONY_SECS" ;;
    esac
    say "$1: started (pid $(cat "$(rig_pidfile "$1")"))"
}

shipper_main() {
    local cmd=${1:-}
    case "$cmd" in
        start)
            shipper_env_check
            rig_write_loops_py
            for l in $SHIPPER_LOOPS; do shipper_start_loop "$l"; done
            ;;
        flush)
            shipper_env_check
            [ -f "$STATE/shipper_loops.py" ] || rig_write_loops_py
            shipper_flush
            ;;
        stop)
            shipper_env_check
            [ -f "$STATE/shipper_loops.py" ] || rig_write_loops_py
            rig_kill_loop logs pid-only
            rig_kill_loop upload
            python3 "$STATE/shipper_loops.py" catchup "$STATE" "$CATCHUP_SECS" || say "stop: some tails had not caught up after ${CATCHUP_SECS}s (listed above)" >&2
            python3 "$STATE/shipper_loops.py" tails-kill "$STATE"
            for l in cgroups chrony dmesg journal; do rig_kill_loop "$l"; done
            shipper_flush
            ;;
        status)
            local l rc=0
            for l in $SHIPPER_LOOPS; do
                if rig_alive "$l"; then
                    say "$l: alive (pid $(cat "$(rig_pidfile "$l")"))"
                else
                    say "$l: DEAD"
                    rc=1
                fi
            done
            if [ -f "$STATE/shipper_loops.py" ]; then
                python3 "$STATE/shipper_loops.py" tails-status "$STATE"
            fi
            say "last flush: $(cat "$STATE/last-flush-evidence" 2>/dev/null || echo never)"
            return $rc
            ;;
        _run)
            case "${2:-}" in
                dmesg) run_dmesg ;;
                journal) run_journal ;;
                upload) run_upload ;;
                *) die "unknown loop ${2:-}" ;;
            esac
            ;;
        *)
            sed -n '2,30p' "$SELF" >&2
            exit 2
            ;;
    esac
}

if [ "${SHIPPER_LIB_ONLY:-0}" != 1 ]; then
    shipper_main "$@"
fi
