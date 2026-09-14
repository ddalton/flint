#!/bin/sh
# writers-live UI actor (leg A6): a human-in-the-loop editor writing the
# shared paths through the lean gateway while the agents storm. It
# journals like agent.sh with agent "ui" (README.md §1): an `op` line
# BEFORE each gateway PUT, then an `ack` line with the gateway's etag.
#
# Each write is read-modify-write, as a browser does it: GET the file
# (its bytes are the journaled `base`, its etag the If-Match), then PUT
# with that precondition — the gateway refuses an overwrite that names
# no version (428), so a blind PUT is not an option. A refused attempt
# (412 file-changed, 409 window/concurrent-write) is journaled with
# status "refused" and retried from a fresh GET, up to UI_RETRIES.
#
# Needs agent.sh next to it (or AGENT_SH) for the LCG, clock and hashing,
# and curl. Environment: GATEWAY (e.g. http://127.0.0.1:8080), WORKSPACE,
# GATEWAY_TOKEN or GATEWAY_TOKEN_FILE, UI_MODE (hot|churn), UI_SEED (1),
# JOURNAL (/ui/journal.jsonl), CONTROL_DIR (/ui; pause/stop as agent.sh),
# UI_INTERVAL_MS (2000), UI_RETRIES (4), UI_MAX_WRITES (0 = until stopped),
# UI_PATHS (paths in the mode's set).

set -u
set -f

_here=$(cd "$(dirname "$0")" && pwd)
AGENT_LIB_ONLY=1
. "${AGENT_SH:-$_here/agent.sh}"

: "${GATEWAY:?GATEWAY is required}"
: "${WORKSPACE:?WORKSPACE is required}"
AGENT_ID=ui
UI_MODE=${UI_MODE:-hot}
UI_SEED=${UI_SEED:-1}
JOURNAL=${JOURNAL:-/ui/journal.jsonl}
CONTROL_DIR=${CONTROL_DIR:-/ui}
UI_INTERVAL_MS=${UI_INTERVAL_MS:-2000}
UI_RETRIES=${UI_RETRIES:-4}
UI_MAX_WRITES=${UI_MAX_WRITES:-0}
if [ -z "${GATEWAY_TOKEN:-}" ] && [ -n "${GATEWAY_TOKEN_FILE:-}" ]; then
    GATEWAY_TOKEN=$(cat "$GATEWAY_TOKEN_FILE")
fi
GATEWAY_TOKEN=${GATEWAY_TOKEN:-}
case "$UI_MODE" in
    hot) DIR=hot; NP=${UI_PATHS:-30} ;;
    churn) DIR=churn; NP=${UI_PATHS:-50} ;;
    *) echo "ui: UI_MODE must be hot|churn" >&2; exit 2 ;;
esac
case "$UI_SEED" in *[!0-9]*|'') echo "ui: UI_SEED must be a non-negative integer" >&2; exit 2 ;; esac

WORK=$(mktemp -d "${TMPDIR:-/tmp}/writers-ui.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
trap 'TERMINATED=1' TERM INT

GW_CODE=""; GW_ETAG=""; GW_ERROR=""; GW_RETRY=""

# GET the file's current bytes into $2. Sets GW_CODE (200, 404, …) and
# GW_ETAG (the `etag` response header, verbatim — quotes included).
gw_read() {
    GW_ETAG=""; GW_ERROR=""
    GW_CODE=$(curl -sS -o "$2" -D "$WORK/get.hdr" -w '%{http_code}' \
        -H "Authorization: Bearer $GATEWAY_TOKEN" \
        "$GATEWAY/lean/v1/$WORKSPACE/files/$1" 2>"$WORK/get.err") || GW_CODE=000
    GW_ETAG=$(sed -n 's/^[Ee][Tt][Aa][Gg]:[[:space:]]*//p' "$WORK/get.hdr" 2>/dev/null | tr -d '\r' | tail -n 1)
}

# ---- ASSUMPTION — the cluster engineer confirms this against a live gateway
# The write: PUT /lean/v1/<workspace>/files/<path> with the body, the bearer,
# and the precondition header in $3 ("If-Match: <etag>" or "If-None-Match: *").
# SUCCESS is HTTP 2xx with a JSON body carrying the new entity-tag as `etag`,
# i.e. {"etag":"\"<md5>\""} — read from lean/gateway/src/http.rs at HEAD
# (handle_files_put -> ok_json(EtagResp{etag})), never observed on the wire.
# A refusal is {"error":"<code>","message":…} with 4xx (409 adds Retry-After).
# Sets GW_CODE, GW_ETAG (the JSON string's contents, still JSON-escaped, so
# it drops into the journal line as is), GW_ERROR, GW_RETRY (seconds).
gw_write() {   # $1 path, $2 body file, $3 precondition header
    GW_ETAG=""; GW_ERROR=""; GW_RETRY=""
    GW_CODE=$(curl -sS -o "$WORK/put.body" -D "$WORK/put.hdr" -w '%{http_code}' -X PUT \
        -H "Authorization: Bearer $GATEWAY_TOKEN" -H "x-flint-author: ui" -H "$3" \
        --data-binary @"$2" "$GATEWAY/lean/v1/$WORKSPACE/files/$1" 2>"$WORK/put.err") || GW_CODE=000
    _body=$(tr -d '\n\r' < "$WORK/put.body" 2>/dev/null) || _body=""
    case "$GW_CODE" in
        2??) GW_ETAG=$(printf '%s' "$_body" | sed -n 's/.*"etag"[[:space:]]*:[[:space:]]*"\(.*\)"[[:space:]]*}.*/\1/p') ;;
        *) GW_ERROR=$(printf '%s' "$_body" | sed -n 's/.*"error"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p') ;;
    esac
    GW_RETRY=$(sed -n 's/^[Rr]etry-[Aa]fter:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$WORK/put.hdr" 2>/dev/null | tail -n 1)
}
# ---- end of the assumption ------------------------------------------------

