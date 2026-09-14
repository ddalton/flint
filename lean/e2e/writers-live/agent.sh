#!/bin/sh
# writers-live agent: one simulated AI agent editing a shared lean
# workspace through FILES only. README.md §1 is the contract; §5 records
# the implementation decisions (the race-checked base read, the temp
# names, the extra journal kinds).
#
# POSIX sh for BusyBox ash: no arrays, no [[ ]], no $RANDOM, no `local`.
# Every random choice comes from a seeded LCG, so a run replays from
# AGENT_SEED (given the same tree evolution). Nothing is ever logged into
# the tree: diagnostics go to stderr, evidence to $JOURNAL.
#
#   agent.sh                   run the agent (environment below)
#   agent.sh parse-ack FILE    print the fields the agent journals from an ack
#   agent.sh rand SEED COUNT   print COUNT draws in [0,100) from the LCG
#
# Sourced with AGENT_LIB_ONLY=1 (ui.sh does), it only defines functions.
#
# Environment: AGENT_ID, AGENT_MODE (disjoint|hot|churn|vocab), AGENT_SEED,
# TREE, JOURNAL (/agent/journal.jsonl), CONTROL_DIR (/agent),
# OPS_PER_BATCH (5), BATCH_SLEEP_MIN_MS (3000), BATCH_SLEEP_MAX_MS (10000),
# FLOOR_SECS (5; ack timeout = 3 x FLOOR_SECS unless ACK_TIMEOUT_SECS),
# ACK_POLL_MS (200), AGENT_PATHS (paths in the mode's set), AGENT_MIX
# ("write delete mv same" percentages), READY_TIMEOUT_SECS (600),
# MAX_BATCHES (0 = until stopped), RACE_TRIES (5).

set -u
set -f

TMP_SUFFIX=".flint-sync-tmp"   # the syncer's scan skips this suffix (scan.rs)

# ---------- time -----------------------------------------------------------

TIME_MODE=""
NOW=0
detect_time() {
    _t=$(date +%s%N 2>/dev/null) || _t=""
    case "$_t" in
        ''|*[!0-9]*) ;;
        *) if [ "${#_t}" -ge 19 ]; then TIME_MODE=ns; return 0; fi ;;
    esac
    if command -v gdate >/dev/null 2>&1; then
        _t=$(gdate +%s%N 2>/dev/null) || _t=""
        case "$_t" in ''|*[!0-9]*) ;; *) TIME_MODE=gdate; return 0 ;; esac
    fi
    if [ -r /proc/uptime ]; then
        # A BusyBox without %N: anchor /proc/uptime (10 ms ticks) to the
        # wall clock at a second boundary, then read it with no fork.
        _s0=$(date +%s); _s1=$_s0
        while [ "$_s1" = "$_s0" ]; do _s1=$(date +%s); done
        uptime_ms; UPTIME_EPOCH_MS=$((_s1 * 1000 - _up))
        TIME_MODE=uptime; return 0
    fi
    if command -v perl >/dev/null 2>&1; then TIME_MODE=perl; return 0; fi
    TIME_MODE=sec
}
uptime_ms() {  # sets _up: /proc/uptime in ms
    read -r _u _rest < /proc/uptime
    _f=${_u#*.}; _f=$(( 1$_f - 100 ))
    _up=$(( ${_u%.*} * 1000 + _f * 10 ))
}
now_ms() {
    case "$TIME_MODE" in
        ns) NOW=$(date +%s%N); NOW=$((NOW / 1000000)) ;;
        gdate) NOW=$(gdate +%s%N); NOW=$((NOW / 1000000)) ;;
        uptime) uptime_ms; NOW=$((UPTIME_EPOCH_MS + _up)) ;;
        perl) NOW=$(perl -MTime::HiRes=time -e 'printf("%d\n", time()*1000)') ;;
        *) NOW=$(date +%s); NOW=$((NOW * 1000)) ;;
    esac
}
sleep_ms() {
    sleep "$(($1 / 1000)).$(printf '%03d' $(($1 % 1000)))"
}

# ---------- randomness: a seeded LCG (glibc constants, mod 2^31) ----------

