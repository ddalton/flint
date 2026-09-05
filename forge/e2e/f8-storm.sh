#!/usr/bin/env bash
# FALSIFIER 8 — the clone storm, on EC2 only.
#
# kind would measure its host's loopback, not a NIC and not S3 fan-out,
# so this is meaningless anywhere else.
#
# THREE ARMS, and the third is the one people forget:
#   treatment  bundle advertised AND transfer.bundleURI=true
#   ctl-noadv  bundle advertisement withdrawn      -> server carries all
#   ctl-noopt  advertised but the client opts OUT  -> identical to
#              ctl-noadv, which is what proves the ADVERTISEMENT alone
#              does nothing. `transfer.bundleURI` defaults to false, so
#              this is also the accidental production configuration.
#
# THE ORACLE IS SERVER EGRESS, read from the pod's own NIC counter
# before and after. Wall-clock is not the oracle: a storm that is
# slower but off the server's NIC still passes, and a fast one that
# saturates the NIC still fails.
#
# CREDENTIAL SCOPE MATTERS AND IS EASY TO GET WRONG. A global
# `http.extraHeader` is sent to EVERY host git talks to, including the
# presigned S3 URL — and S3 rejects a presigned request that also
# carries an Authorization header. git then logs "failed to download
# bundle" only under GIT_TRACE2 and silently falls back to a full
# fetch, so the lever looks inert for a reason that has nothing to do
# with forge. Scope the header to the door's URL.
set -uo pipefail
NS=${NS:-agents}; REPO=${REPO:-proj}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
N=${N:-1000}                 # total clones
WIDTH=${WIDTH:-128}          # in flight per client pod
CLIENTS=${CLIENTS:-2}        # client pods (one per big node)
TAG=${TAG:-1.46.0-forge.5}

tx() { kubectl exec -n "$NS" deploy/forge-proj -c git-http -- \
         cat /sys/class/net/eth0/statistics/tx_bytes 2>/dev/null; }

repo_bytes() { kubectl exec -n "$NS" deploy/forge-proj -c syncer -- \
  sh -c 'git --git-dir=/repo/'"$NS"'/'"$REPO"'.git count-objects -v | awk -F": " "/size-pack/{print \$2*1024}"'; }

