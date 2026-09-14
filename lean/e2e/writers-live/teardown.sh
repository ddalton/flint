#!/usr/bin/env bash
# teardown.sh — the live multi-writer drill's teardown (plan §8). Run on the MAC,
# by a person, one step at a time. NEVER from a driver, a loop, or a trap:
# there is no default subcommand, and every destructive step prints what it
# would delete and does nothing without --yes.
#
#   BUCKET=flint-lean-writers-<date> teardown.sh pull <local-dir> [--allow-empty]
#   BUCKET=... [LOGBUCKET=flint-lean-writers-<date>-logs] teardown.sh bucket [--yes]
#   POLICY=<inline policy name> teardown.sh policy [--yes]
#   teardown.sh project <trove project name> [--yes]
#   [POLICY=...] teardown.sh zeroset <cluster-name> [--ignore-foreign-instances]
#
# Order (plan §8): pull -> bucket -> policy -> project (cluster, then the Ozone
# instance's project) -> zeroset.
#
# pull     `aws s3 sync` _rig/collect, _rig/evidence, _rig/history (and
#          s3://$LOGBUCKET/ into access-logs/ when LOGBUCKET is set — S3 access
#          logs arrive hours late: say so if you do not wait) into <local-dir>;
#          prints each prefix's bucket object count and bytes beside the local
#          count; then checks EVERY SHA256SUMS found (a changed or missing file
#          fails) and lists files no SHA256SUMS covers. Fails on an empty
#          evidence prefix unless --allow-empty; a listing that errors is a
#          failure, never "0 objects".
# bucket   `aws s3 rb --force` of $BUCKET and $LOGBUCKET. Both names must start
#          `flint-lean-writers-`. Prints object counts, versioning and region
#          first; refuses a versioned bucket (rb --force leaves versions).
# policy   `aws iam delete-role-policy --role-name TroveSSMInstanceProfile
#          --policy-name $POLICY`; prints the policy document first. Already
#          absent (NoSuchEntity) is success.
# project  trove: GET /api/v1/projects, the one project with that exact name;
#          POST /api/v1/projects/delete {"projectId":N}; waits for the row to go
#          (trove deletes in the background; status "Failure" fails); a second
#          POST must answer 404. A trove that does not answer is a failure, not
#          a 404.
# zeroset  with $ADMIN_PROFILE (trove-admin) in us-west-1, after printing
#          `aws sts get-caller-identity`: non-terminated instances of the cluster
#          (tag trove:cluster, or the name in any tag) AND in the whole region;
#          open/active spot requests; available volumes; security groups
#          trove-<cluster>-*; buckets flint-lean-writers-*; the inline policy on
#          TroveSSMInstanceProfile ($POLICY, or any flint-lean-writers* name);
#          CloudTrail trails flint-lean-writers*; trove GET /api/v1/aws/orphans
#          (matched 0, ghostRows, orphans empty). Each prints its count; any
#          nonzero fails. Every query must SUCCEED: AccessDenied is not zero —
#          `aws s3 ls` showing no buckets on the rolesanywhere profile is not a
#          zero set. --ignore-foreign-instances reports region-wide instances
#          that are not the cluster's without failing on them (another session's
#          cluster).
#
# Environment: BUCKET, LOGBUCKET, POLICY, ADMIN_PROFILE (trove-admin, for every
# aws call), REGION (us-west-1), TROVE_API (https://localhost:8080/api/v1),
# PROJECT_WAIT_SECS (1800). Test seams: AWS_BIN, CURL_BIN, POLL_SECS.
set -u