RS=1
R=0
seed_lcg() {   # $1 seed, $2 salt (the batch a resumed agent starts from)
    RS=$(( ($1 % 2147483648) * 2654435761 % 2147483648 ))
    RS=$(( (RS + ($2 % 2147483648) * 40503 + 1) % 2147483648 ))
    rand_below 2; rand_below 2; rand_below 2
}
rand_below() { # sets R in [0, $1); uses bits 16..30, the LCG's good bits
    RS=$(( (RS * 1103515245 + 12345) % 2147483648 ))
    R=$(( (RS / 65536) % $1 ))
}

# ---------- hashing and inodes ---------------------------------------------

SHA_CMD=""
detect_sha() {
    if command -v sha256sum >/dev/null 2>&1; then SHA_CMD="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then SHA_CMD="shasum -a 256"
    else echo "agent: no sha256sum or shasum" >&2; exit 2
    fi
}
H=""
sha_file() {   # sets H: 64 hex, "absent" (no regular file), or "" (read raced)
    if [ ! -f "$1" ]; then H=absent; return 0; fi
    H=$( { $SHA_CMD < "$1"; } 2>/dev/null ) || H=""
    H=${H%% *}
    case "$H" in *[!0-9a-f]*) H="" ;; esac
    [ "${#H}" -eq 64 ] || H=""
}
INO=""
ino_of() {     # sets INO to the inode number at $1, "" when nothing is there
    INO=""
    set -- $(ls -di "$1" 2>/dev/null)
    INO=${1:-}
}

# ---------- ack parsing (compact or pretty serde JSON, no jq) --------------

json_array() { # $1 flattened JSON, $2 key -> the array's items, e.g. "a","b"
    printf '%s' "$1" \
        | sed -n 's/.*"'"$2"'"[[:space:]]*:[[:space:]]*\[\([^]]*\)].*/\1/p' \
        | sed 's/"[[:space:]]*,[[:space:]]*"/","/g; s/^[[:space:]]*//; s/[[:space:]]*$//'
}
json_scalar() { # $1 flattened JSON, $2 key -> a bare number/true/false, or ""
    printf '%s' "$1" | sed -n 's/.*"'"$2"'"[[:space:]]*:[[:space:]]*\([0-9a-z][0-9a-z]*\).*/\1/p'
}
parse_ack() {  # $1 ack file -> ACK_* variables
    _flat=$(tr -d '\n\r' < "$1" 2>/dev/null) || _flat=""
    ACK_STATUS=$(printf '%s' "$_flat" | sed -n 's/.*"status"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
    ACK_SEQ=$(json_scalar "$_flat" seq)
    case "$ACK_SEQ" in ''|*[!0-9]*) ACK_SEQ=null ;; esac
    ACK_NONCES=$(json_array "$_flat" nonces)
    ACK_DROPPED=$(json_array "$_flat" dropped)
    ACK_UPLOADED=$(json_scalar "$_flat" uploaded)
    ACK_DELETED=$(json_scalar "$_flat" deleted)
    ACK_PARKED=$(json_scalar "$_flat" parked)
    ACK_CONSUMED=$(json_scalar "$_flat" consumed)
    ACK_NO_CHANGE=$(json_scalar "$_flat" no_change)
    for _v in ACK_UPLOADED ACK_DELETED ACK_PARKED ACK_CONSUMED ACK_NO_CHANGE; do
        eval "_x=\$$_v"
        case "$_x" in ''|*[!0-9a-z]*) eval "$_v=null" ;; esac
    done
}
ack_covers() { # $1 nonce; true iff the parsed ack names it exactly
    case ",$ACK_NONCES," in *",\"$1\","*) return 0 ;; esac
    return 1
}

# ---------- journal ----------------------------------------------------------

jline() { printf '%s\n' "$1" >> "$JOURNAL"; }
log() { echo "agent[${AGENT_ID:-?}]: $*" >&2; }

