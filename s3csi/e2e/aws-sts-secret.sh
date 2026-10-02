#!/usr/bin/env bash
# identity.mode stsSecret against a REAL store with REAL sessions — the
# half the kind rig cannot reach (docs/plans/passthrough-sts-secret-mode.md
# §7). Runs after `run-s3csi.sh setup` with STORE=s3 (a real bucket; the
# drill IAM user's key in S3_KEY_FILE), on kind or on real nodes.
#
#   R1  two pods under one CR, one dimension apart. Both start on the same
#       15-minute STS session. One pod's Secret is rotated — a new session
#       and a higher generation every ~8 minutes, four sessions in all —
#       and must read every 5 s for ~32 minutes across THREE expiries with
#       zero errors. The other pod's Secret is left alone and must FAIL
#       when its session expires: the known-bad, without which zero errors
#       would be vacuous (the store might not enforce expiry; the mounter
#       might never have needed the new material). The worker's door log,
#       with timestamps, shows WHEN the mounter fetched each generation —
#       which must be before its predecessor expired.
#
# Env: KUBECONFIG CTX BUCKET S3_REGION S3_KEY_FILE as run-s3csi.sh with
# STORE=s3; AWSCLI overrides the minting command (default: the aws-cli
# image under docker, so the box needs no AWS CLI of its own).
set -u
cd "$(dirname "$0")"
export STORE=s3
REPO=$(cd ../.. && pwd)
# The drill's knobs and helpers, verbatim, up to its setup block (this
# also reads S3_KEY_FILE into AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY).
eval "$(sed -n '/^CTX=\${CTX:-/,/^# ── setup \/ teardown/p' run-s3csi.sh | sed '$d')"
export AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
AWSCLI=${AWSCLI:-docker run --rm -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_DEFAULT_REGION amazon/aws-cli:latest}
now() { date +%s; }
# A real session: {AccessKeyId, SecretAccessKey, SessionToken, Expiration},
# 900 s (STS's minimum). Empty on failure — the caller must check.
mint() { AWS_DEFAULT_REGION=$S3_REGION $AWSCLI sts get-session-token --duration-seconds 900 --output json 2>/dev/null | jq -c '.Credentials // empty' 2>/dev/null; }
# The stsSecret Secret from a minted session; prints its Expiration.
sts_secret_real() { # name gen creds-json [k=v ...]
    local name=$1 gen=$2 c=$3 kv extra=""; shift 3
    for kv in "$@"; do extra="$extra --from-literal=$kv"; done
    # shellcheck disable=SC2086
    $K -n $NS create secret generic "$name" --dry-run=client -o yaml \
        --from-literal=AWS_ACCESS_KEY_ID="$(jq -r .AccessKeyId <<<"$c")" \
        --from-literal=AWS_SECRET_ACCESS_KEY="$(jq -r .SecretAccessKey <<<"$c")" \
        --from-literal=AWS_SESSION_TOKEN="$(jq -r .SessionToken <<<"$c")" \
        --from-literal=AWS_CREDENTIAL_EXPIRATION="$(jq -r .Expiration <<<"$c")" \
        --from-literal=generation="$gen" $extra | $K apply -f - >/dev/null
    jq -r .Expiration <<<"$c"
}
# When the door FIRST served a given expiration, as epoch seconds (the
# worker logs one line per fetch; kubectl stamps it). Empty if never.
door_first_served() { # worker expiration
    $K -n $WNS logs --timestamps "$1" 2>/dev/null | grep -F "door served creds.json Expiration=$2" | head -1 | awk '{print $1}' | sed 's/\.[0-9]*Z$/Z/' | { read -r t; [ -n "$t" ] && iso_to_epoch "$t"; }
}
door_count() { $K -n $WNS logs "$1" 2>/dev/null | grep -c "door served creds.json Expiration=$2" || true; }
# The in-pod reader: every 5 s until END, remembering the first failure
# and the first recovery after it (epoch seconds), and the totals.
reader_loop() { # end-epoch
    printf 'e=0; n=0; ff=""; fr=""; st=ok; while [ $(date +%%s) -lt %s ]; do n=$((n+1)); if [ "$(cat /mnt/s3/shard-07.txt 2>/dev/null)" = seeded-object-07 ]; then [ $st = fail ] && { fr="$fr $(date +%%s)"; st=ok; }; else e=$((e+1)); [ $st = ok ] && { ff="$ff $(date +%%s)"; st=fail; }; fi; sleep 5; done; echo "errors=$e reads=$n first_fail=$ff recover=$fr"' "$1"
}
echo "stsSecret against a real store — cluster $CTX, node $NODE, bucket $BUCKET ($S3_REGION)"
$K get csidriver s3.csi.chert.us >/dev/null 2>&1 || { echo "no s3.csi.chert.us — run run-s3csi.sh setup first (STORE=s3)"; exit 2; }