ui_write_once() { # $1 path; returns 0 when done (acked or given up), 1 to retry
    gw_read "$1" "$WORK/cur"
    case "$GW_CODE" in
        200) sha_file "$WORK/cur"; _base=$H; _cond="If-Match: $GW_ETAG"
             valid_hash "$_base" && [ -n "$GW_ETAG" ] || { log "GET $1: no hash/etag"; return 1; } ;;
        404) _base=absent; _cond="If-None-Match: *" ;;
        *) log "GET $1 -> $GW_CODE $(cat "$WORK/get.err" 2>/dev/null)"; return 1 ;;
    esac
    N=$((N + 1)); NONCE="ui-$N"
    rand_below 16; _pad=$R
    {
        printf 'writers-live ui n=%s mode=%s seed=%s\n' "$N" "$UI_MODE" "$UI_SEED"
        _i=0; while [ "$_i" -lt "$_pad" ]; do printf 'pad ui %s %d\n' "$N" "$_i"; _i=$((_i + 1)); done
    } > "$WORK/new"
    sha_file "$WORK/new"; _new=$H
    _betag=$(printf '%s' "$GW_ETAG" | sed 's/\\/\\\\/g; s/"/\\"/g')
    now_ms
    jline "{\"k\":\"op\",\"agent\":\"ui\",\"n\":$N,\"t_ms\":$NOW,\"op\":\"write\",\"path\":\"$1\",\"sha256\":\"$_new\",\"base\":\"$_base\",\"base_etag\":\"$_betag\",\"nonce\":\"$NONCE\"}"
    gw_write "$1" "$WORK/new" "$_cond"
    now_ms
    case "$GW_CODE" in
        2??)
            if [ -n "$GW_ETAG" ]; then
                jline "{\"k\":\"ack\",\"agent\":\"ui\",\"t_ms\":$NOW,\"nonce\":\"$NONCE\",\"status\":\"ok\",\"etag\":\"$GW_ETAG\",\"http\":$GW_CODE}"
            else
                jline "{\"k\":\"ack\",\"agent\":\"ui\",\"t_ms\":$NOW,\"nonce\":\"$NONCE\",\"status\":\"error\",\"http\":$GW_CODE,\"error\":\"unparsed-2xx\"}"
            fi
            return 0 ;;
        409|412)
            jline "{\"k\":\"ack\",\"agent\":\"ui\",\"t_ms\":$NOW,\"nonce\":\"$NONCE\",\"status\":\"refused\",\"http\":$GW_CODE,\"error\":\"$GW_ERROR\"}"
            _wait=${GW_RETRY:-0}; [ "$_wait" -le 5 ] || _wait=5
            sleep_ms $((_wait * 1000 + 300))
            return 1 ;;
        *)
            case "$GW_CODE" in ''|*[!0-9]*|000) _hc=0 ;; *) _hc=$GW_CODE ;; esac
            jline "{\"k\":\"ack\",\"agent\":\"ui\",\"t_ms\":$NOW,\"nonce\":\"$NONCE\",\"status\":\"error\",\"http\":$_hc,\"error\":\"$GW_ERROR\"}"
            return 0 ;;
    esac
}

detect_time; detect_sha
mkdir -p "${JOURNAL%/*}" "$CONTROL_DIR"
N=0; BATCH=0; NONCE=""; TERMINATED=0
if [ -s "$JOURNAL" ]; then
    _v=$(sed -n 's/.*"nonce":"ui-\([0-9][0-9]*\)".*/\1/p' "$JOURNAL" | sort -n | tail -n 1)
    [ -n "$_v" ] && N=$_v
fi
seed_lcg "$UI_SEED" "$N"
journal_mark start
log "mode=$UI_MODE seed=$UI_SEED n=$N gateway=$GATEWAY workspace=$WORKSPACE"
_writes=0
while :; do
    if [ "$TERMINATED" = 1 ]; then journal_mark terminated; exit 143; fi
    if [ -e "$CONTROL_DIR/stop" ]; then journal_mark stopped; exit 0; fi
    if [ -e "$CONTROL_DIR/pause" ]; then
        journal_mark paused
        while [ -e "$CONTROL_DIR/pause" ] && [ ! -e "$CONTROL_DIR/stop" ] && [ "$TERMINATED" = 0 ]; do sleep 1; done
        journal_mark resumed
        continue
    fi
    rand_below "$NP"; path_of "$R"
    _attempt=0
    while [ "$_attempt" -lt "$UI_RETRIES" ]; do
        _attempt=$((_attempt + 1))
        ui_write_once "$REL" && break
    done
    _writes=$((_writes + 1))
    if [ "$UI_MAX_WRITES" -gt 0 ] && [ "$_writes" -ge "$UI_MAX_WRITES" ]; then journal_mark stopped; exit 0; fi
    idle_sleep "$UI_INTERVAL_MS"
done