REGION=${REGION:-us-west-1}
ADMIN_PROFILE=${ADMIN_PROFILE:-trove-admin}
TROVE_API=${TROVE_API:-https://localhost:8080/api/v1}
TROVE_TOKEN=trove-dummy-token
ROLE=TroveSSMInstanceProfile
AWS_BIN=${AWS_BIN:-aws}
CURL_BIN=${CURL_BIN:-curl}
PROJECT_WAIT_SECS=${PROJECT_WAIT_SECS:-1800}
POLL_SECS=${POLL_SECS:-15}
SELF="${BASH_SOURCE[0]}"

say() { printf '%s\n' "$*"; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
aws_() { AWS_PROFILE="$ADMIN_PROFILE" "$AWS_BIN" --region "$REGION" "$@"; }

has_flag() { # flag args...
    local f=$1
    shift
    for a in "$@"; do [ "$a" = "$f" ] && return 0; done
    return 1
}

identity() {
    local out
    out=$(aws_ sts get-caller-identity --output json 2>&1) || die "sts get-caller-identity failed with profile $ADMIN_PROFILE: $out"
    say "caller identity (profile $ADMIN_PROFILE): $(printf '%s' "$out" | tr -d '\n' | tr -s ' ')"
}

drill_bucket_name() { # name label
    case "$1" in
        flint-lean-writers-*) ;;
        *) die "$2='$1' does not start with flint-lean-writers- — refusing to touch it" ;;
    esac
    case "$1" in *[!a-z0-9.-]*) die "$2='$1' is not a bucket name" ;; esac
}

s3_count() { # s3-url -> prints "objects bytes"; exit 1 when the listing fails
    local out objects bytes
    out=$(aws_ s3 ls "$1" --recursive --summarize 2>&1) || { printf 'listing %s failed: %s\n' "$1" "$out" >&2; return 1; }
    objects=$(printf '%s\n' "$out" | sed -n 's/^ *Total Objects: *\([0-9][0-9]*\).*/\1/p')
    bytes=$(printf '%s\n' "$out" | sed -n 's/^ *Total Size: *\([0-9][0-9]*\).*/\1/p')
    [ -n "$objects" ] && [ -n "$bytes" ] || { printf 'listing %s has no totals: %s\n' "$1" "$out" >&2; return 1; }
    printf '%s %s\n' "$objects" "$bytes"
}

# ------------------------------------------------------------------- pull --

cmd_pull() {
    local dir=${1:-} rc=0 p counts n_local
    [ -n "$dir" ] || die "usage: BUCKET=... teardown.sh pull <local-dir> [--allow-empty]"
    drill_bucket_name "${BUCKET:-}" BUCKET
    identity
    mkdir -p "$dir"
    for p in collect evidence history; do
        counts=$(s3_count "s3://$BUCKET/_rig/$p/") || { rc=1; continue; }
        say "s3://$BUCKET/_rig/$p/: objects=${counts% *} bytes=${counts#* }"
        if [ "${counts% *}" = 0 ]; then
            if has_flag --allow-empty "$@"; then
                say "  _rig/$p/ is empty (allowed)"
            else
                say "FAIL: _rig/$p/ is EMPTY in the bucket (--allow-empty if that is expected)"
                rc=1
            fi
            continue
        fi
        if ! aws_ s3 sync "s3://$BUCKET/_rig/$p/" "$dir/$p/" --exact-timestamps --no-progress --only-show-errors; then
            say "FAIL: sync of _rig/$p/ failed"
            rc=1
            continue
        fi
        n_local=$(find "$dir/$p" -type f 2>/dev/null | wc -l | tr -d ' ')
        say "  local $dir/$p: files=$n_local"
        [ "$n_local" -ge "${counts% *}" ] || { say "FAIL: fewer local files ($n_local) than bucket objects (${counts% *})"; rc=1; }
    done
    if [ -n "${LOGBUCKET:-}" ]; then
        drill_bucket_name "$LOGBUCKET" LOGBUCKET
        if counts=$(s3_count "s3://$LOGBUCKET/"); then
            say "s3://$LOGBUCKET/ (access logs; delivery lags by hours): objects=${counts% *} bytes=${counts#* }"
            aws_ s3 sync "s3://$LOGBUCKET/" "$dir/access-logs/" --exact-timestamps --no-progress --only-show-errors || { say "FAIL: access-log sync"; rc=1; }
        else
            rc=1
        fi
    fi
    verify_sums "$dir" || rc=1
    if [ $rc = 0 ]; then say "pull: OK"; else say "pull: FAILED"; fi
    return $rc
}