journal_op() { # $1 op $2 path $3 sha256|- $4 base $5 t_ms [$6 to $7 to_base]
    case "$1" in
        write) jline "{\"k\":\"op\",\"agent\":\"$AGENT_ID\",\"n\":$N,\"t_ms\":$5,\"op\":\"write\",\"path\":\"$2\",\"sha256\":\"$3\",\"base\":\"$4\",\"nonce\":\"$NONCE\"}" ;;
        delete) jline "{\"k\":\"op\",\"agent\":\"$AGENT_ID\",\"n\":$N,\"t_ms\":$5,\"op\":\"delete\",\"path\":\"$2\",\"base\":\"$4\",\"nonce\":\"$NONCE\"}" ;;
        mv) jline "{\"k\":\"op\",\"agent\":\"$AGENT_ID\",\"n\":$N,\"t_ms\":$5,\"op\":\"mv\",\"path\":\"$2\",\"to\":\"$6\",\"base\":\"$4\",\"to_base\":\"$7\",\"sha256\":\"$3\",\"nonce\":\"$NONCE\"}" ;;
    esac
}
journal_skip() { # $1 path $2 reason: an op the base read could not pin
    now_ms
    jline "{\"k\":\"skip\",\"agent\":\"$AGENT_ID\",\"t_ms\":$NOW,\"path\":\"$1\",\"reason\":\"$2\",\"nonce\":\"$NONCE\"}"
}
journal_mark() { # $1 kind (start|paused|resumed|stopped|terminated)
    now_ms
    jline "{\"k\":\"$1\",\"agent\":\"$AGENT_ID\",\"t_ms\":$NOW,\"batch\":$BATCH,\"n\":$N}"
}

# ---------- the op set -------------------------------------------------------

DIR=""; NP=0; MIX_W=0; MIX_D=0; MIX_M=0; MIX_S=0
setup_mode() {
    case "$AGENT_MODE" in
        disjoint) DIR="$AGENT_ID"; NP=50; set -- 80 10 10 0 ;;
        hot) DIR=hot; NP=30; set -- 100 0 0 0 ;;
        churn) DIR=churn; NP=50; set -- 40 30 20 10 ;;
        vocab) DIR=vocab; NP=30; set -- 70 30 0 0 ;;
        *) echo "agent: AGENT_MODE must be disjoint|hot|churn|vocab" >&2; exit 2 ;;
    esac
    [ -n "${AGENT_MIX:-}" ] && set -- $AGENT_MIX
    MIX_W=$1; MIX_D=$2; MIX_M=$3; MIX_S=$4
    NP=${AGENT_PATHS:-$NP}
    if [ "$(($1 + $2 + $3 + $4))" -ne 100 ]; then
        echo "agent: AGENT_MIX must sum to 100" >&2; exit 2
    fi
}
REL=""
path_of() { REL=$(printf '%s/p%02d.txt' "$DIR" "$1"); }
USED=" "
PICK=-1
pick_unused() { # a path index not yet touched in this batch, or -1
    rand_below "$NP"; PICK=$R; _i=0
    while [ "$_i" -lt "$NP" ]; do
        case "$USED" in *" $PICK "*) ;; *) USED="$USED$PICK "; return 0 ;; esac
        PICK=$(( (PICK + 1) % NP )); _i=$((_i + 1))
    done
    PICK=-1
}

VOCAB_K=0; PAD=0
write_content() { # to stdout; the draws were made by the caller
    if [ "$AGENT_MODE" = vocab ]; then
        printf 'writers-live vocab body %d\n' "$VOCAB_K"
        _i=0; while [ "$_i" -le "$VOCAB_K" ]; do printf 'line %d of body %d\n' "$_i" "$VOCAB_K"; _i=$((_i + 1)); done
    else
        printf 'writers-live agent=%s n=%s mode=%s seed=%s\n' "$AGENT_ID" "$N" "$AGENT_MODE" "$AGENT_SEED"
        _i=0; while [ "$_i" -lt "$PAD" ]; do printf 'pad %s %s %d\n' "$AGENT_ID" "$N" "$_i"; _i=$((_i + 1)); done
    fi
}

valid_hash() { [ "${#1}" -eq 64 ]; }