# One clone runner per client pod.
#
# BOUNDED CONCURRENCY, and the honesty that goes with it. A truly
# simultaneous 1,000 clones of a 40 MiB repository needs more RAM than
# this fleet has (git peaks ~80-150 MiB resolving a pack), so each pod
# runs a fixed-width pool and the script reports the width it actually
# achieved rather than implying 1,000 at once. Each clone is deleted the
# moment it finishes, so peak storage is width x repo, not N x repo.
storm_one() { # $1=pod $2=count $3=opt-in $4=start-epoch $5=width
  kubectl exec -n "$NS" "$1" -- sh -c "
    T=\$(cat /var/run/secrets/forge/token)
    H=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 -w0)\"
    rm -rf /storm/* /tmp/ok /tmp/no 2>/dev/null; : > /tmp/ok; : > /tmp/no
    while [ \"\$(date +%s)\" -lt $4 ]; do :; done
    i=0
    while [ \$i -lt $2 ]; do
      (
        d=/storm/c\$i
        if git -c http.$DOOR/.extraHeader=\"\$H\" -c transfer.bundleURI=$3 \
             clone -q --bare $DOOR/git/$NS/$REPO.git \$d >/dev/null 2>&1
        then echo 1 >> /tmp/ok; else echo 1 >> /tmp/no; fi
        rm -rf \$d
      ) &
      i=\$((i+1))
      # alpine's ash has no \`wait -n\` and no usable \`jobs -p\` when
      # non-interactive, so the pool is a BATCH: launch \$5, wait for
      # all of them, launch the next \$5.
      if [ \$(( i % $5 )) -eq 0 ]; then wait; fi
    done
    wait
    printf 'ok=%s fail=%s\n' \"\$(wc -l < /tmp/ok)\" \"\$(wc -l < /tmp/no)\"
  " 2>&1 | tail -1
}

server_id() { kubectl get pods -n "$NS" -l chert.us/repo=proj \
  -o jsonpath='{.items[0].metadata.uid}' 2>/dev/null; }

arm() { # $1=label $2=opt-in $3=advertised(yes|no)
  echo "-- arm: $1  (client opt-in=$2, advertised=$3) --"
  if [ "$3" = no ]; then
    kubectl exec -n "$NS" deploy/forge-proj -c syncer -- \
      git --git-dir=/repo/$NS/$REPO.git config uploadpack.advertiseBundleURIs false >/dev/null
  else
    kubectl exec -n "$NS" deploy/forge-proj -c syncer -- \
      git --git-dir=/repo/$NS/$REPO.git config uploadpack.advertiseBundleURIs true >/dev/null
  fi

  # THE GUARD. If the server pod is replaced mid-arm its NIC counter
  # resets and the delta is meaningless — the first calibration run
  # reported NEGATIVE egress that way and would happily have reported a
  # small positive one. Identity is checked, not assumed.
  local id0 id1 before after start per t0 t1 mib exp
  id0=$(server_id)
  per=$(( N / CLIENTS )); start=$(( $(date +%s) + 15 ))
  before=$(tx); t0=$(date +%s)
  local results=""
  for p in $PODS; do results="$results$(storm_one "$p" "$per" "$2" "$start" "$WIDTH")  "; done
  t1=$(date +%s); after=$(tx); id1=$(server_id)

  if [ -z "$id0" ] || [ "$id0" != "$id1" ]; then
    echo "   INVALID — the server pod was replaced during this arm; the counter reset."
    echo "$1 INVALID - $((t1-t0))" >> "$RESULTS"; return
  fi
  # A STORM OF FAILED CLONES MOVES NO BYTES, so a broken arm reports
  # 0 MiB and reads as the treatment working perfectly. Assert that the
  # clones actually happened before believing any number.
  local okc
  okc=$(printf '%s' "$results" | tr ' ' '\n' | sed -n 's/^ok=//p' | paste -sd+ - | bc)
  okc=${okc:-0}
  if [ "$okc" -lt "$N" ]; then
    echo "   INVALID — only $okc/$N clones succeeded; egress is not a measurement of anything."
    echo "$1 INVALID($okc/$N) - $((t1-t0))" >> "$RESULTS"; return
  fi
  mib=$(echo "scale=1; ($after-$before)/1048576" | bc)
  exp=$(echo "scale=1; $N * $RB / 1048576" | bc)
  printf '   %s\n   server egress %s MiB   (server-carried would be ~%s MiB)   %ss\n' \
    "$results" "$mib" "$exp" "$((t1-t0))"
  echo "$1 $mib $exp $((t1-t0))" >> "$RESULTS"
}

PODS=""
NODES=$(kubectl get nodes -l '!node-role.kubernetes.io/control-plane' \
        -o jsonpath='{range .items[*]}{.metadata.name} {.metadata.labels.node\.kubernetes\.io/instance-type}{"\n"}{end}' \
        | awk '$2=="i3en.2xlarge"{print $1}')
i=0
for node in $NODES; do
  i=$((i+1)); [ $i -gt "$CLIENTS" ] && break
  p="stormer-$i"; PODS="$PODS $p"
  kubectl get pod -n "$NS" "$p" >/dev/null 2>&1 || \
    AGENT=$p NODE=$node TAG=$TAG envsubst '$AGENT $NODE $TAG' \
      < "$(dirname "$0")/stormer.yaml.tpl" | kubectl apply -f - >/dev/null
done
kubectl wait -n "$NS" --for=condition=Ready $(for p in $PODS; do echo -n "pod/$p "; done) --timeout=300s >/dev/null

RB=$(repo_bytes); RESULTS=$(mktemp)
echo "repo pack = $(echo "scale=1; $RB/1048576" | bc) MiB   N=$N over $CLIENTS clients, ${WIDTH}-wide each ($((WIDTH*CLIENTS)) in flight)"
echo ""
arm treatment true  yes
arm ctl-noopt  false yes
arm ctl-noadv  false no
echo ""
echo "── summary (MiB off the server's NIC) ──"; column -t "$RESULTS"