verify_sums() { # dir — every SHA256SUMS must verify; report uncovered files
    local dir=$1 rc=0 n=0 sums d
    while IFS= read -r sums; do
        [ -n "$sums" ] || continue
        d=$(dirname "$sums")
        n=$((n + 1))
        if (cd "$d" && shasum -a 256 -c --quiet SHA256SUMS); then
            say "  sha256 OK: $sums ($(wc -l <"$sums" | tr -d ' ') files)"
        else
            say "  sha256 FAILED: $sums"
            rc=1
        fi
    done <<EOF
$(find "$dir" -name SHA256SUMS -type f | sort)
EOF
    [ $n -gt 0 ] || { say "FAIL: no SHA256SUMS anywhere under $dir"; rc=1; }
    python3 - "$dir" <<'PY'
import os, sys
root = sys.argv[1]
covered = set()
for d, _, fs in os.walk(root):
    if "SHA256SUMS" in fs:
        for line in open(os.path.join(d, "SHA256SUMS")):
            parts = line.rstrip("\n").split("  ", 1)
            if len(parts) == 2:
                covered.add(os.path.normpath(os.path.join(d, parts[1].lstrip("*"))))
unc = {}
for d, _, fs in os.walk(root):
    for f in fs:
        p = os.path.normpath(os.path.join(d, f))
        if f != "SHA256SUMS" and p not in covered:
            top = os.path.relpath(p, root).split(os.sep)[0]
            unc.setdefault(top, []).append(os.path.relpath(p, root))
for top, files in sorted(unc.items()):
    print(f"  not covered by any SHA256SUMS under {top}/: {len(files)} files (e.g. {files[0]})")
PY
    return $rc
}

# ----------------------------------------------------------------- bucket --

cmd_bucket() {
    local b counts ver rc=0 out
    drill_bucket_name "${BUCKET:-}" BUCKET
    [ -z "${LOGBUCKET:-}" ] || drill_bucket_name "$LOGBUCKET" LOGBUCKET
    identity
    for b in "$BUCKET" ${LOGBUCKET:+"$LOGBUCKET"}; do
        counts=$(s3_count "s3://$b/") || die "cannot list s3://$b/ — not deleting what cannot be counted"
        ver=$(aws_ s3api get-bucket-versioning --bucket "$b" --output text --query Status 2>&1) || die "get-bucket-versioning $b: $ver"
        say "s3://$b: objects=${counts% *} bytes=${counts#* } versioning=${ver:-None} region=$(aws_ s3api get-bucket-location --bucket "$b" --output text --query LocationConstraint 2>&1)"
        case "$ver" in
            Enabled|Suspended) die "s3://$b has versioning $ver: rb --force would leave its versions; delete versions first" ;;
        esac
    done
    if ! has_flag --yes "$@"; then
        say "would run: aws s3 rb s3://$BUCKET --force${LOGBUCKET:+ ; aws s3 rb s3://$LOGBUCKET --force}"
        say "nothing deleted — re-run with --yes"
        return 2
    fi
    for b in "$BUCKET" ${LOGBUCKET:+"$LOGBUCKET"}; do
        aws_ s3 rb "s3://$b" --force || { say "FAIL: rb s3://$b"; rc=1; continue; }
        out=$(aws_ s3api head-bucket --bucket "$b" 2>&1)
        case "$out" in
            *404*|*"Not Found"*|*NoSuchBucket*) say "s3://$b: gone (head-bucket 404)" ;;
            *) say "FAIL: s3://$b still answers head-bucket: ${out:-200 OK}"; rc=1 ;;
        esac
    done
    return $rc
}

# ----------------------------------------------------------------- policy --