# A write: the content goes to a sibling temp the scan skips, the base is
# read race-checked (inode before the hash == inode just before the
# rename), and an absent path is created with `ln` so a file that
# appeared meanwhile is never silently replaced.
op_write() {
    N=$((N + 1))
    _abs="$TREE/$1"; _d=${_abs%/*}; _b=${_abs##*/}
    if ! mkdir -p "$_d"; then journal_skip "$1" mkdir; return 0; fi
    _tmp="$_d/.$_b.$AGENT_ID-$N$TMP_SUFFIX"
    if [ "$AGENT_MODE" = vocab ]; then rand_below 8; VOCAB_K=$R; else rand_below 16; PAD=$R; fi
    write_content > "$_tmp"
    sha_file "$_tmp"; _new=$H
    _tries=0
    while [ "$_tries" -lt "$RACE_TRIES" ]; do
        _tries=$((_tries + 1))
        ino_of "$_abs"; _i1=$INO
        if [ -z "$_i1" ]; then
            now_ms; _t=$NOW
            if ln "$_tmp" "$_abs" 2>/dev/null; then
                rm -f "$_tmp"; journal_op write "$1" "$_new" absent "$_t"; return 0
            fi
            if [ ! -e "$_abs" ] && mv -f "$_tmp" "$_abs"; then   # no hard links here
                journal_op write "$1" "$_new" absent "$_t"; return 0
            fi
            continue
        fi
        sha_file "$_abs"; _base=$H
        valid_hash "$_base" || continue
        now_ms; _t=$NOW
        ino_of "$_abs"; [ "$INO" = "$_i1" ] || continue
        if mv -f "$_tmp" "$_abs"; then journal_op write "$1" "$_new" "$_base" "$_t"; return 0; fi
    done
    rm -f "$_tmp"; journal_skip "$1" base-unstable
}

op_delete() {
    _abs="$TREE/$1"; _tries=0
    while [ "$_tries" -lt "$RACE_TRIES" ]; do
        _tries=$((_tries + 1))
        ino_of "$_abs"; _i1=$INO
        if [ -z "$_i1" ]; then op_write "$1"; return 0; fi   # nothing to delete
        sha_file "$_abs"; _base=$H
        valid_hash "$_base" || continue
        now_ms; _t=$NOW
        ino_of "$_abs"; [ "$INO" = "$_i1" ] || continue
        if rm -f "$_abs"; then N=$((N + 1)); journal_op delete "$1" - "$_base" "$_t"; return 0; fi
    done
    journal_skip "$1" base-unstable
}

op_mv() {      # $1 source, $2 destination (both marked used by the caller)
    _abs="$TREE/$1"; _dst="$TREE/$2"; _tries=0
    mkdir -p "${_dst%/*}" || { journal_skip "$2" mkdir; return 0; }
    while [ "$_tries" -lt "$RACE_TRIES" ]; do
        _tries=$((_tries + 1))
        ino_of "$_abs"; _i1=$INO
        if [ -z "$_i1" ]; then op_write "$1"; return 0; fi   # nothing to move
        sha_file "$_abs"; _base=$H
        valid_hash "$_base" || continue
        ino_of "$_dst"; _j1=$INO
        if [ -n "$_j1" ]; then
            sha_file "$_dst"; _tbase=$H
            valid_hash "$_tbase" || continue
        else
            _tbase=absent
        fi
        now_ms; _t=$NOW
        ino_of "$_abs"; [ "$INO" = "$_i1" ] || continue
        ino_of "$_dst"; [ "$INO" = "$_j1" ] || continue
        if mv -f "$_abs" "$_dst"; then
            N=$((N + 1)); journal_op mv "$1" "$_base" "$_base" "$_t" "$2" "$_tbase"; return 0
        fi
    done
    journal_skip "$1" base-unstable
}

op_same() {    # rewrite the path's current bytes (a new inode, same content)
    _abs="$TREE/$1"; _d=${_abs%/*}; _b=${_abs##*/}; _tries=0
    while [ "$_tries" -lt "$RACE_TRIES" ]; do
        _tries=$((_tries + 1))
        ino_of "$_abs"; _i1=$INO
        if [ -z "$_i1" ]; then op_write "$1"; return 0; fi
        _tmp="$_d/.$_b.$AGENT_ID-same$TMP_SUFFIX"
        cat "$_abs" > "$_tmp" 2>/dev/null || { rm -f "$_tmp"; continue; }
        sha_file "$_tmp"; _new=$H
        valid_hash "$_new" || { rm -f "$_tmp"; continue; }
        now_ms; _t=$NOW
        ino_of "$_abs"; [ "$INO" = "$_i1" ] || { rm -f "$_tmp"; continue; }
        if mv -f "$_tmp" "$_abs"; then N=$((N + 1)); journal_op write "$1" "$_new" "$_new" "$_t"; return 0; fi
    done
    rm -f "${_tmp:-}" 2>/dev/null; journal_skip "$1" base-unstable
}

# ---------- publish -----------------------------------------------------------