# ── R1 real sessions: rotated reads across three expiries; unrotated fails at one ──
leg R1 "real STS sessions: a rotated pod reads every 5 s for ~32 min across three expiries with zero errors; an unrotated pod FAILS when its session expires; the mounter fetched each generation before its predecessor expired"
c1=$(mint)
if [ -z "$c1" ]; then
    bad "STS get-session-token returned nothing — no session, no leg"
else
    ok "PRECONDITION: a real 900 s session minted (AccessKeyId $(jq -r .AccessKeyId <<<"$c1" | cut -c1-4)…, expires $(jq -r .Expiration <<<"$c1"))"
    exp1=$(sts_secret_real sts-session 1 "$c1")
    sts_secret_real sts-session-bad 1 "$c1" >/dev/null
    apply_fx sts-reader.yaml >/dev/null
    sed -e 's/name: reader-sts$/name: reader-sts-bad/' -e 's/name: sts-session$/name: sts-session-bad/' sts-reader.yaml | fx /dev/stdin | $K apply -f - >/dev/null
    if wait_phase reader-sts Running 240 && wait_phase reader-sts-bad Running 240; then
        for p in reader-sts reader-sts-bad; do
            got=$(inpod "$p" cat /mnt/s3/shard-07.txt)
            [ "$got" = "seeded-object-07" ] && ok "$p reads real-bucket content through the session (token and all)" || bad "$p read: '$got'"
        done
        wg=$(worker_of reader-sts); wb=$(worker_of reader-sts-bad)
        t0=$(now); END=$((t0 + 1900))
        good_out=$(mktemp); bad_out=$(mktemp); keys=$(mktemp)
        ( $K -n $NS exec reader-sts -c agent -- /bin/sh -c "$(reader_loop $END)" > "$good_out" 2>&1 ) & gpid=$!
        ( $K -n $NS exec reader-sts-bad -c agent -- /bin/sh -c "$(reader_loop $END)" > "$bad_out" 2>&1 ) & bpid=$!
        ( while [ "$(now)" -lt $END ]; do $K -n $WNS exec "$wg" -- cat /comm/creds.json 2>/dev/null | jq -r '.AccessKeyId // empty' 2>/dev/null; sleep 30; done >> "$keys" 2>/dev/null ) & kpid=$!
        ok "readers started on both pods at t0 (session 1 expires $exp1); the rotated pod's door is $wg, the unrotated pod's $wb"
        # The rotation schedule, seconds after t0: each new session arrives
        # 400+ s before the one the mounter should be holding expires.
        exps=("$exp1"); gens=(1); installs=(0)
        for spec in "420 2" "900 3" "1380 4"; do
            at=${spec% *}; g=${spec#* }
            while [ "$(now)" -lt $((t0 + at)) ]; do sleep 10; done
            c=$(mint)
            if [ -z "$c" ]; then bad "minting session $g failed at t0+${at}s"; continue; fi
            e=$(sts_secret_real sts-session "$g" "$c")
            t=$(wait_creds_exp "$wg" "$e" 240) && ok "generation $g (expires $e) reached the rotated pod's door ${t}s after it was written, at t0+$(( $(now) - t0 ))s" || bad "generation $g never reached the door within ${t}s (door serves '$(creds_exp "$wg")')"
            exps+=("$e"); gens+=("$g"); installs+=($(now))
        done
        wait "$gpid" 2>/dev/null; wait "$bpid" 2>/dev/null; kill "$kpid" 2>/dev/null; wait "$kpid" 2>/dev/null
        gres=$(cat "$good_out"); bres=$(cat "$bad_out")
        note "rotated:   $gres"
        note "unrotated: $bres"
        # The rotated pod: zero errors, and a plausible read count.
        case "$gres" in
            errors=0\ reads=*) r=${gres#errors=0 reads=}; r=${r%% *}; [ "$r" -ge 340 ] && ok "the rotated pod read $r times across three session expiries with ZERO errors" || bad "the rotated pod read only $r times (wanted ≥340 over ~1900 s)" ;;
            *) bad "the rotated pod saw errors: '$gres'" ;;
        esac
        # The unrotated pod: the known-bad. It MUST fail, and only after its session expired.
        e1=$(iso_to_epoch "$exp1")
        ff=$(sed -n 's/.*first_fail= *\([0-9]*\).*/\1/p' <<<"$bres")
        if [ -z "$ff" ]; then
            bad "the unrotated pod NEVER failed in $(( END - e1 ))s past its session's expiry — the store did not enforce it, and the rotated pod's zero errors prove nothing"
        elif [ "$ff" -lt $((e1 - 30)) ]; then
            bad "the unrotated pod failed $(( e1 - ff ))s BEFORE its session expired — something other than expiry broke it"
        else
            ok "the unrotated pod failed $(( ff - e1 ))s after its session expired (the store enforces expiry; the mounter had nothing else to fall back on)"
            case "$bres" in errors=0*) bad "unrotated errors=0 yet a first failure was recorded — the loop's own bookkeeping disagrees" ;; esac
        fi
        # The consumer's side: each generation fetched from the door BEFORE its predecessor expired.
        for i in 1 2 3; do
            [ "${gens[$i]:-}" ] || continue
            f=$(door_first_served "$wg" "${exps[$i]}"); pe=$(iso_to_epoch "${exps[$((i-1))]}")
            if [ -z "$f" ]; then
                bad "the mounter never fetched generation ${gens[$i]} (expires ${exps[$i]}) from the door"
            elif [ "$f" -lt "$pe" ]; then
                ok "the mounter fetched generation ${gens[$i]} $(( pe - f ))s before generation ${gens[$((i-1))]} expired ($(( f - installs[i] ))s after it was installed; fetched $(door_count "$wg" "${exps[$i]}") time(s) in all)"
            else
                bad "the mounter fetched generation ${gens[$i]} only $(( f - pe ))s AFTER generation ${gens[$((i-1))]} had expired"
            fi
        done
        nk=$(sort -u "$keys" | grep -c . || true)
        note "$nk distinct AccessKeyIds landed in the rotated pod's creds.json (4 sessions minted); the unrotated door served its one expiration $(door_count "$wb" "$exp1") time(s)"
        n=$(mount_events reader-sts | grep -c '^CredentialReplaced' || true)
        [ "${n:-0}" -ge 3 ] && ok "$n CredentialReplaced events on the rotated pod" || bad "only ${n:-0} CredentialReplaced events on the rotated pod (wanted 3)"
        rm -f "$good_out" "$bad_out" "$keys"
    else
        bad "the readers never reached Running: $(mount_events reader-sts | tail -1 | cut -c1-200) / $(mount_events reader-sts-bad | tail -1 | cut -c1-200)"
    fi
fi
$K -n $NS delete pod reader-sts reader-sts-bad --ignore-not-found --wait=true --timeout=240s >/dev/null 2>&1
$K -n $NS delete secret sts-session sts-session-bad --ignore-not-found >/dev/null 2>&1