cmd_policy() {
    local out
    [ -n "${POLICY:-}" ] || die "POLICY (the inline policy name) is required"
    identity
    if ! out=$(aws_ iam get-role-policy --role-name "$ROLE" --policy-name "$POLICY" --output json 2>&1); then
        case "$out" in
            *NoSuchEntity*) say "inline policy $POLICY on $ROLE: already absent"; return 0 ;;
            *) die "get-role-policy failed (NOT evidence of absence): $out" ;;
        esac
    fi
    say "inline policy $POLICY on $ROLE:"
    printf '%s\n' "$out"
    if ! has_flag --yes "$@"; then
        say "would run: aws iam delete-role-policy --role-name $ROLE --policy-name $POLICY"
        say "nothing deleted — re-run with --yes"
        return 2
    fi
    aws_ iam delete-role-policy --role-name "$ROLE" --policy-name "$POLICY" || die "delete-role-policy failed"
    out=$(aws_ iam get-role-policy --role-name "$ROLE" --policy-name "$POLICY" 2>&1)
    case "$out" in
        *NoSuchEntity*) say "inline policy $POLICY: deleted (get-role-policy answers NoSuchEntity)" ;;
        *) die "policy still readable after delete: $out" ;;
    esac
}

# ---------------------------------------------------------------- project --