publish_and_wait() {
    _ctl="$TREE/.flint"; _ptmp="$_ctl/.publish.$AGENT_ID.tmp"
    now_ms; _pub=$NOW
    if ! { printf '{"nonce":"%s"}' "$NONCE" > "$_ptmp" && mv -f "$_ptmp" "$_ctl/publish"; }; then
        jline "{\"k\":\"ack\",\"agent\":\"$AGENT_ID\",\"t_ms\":$_pub,\"nonce\":\"$NONCE\",\"status\":\"publish-failed\"}"
        return 0
    fi
    _deadline=$((_pub + ACK_TIMEOUT_SECS * 1000))
    while :; do
        if [ -f "$_ctl/publish.ack" ] && grep -q "\"$NONCE\"" "$_ctl/publish.ack" 2>/dev/null; then
            parse_ack "$_ctl/publish.ack"
            if [ -n "$ACK_STATUS" ] && ack_covers "$NONCE"; then
                now_ms
                jline "{\"k\":\"ack\",\"agent\":\"$AGENT_ID\",\"t_ms\":$NOW,\"nonce\":\"$NONCE\",\"status\":\"$ACK_STATUS\",\"seq\":$ACK_SEQ,\"dropped\":[$ACK_DROPPED],\"covered\":[$ACK_NONCES],\"report\":{\"uploaded\":$ACK_UPLOADED,\"deleted\":$ACK_DELETED,\"parked\":$ACK_PARKED,\"consumed\":$ACK_CONSUMED,\"no_change\":$ACK_NO_CHANGE}}"
                return 0
            fi
        fi
        now_ms
        if [ "$NOW" -ge "$_deadline" ]; then
            jline "{\"k\":\"ack\",\"agent\":\"$AGENT_ID\",\"t_ms\":$NOW,\"nonce\":\"$NONCE\",\"status\":\"no-ack\"}"
            return 0
        fi
        sleep_ms "$ACK_POLL_MS"
    done
}

run_batch() {
    BATCH=$((BATCH + 1)); NONCE="$AGENT_ID-$BATCH"; USED=" "
    _k=0
    while [ "$_k" -lt "$OPS_PER_BATCH" ]; do
        _k=$((_k + 1))
        rand_below 100; _roll=$R
        pick_unused; [ "$PICK" -ge 0 ] || break
        path_of "$PICK"; _src=$REL
        if [ "$_roll" -lt "$MIX_W" ]; then
            op_write "$_src"
        elif [ "$_roll" -lt $((MIX_W + MIX_D)) ]; then
            op_delete "$_src"
        elif [ "$_roll" -lt $((MIX_W + MIX_D + MIX_M)) ]; then
            pick_unused
            if [ "$PICK" -lt 0 ]; then op_write "$_src"; else path_of "$PICK"; op_mv "$_src" "$REL"; fi
        else
            op_same "$_src"
        fi
    done
    publish_and_wait
}

# ---------- control -------------------------------------------------------------

TERMINATED=0
final_publish_and_exit() {
    BATCH=$((BATCH + 1)); NONCE="$AGENT_ID-$BATCH"
    publish_and_wait
    journal_mark stopped
    exit 0
}
idle_sleep() { # $1 ms, cut short by stop, pause or SIGTERM
    _left=$1
    while [ "$_left" -gt 0 ]; do
        [ "$TERMINATED" = 1 ] && return 0
        [ -e "$CONTROL_DIR/stop" ] && return 0
        [ -e "$CONTROL_DIR/pause" ] && return 0
        if [ "$_left" -gt 250 ]; then sleep_ms 250; _left=$((_left - 250)); else sleep_ms "$_left"; _left=0; fi
    done
}

run_agent() {
    : "${AGENT_ID:?AGENT_ID is required}"
    : "${TREE:?TREE is required}"
    case "$AGENT_ID" in *[!A-Za-z0-9_-]*|'') echo "agent: AGENT_ID must be [A-Za-z0-9_-]" >&2; exit 2 ;; esac
    AGENT_MODE=${AGENT_MODE:-disjoint}
    AGENT_SEED=${AGENT_SEED:-1}
    case "$AGENT_SEED" in *[!0-9]*|'') echo "agent: AGENT_SEED must be a non-negative integer" >&2; exit 2 ;; esac
    JOURNAL=${JOURNAL:-/agent/journal.jsonl}
    CONTROL_DIR=${CONTROL_DIR:-/agent}
    OPS_PER_BATCH=${OPS_PER_BATCH:-5}
    BATCH_SLEEP_MIN_MS=${BATCH_SLEEP_MIN_MS:-3000}
    BATCH_SLEEP_MAX_MS=${BATCH_SLEEP_MAX_MS:-10000}
    FLOOR_SECS=${FLOOR_SECS:-5}
    ACK_TIMEOUT_SECS=${ACK_TIMEOUT_SECS:-$((3 * FLOOR_SECS))}
    ACK_POLL_MS=${ACK_POLL_MS:-200}
    READY_TIMEOUT_SECS=${READY_TIMEOUT_SECS:-600}
    MAX_BATCHES=${MAX_BATCHES:-0}
    RACE_TRIES=${RACE_TRIES:-5}
    setup_mode
    [ "$OPS_PER_BATCH" -le "$NP" ] || OPS_PER_BATCH=$NP
    detect_time; detect_sha
    mkdir -p "${JOURNAL%/*}" "$CONTROL_DIR"

    _waited=0
    while [ ! -d "$TREE/.flint" ]; do
        if [ "$_waited" -ge "$READY_TIMEOUT_SECS" ]; then
            log "no $TREE/.flint after ${READY_TIMEOUT_SECS}s"; exit 2
        fi
        sleep 1; _waited=$((_waited + 1))
    done

    # Resume after a container restart: never reuse an n or a nonce.
    N=0; BATCH=0
    if [ -s "$JOURNAL" ]; then
        _v=$(sed -n 's/^{"k":"op",.*"n":\([0-9][0-9]*\),.*/\1/p' "$JOURNAL" | tail -n 1)
        [ -n "$_v" ] && N=$_v
        _v=$(sed -n 's/.*"nonce":"'"$AGENT_ID"'-\([0-9][0-9]*\)".*/\1/p' "$JOURNAL" | sort -n | tail -n 1)
        [ -n "$_v" ] && BATCH=$_v
    fi
    NONCE=""
    seed_lcg "$AGENT_SEED" "$BATCH"
    journal_mark start
    log "mode=$AGENT_MODE seed=$AGENT_SEED batch=$BATCH n=$N tree=$TREE"
    trap 'TERMINATED=1' TERM INT

    _batches=0
    while :; do
        if [ "$TERMINATED" = 1 ]; then journal_mark terminated; exit 143; fi
        [ -e "$CONTROL_DIR/stop" ] && final_publish_and_exit
        if [ -e "$CONTROL_DIR/pause" ]; then
            journal_mark paused
            while [ -e "$CONTROL_DIR/pause" ] && [ ! -e "$CONTROL_DIR/stop" ] && [ "$TERMINATED" = 0 ]; do sleep 1; done
            journal_mark resumed
            continue
        fi
        run_batch
        _batches=$((_batches + 1))
        if [ "$MAX_BATCHES" -gt 0 ] && [ "$_batches" -ge "$MAX_BATCHES" ]; then final_publish_and_exit; fi
        _span=$((BATCH_SLEEP_MAX_MS - BATCH_SLEEP_MIN_MS + 1))
        [ "$_span" -gt 0 ] || _span=1
        rand_below "$_span"
        idle_sleep $((BATCH_SLEEP_MIN_MS + R))
    done
}

# ---------- entry -------------------------------------------------------------

if [ "${AGENT_LIB_ONLY:-0}" != 1 ]; then
    case "${1:-}" in
        parse-ack)
            parse_ack "$2"
            printf 'status=%s\nseq=%s\nnonces=[%s]\ndropped=[%s]\nuploaded=%s\ndeleted=%s\nparked=%s\nconsumed=%s\nno_change=%s\n' \
                "$ACK_STATUS" "$ACK_SEQ" "$ACK_NONCES" "$ACK_DROPPED" "$ACK_UPLOADED" "$ACK_DELETED" "$ACK_PARKED" "$ACK_CONSUMED" "$ACK_NO_CHANGE"
            ;;
        rand)
            seed_lcg "$2" 0; _c=0
            while [ "$_c" -lt "$3" ]; do rand_below 100; echo "$R"; _c=$((_c + 1)); done
            ;;
        '') run_agent ;;
        *) echo "usage: agent.sh [parse-ack FILE | rand SEED COUNT]" >&2; exit 2 ;;
    esac
fi