trove() { # method path [json-body] -> prints "<http code> <body file>"; exit 1 if trove did not answer
    local body_file code
    body_file=$(mktemp "${TMPDIR:-/tmp}/trove.XXXXXX")
    if [ -n "${3:-}" ]; then
        code=$("$CURL_BIN" -sk -o "$body_file" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $TROVE_TOKEN" \
            -H 'Content-Type: application/json' -d "$3" "$TROVE_API$2") || { rm -f "$body_file"; return 1; }
    else
        code=$("$CURL_BIN" -sk -o "$body_file" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $TROVE_TOKEN" \
            "$TROVE_API$2") || { rm -f "$body_file"; return 1; }
    fi
    [ "$code" != 000 ] || { rm -f "$body_file"; return 1; }
    printf '%s %s\n' "$code" "$body_file"
}

project_rows() { # body-file name -> "id<TAB>status<TAB>servers" per exact-name match; exit 3 if not a project list
    python3 - "$1" "$2" <<'PY'
import json, sys
try:
    doc = json.load(open(sys.argv[1]))
    rows = doc["data"]
except (ValueError, KeyError, TypeError):
    sys.exit(3)
for r in rows:
    if r.get("name") == sys.argv[2]:
        print(f"{r.get('id')}\t{r.get('status')}\t{r.get('totalServersCount')}")
PY
}

cmd_project() {
    local name=${1:-} res code body rows n id status started
    [ -n "$name" ] || die "usage: teardown.sh project <trove project name> [--yes]"
    res=$(trove GET /projects) || die "trove did not answer at $TROVE_API (is the backend up?) — nothing known"
    code=${res%% *}; body=${res#* }
    [ "$code" = 200 ] || die "GET /projects answered $code: $(cat "$body")"
    rows=$(project_rows "$body" "$name") || die "GET /projects did not return a project list: $(head -c 300 "$body")"
    rm -f "$body"
    n=$(printf '%s' "$rows" | grep -c . || true)
    if [ "$n" = 0 ]; then
        say "trove has no project named '$name' (already deleted, or a wrong name — check GET /projects)"
        return 0
    fi
    [ "$n" = 1 ] || die "$n projects are named '$name' — refusing to guess: $rows"
    id=$(printf '%s' "$rows" | cut -f1)
    say "project '$name': id=$id status=$(printf '%s' "$rows" | cut -f2) servers=$(printf '%s' "$rows" | cut -f3)"
    if ! has_flag --yes "$@"; then
        say "would run: POST $TROVE_API/projects/delete {\"projectId\":$id}"
        say "nothing deleted — re-run with --yes"
        return 2
    fi
    res=$(trove POST /projects/delete "{\"projectId\":$id}") || die "trove did not answer the delete"
    code=${res%% *}; body=${res#* }
    rm -f "$body"
    [ "$code" = 200 ] || die "POST /projects/delete answered $code"
    say "delete accepted; trove tears the cluster down in the background — waiting for the row to go"
    started=$(date +%s)
    while :; do
        res=$(trove GET "/projects?id=$id") || die "trove stopped answering while waiting"
        code=${res%% *}; body=${res#* }
        rows=$(project_rows "$body" "$name") || die "GET /projects?id=$id did not return a project list"
        rm -f "$body"
        [ -z "$rows" ] && break
        status=$(printf '%s' "$rows" | cut -f2)
        [ "$status" = Failure ] && die "trove marked project $id 'Failure' — the delete did not complete; check trove's log"
        [ $(($(date +%s) - started)) -ge "$PROJECT_WAIT_SECS" ] && die "project $id still present (status $status) after ${PROJECT_WAIT_SECS}s"
        say "  project $id: status=$status ($(($(date +%s) - started))s)"
        sleep "$POLL_SECS"
    done
    res=$(trove POST /projects/delete "{\"projectId\":$id}") || die "trove did not answer the second delete"
    code=${res%% *}; body=${res#* }
    rm -f "$body"
    [ "$code" = 404 ] || die "second POST /projects/delete answered $code, not 404"
    say "project $id: deleted (a second delete answers 404)"
}

# ---------------------------------------------------------------- zeroset --

cmd_zeroset() {
    local cluster=${1:-} fails=0 out n res code body matched ghosts orphans at
    [ -n "$cluster" ] || die "usage: teardown.sh zeroset <cluster-name> [--ignore-foreign-instances]"
    case "$cluster" in *[!A-Za-z0-9_-]*) die "cluster name '$cluster' has unexpected characters" ;; esac
    identity
    row() { # label count [detail] — a count that is not a number is a failed query, never a zero
        case "$2" in
            0) say "  ZERO  $1: 0" ;;
            ''|*[!0-9]*) say "  FAIL  $1: no count (query or parse failed — NOT a zero)"; fails=$((fails + 1)) ;;
            *) say "  FAIL  $1: $2${3:+  $3}"; fails=$((fails + 1)) ;;
        esac
    }
    Q="${TMPDIR:-/tmp}/zeroset.$$.json"
    q() { # aws args... -> $Q; runs in THIS shell so a failure is counted — never a zero
        if ! aws_ "$@" --output json >"$Q" 2>&1; then
            say "  FAIL  query failed (NOT a zero): aws $*: $(head -c 300 "$Q")"
            fails=$((fails + 1))
            return 1
        fi
    }
    say "zero set for cluster '$cluster', region $REGION"

    if q ec2 describe-security-groups --filters Name=group-name,Values=default; then
        n=$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))["SecurityGroups"]))' "$Q")
        if [ "${n:-0}" -ge 1 ] 2>/dev/null; then say "  ok    control: the region's default security group is visible ($n)"; else say "  FAIL  control: no default security group visible — this profile/region cannot see EC2"; fails=$((fails + 1)); fi
    fi

    if q ec2 describe-instances --filters Name=instance-state-name,Values=pending,running,shutting-down,stopping,stopped; then
        python3 - "$Q" "$cluster" >"$Q.instances" <<'PY'
import json, sys
cluster = sys.argv[2]
mine, other = [], []
for r in json.load(open(sys.argv[1]))["Reservations"]:
    for i in r["Instances"]:
        tags = {t["Key"]: t["Value"] for t in i.get("Tags", [])}
        desc = "%s %s %s Name=%s trove:cluster=%s" % (i["InstanceId"], i["InstanceType"], i["State"]["Name"],
                                                      tags.get("Name"), tags.get("trove:cluster"))
        (mine if tags.get("trove:cluster") == cluster or any(cluster in v for v in tags.values()) else other).append(desc)
print(len(mine))
print(len(mine) + len(other))
for d in mine:
    print("cluster  " + d)
for d in other:
    print("foreign  " + d)
PY
        n=$(sed -n 1p "$Q.instances")
        row "non-terminated instances of cluster $cluster" "$n"
        n=$(sed -n 2p "$Q.instances")
        if has_flag --ignore-foreign-instances "$@" && [ -n "$n" ] && [ "$n" != 0 ] && [ "$(sed -n 1p "$Q.instances")" = 0 ]; then
            say "  NOTE  non-terminated instances in $REGION (all): $n — not failed (--ignore-foreign-instances)"
        else
            row "non-terminated instances in $REGION (all)" "$n"
        fi
        sed -n '3,$p' "$Q.instances" | sed 's/^/          /'
        rm -f "$Q.instances"
    fi

    count_json() { # python-expression-over-d
        python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print($1)" "$Q" 2>/dev/null
    }
    if q ec2 describe-spot-instance-requests --filters Name=state,Values=open,active; then
        row "open/active spot requests" "$(count_json 'len(d["SpotInstanceRequests"])')"
    fi
    if q ec2 describe-volumes --filters Name=status,Values=available; then
        row "available EBS volumes" "$(count_json 'len(d["Volumes"])')" "$(count_json '" ".join(v["VolumeId"] for v in d["Volumes"])')"
    fi
    if q ec2 describe-security-groups --filters "Name=group-name,Values=trove-$cluster-*"; then
        row "security groups trove-$cluster-*" "$(count_json 'len(d["SecurityGroups"])')" "$(count_json '" ".join(g["GroupId"] for g in d["SecurityGroups"])')"
    fi
    if q s3api list-buckets; then
        # an answer without Owner is not a bucket listing: count_json prints nothing, row fails it
        row "buckets flint-lean-writers-*" \
            "$(count_json 'len([b for b in d["Buckets"] if b["Name"].startswith("flint-lean-writers-")]) if "Owner" in d else ""')" \
            "$(count_json '" ".join(b["Name"] for b in d.get("Buckets", []) if b["Name"].startswith("flint-lean-writers-"))')"
    fi
    if q iam list-role-policies --role-name "$ROLE"; then
        say "          inline policies on $ROLE: $(count_json '", ".join(d["PolicyNames"]) or "(none)"')"
        row "drill inline policy on $ROLE (${POLICY:-any flint-lean-writers*})" \
            "$(POLICY_WANT="${POLICY:-}" count_json 'len([p for p in d["PolicyNames"] if (__import__("os").environ["POLICY_WANT"] and p == __import__("os").environ["POLICY_WANT"]) or "flint-lean-writers" in p])')"
    fi
    if q cloudtrail describe-trails; then
        row "CloudTrail trails flint-lean-writers*" "$(count_json 'sum(1 for t in d["trailList"] if "flint-lean-writers" in t["Name"])')"
    fi
    rm -f "$Q"

    if res=$(trove GET /aws/orphans); then
        code=${res%% *}; body=${res#* }
        if [ "$code" = 200 ] && n=$(python3 - "$body" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
orphans = len(d.get("orphansKnownCluster", [])) + len(d.get("orphansDeletedCluster", []))
print(f"{d['matched']} {len(d['ghostRows'])} {orphans} {d.get('at')}")
PY
        ); then
            read -r matched ghosts orphans at <<ORPHANS
$n
ORPHANS
            row "trove reconcile: matched" "$matched" "(pass at $at)"
            row "trove reconcile: ghostRows" "$ghosts"
            row "trove reconcile: orphan instances" "$orphans"
        else
            say "  FAIL  trove /aws/orphans answered $code or an unexpected document"
            fails=$((fails + 1))
        fi
        rm -f "$body"
    else
        say "  FAIL  trove did not answer /aws/orphans (NOT a zero)"
        fails=$((fails + 1))
    fi

    if [ $fails = 0 ]; then say "ZERO SET: yes"; else say "ZERO SET: NO ($fails failing)"; fi
    [ $fails = 0 ]
}

case "${1:-}" in
    pull) shift; cmd_pull "$@" ;;
    bucket) shift; cmd_bucket "$@" ;;
    policy) shift; cmd_policy "$@" ;;
    project) shift; cmd_project "$@" ;;
    zeroset) shift; cmd_zeroset "$@" ;;
    *) sed -n '2,50p' "$SELF" >&2; exit 2 ;;
esac
