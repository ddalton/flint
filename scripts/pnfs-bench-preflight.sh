#!/usr/bin/env bash
# Refuse to benchmark a pNFS fleet whose rig is not actually what the
# benchmark assumes. Exits non-zero on any violation — wire it in FRONT of
# every scaling/throughput run and let it fail the run, not warn.
#
#   ./scripts/pnfs-bench-preflight.sh [expected-ds-count]
#
# WHY THIS EXISTS. On 2026-08-01 a pNFS DS-scaling sweep was run three
# times and every number was void, because all five data servers' BACKING
# VOLUMES had been provisioned onto ONE node. The DS *pods* were spread
# across five nodes — which is what was checked, and which is exactly why
# it looked correct — but their storage all pointed at a single NVMe. The
# sweep measured one device at every stripe width and reported "pNFS does
# not scale" (1.11x). With the rig corrected the same sweep measured 3.50x.
#
# The cause was ordering: `helm` ran before disk-init, so when the
# StatefulSet's PVCs bound, only one node had a blobstore and all of them
# landed there. Blobstore init is what makes a node visible to placement,
# so a node initialised later never gets used.
#
# Each check below corresponds to a specific way a run has silently
# produced a confident wrong answer. None of them is hypothetical.
set -uo pipefail
EXPECTED_DS=${1:-}
: "${KUBECONFIG:?set KUBECONFIG to the target cluster}"
NS=${FLINT_NS:-flint-system}
FAIL=0
note() { printf "  %s\n" "$*"; }
bad()  { printf "  ✗ %s\n" "$*"; FAIL=1; }
ok()   { printf "  ✓ %s\n" "$*"; }

echo "▶ pNFS benchmark preflight  (namespace $NS)"
echo

# ── 1. Every node's blobstore is initialised ────────────────────────────
# A node with blobstore_initialized=false is INVISIBLE to placement. It
# will not host volumes, so the fleet is quietly narrower than the node
# count suggests, and whichever node was ready first absorbs everything.
echo "1. blobstore initialised on every node"
UNINIT=0 TOTAL=0
for pod in $(kubectl get pods -n "$NS" -l app=flint-csi-node \
             -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null); do
  node=$(kubectl get pod -n "$NS" "$pod" -o jsonpath='{.spec.nodeName}' 2>/dev/null)
  TOTAL=$((TOTAL+1))
  kubectl port-forward -n "$NS" "pod/$pod" 19097:9081 >/dev/null 2>&1 &
  pf=$!; sleep 2
  init=$(curl -s -m 6 -X POST http://localhost:19097/api/disks \
           -H 'Content-Type: application/json' -d '{}' 2>/dev/null | python3 -c "
import sys,json
# ANY initialised data disk counts, not the FIRST one found. This used to
# take the first non-system disk and break, which silently assumed one data
# disk per node — true on i3en.xlarge/i4i.xlarge, false the moment the rig
# grows. On i3en.6xlarge (TWO 7500 GB NVMe, 0000:00:1e.0 and 0000:00:1f.0)
# initialising 1f.0 and leaving 1e.0 alone is a perfectly good rig, and the
# old probe failed all 8 nodes on it — a preflight that blocks a healthy
# cluster is as costly as one that passes a broken one, because the next
# move is to distrust the preflight.
try:
    d=json.load(sys.stdin); ds=d.get('disks') if isinstance(d,dict) else d
    data=[x for x in ds if not x.get('is_system_disk')]
    if not data: print('no-data-disks')
    else:
        n=sum(1 for x in data if x.get('blobstore_initialized'))
        print('yes' if n else 'no', f'({n}/{len(data)} data disks)')
except Exception: print('unknown')
" 2>/dev/null)
  kill $pf 2>/dev/null; wait $pf 2>/dev/null
  case "$init" in
    yes*) ;;
    *)   bad "$node: blobstore not initialised (reported '$init')"; UNINIT=$((UNINIT+1));;
  esac
done
[ "$TOTAL" = 0 ] && bad "no flint-csi-node pods found — is the driver deployed?"
[ "$UNINIT" = 0 ] && [ "$TOTAL" -gt 0 ] && ok "all $TOTAL nodes initialised"
echo

# ── 2. DS backing volumes are on DISTINCT nodes ─────────────────────────
# THE CHECK THAT WOULD HAVE SAVED THE DAY. Pod spread is not storage
# spread; only this one can tell them apart.
echo "2. DS backing volumes on distinct nodes"
PLACEMENT=$(kubectl get pv -o json 2>/dev/null | python3 -c "
import sys,json,collections
c=collections.Counter(); rows=[]
for i in json.load(sys.stdin)['items']:
    if i.get('status',{}).get('phase')!='Bound': continue
    cr=i['spec'].get('claimRef') or {}
    n=cr.get('name','')
    if not n.startswith('data-flint-pnfs-ds'): continue
    a=(i['spec'].get('csi') or {}).get('volumeAttributes',{})
    node=a.get('disk.chert.us/node-name','<unknown>')
    c[node]+=1; rows.append((n,node))
for n,node in sorted(rows): print('ROW',n,node)
print('DISTINCT',len(c)); print('COUNT',len(rows))
for k,v in c.items(): print('NODE',k,v)
" 2>/dev/null)
echo "$PLACEMENT" | awk '$1=="ROW"{printf "     %-24s -> %s\n", $2, $3}'
DISTINCT=$(echo "$PLACEMENT" | awk '$1=="DISTINCT"{print $2}')
VOLCOUNT=$(echo "$PLACEMENT" | awk '$1=="COUNT"{print $2}')
DSPODS=$(kubectl get pods -n "$NS" -l app=flint-pnfs-ds --no-headers 2>/dev/null | wc -l | tr -d ' ')
[ -n "$EXPECTED_DS" ] && [ "$DSPODS" != "$EXPECTED_DS" ] && \
  bad "expected $EXPECTED_DS data servers, found $DSPODS"
if [ "${VOLCOUNT:-0}" = 0 ]; then
  bad "no bound DS backing volumes found"
elif [ "${DISTINCT:-0}" != "${VOLCOUNT:-1}" ]; then
  bad "$VOLCOUNT DS volumes share only $DISTINCT nodes — the stripe is COLLAPSED"
  echo "$PLACEMENT" | awk '$1=="NODE" && $3>1 {printf "       %s hosts %s DS volumes\n", $2, $3}'
  note "  every scaling number from this rig will measure $DISTINCT device(s), not $VOLCOUNT"
  note "  fix: ensure disk-init completed on ALL nodes, then delete the"
  note "       flint-pnfs-ds StatefulSet and its PVCs so they re-place"
else
  ok "$VOLCOUNT DS volumes across $DISTINCT distinct nodes"
fi
echo

# ── 2b. Each data server sits ON its own backing volume ─────────────────
# Check 2 proves the VOLUMES are spread. It does not prove each DS pod is
# on the node holding its volume — and flint PVs carry NO nodeAffinity, so
# a rescheduled DS pod serves its disk REMOTELY over NVMe-oF, crossing the
# network twice per read, with nothing anywhere reporting it.
#
# This is not hypothetical: scaling the DS StatefulSet down and back up on
# 2026-08-02 left flint-pnfs-ds-1's pod on the BENCHMARK CLIENT node while
# its volume stayed three nodes away. Both failure modes at once — remote
# I/O, and a data server competing with the client for CPU — and the sweep
# in flight looked completely normal.
#
# Alignment happens only at FIRST creation, when WaitForFirstConsumer binds
# the volume to wherever the pod already landed. Once a PVC is bound, later
# scheduling is free to put the pod anywhere. The fix is to delete the
# StatefulSet AND its claims so both re-place together.
echo "2b. each data server is co-located with its own volume"
MISALIGNED=0 CHECKED=0
for pod in $(kubectl get pods -n "$NS" -l app=flint-pnfs-ds \
             -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null); do
  idx=${pod##*-}
  pnode=$(kubectl get pod -n "$NS" "$pod" -o jsonpath='{.spec.nodeName}' 2>/dev/null)
  pv=$(kubectl get pvc -n "$NS" "data-flint-pnfs-ds-$idx" \
       -o jsonpath='{.spec.volumeName}' 2>/dev/null)
  vnode=$(kubectl get pv "$pv" \
          -o jsonpath='{.spec.csi.volumeAttributes.disk\.chert\.us/node-name}' 2>/dev/null)
  [ -z "$pnode" ] || [ -z "$vnode" ] && continue
  CHECKED=$((CHECKED+1))
  if [ "$pnode" != "$vnode" ]; then
    bad "$pod runs on $pnode but its volume lives on $vnode — serving REMOTELY"
    MISALIGNED=$((MISALIGNED+1))
  fi
done
if [ "$MISALIGNED" = 0 ] && [ "$CHECKED" -gt 0 ]; then
  ok "all $CHECKED data servers serve their own disk locally"
else
  [ "$MISALIGNED" -gt 0 ] && \
    note "  fix: delete the flint-pnfs-ds StatefulSet AND its data-* PVCs,"
  [ "$MISALIGNED" -gt 0 ] && \
    note "       then let it recreate so pod and volume place together"
fi
echo

# ── 3. The benchmark client hosts no data server ────────────────────────
# A co-located client both serves and consumes: part of its traffic is
# node-local (flattering the number) and it contends for the same CPUs
# (depressing it). Either way the result is uninterpretable.
echo "3. a DS-free node exists for the client"
DSNODES=$(kubectl get pods -n "$NS" -l app=flint-pnfs-ds \
          -o jsonpath='{range .items[*]}{.spec.nodeName}{"\n"}{end}' 2>/dev/null | sort -u)
FREE=""
for n in $(kubectl get nodes -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null); do
  kubectl get node "$n" -o jsonpath='{.spec.taints[*].key}' 2>/dev/null | grep -q control-plane && continue
  echo "$DSNODES" | grep -qx "$n" || FREE="$FREE $n"
done
WORKERS=$(kubectl get nodes -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null | wc -l | tr -d ' ')
if [ "${WORKERS:-0}" = 0 ]; then
  bad "no nodes visible — cluster unreachable or KUBECONFIG wrong"
elif [ -z "$FREE" ]; then
  bad "every worker hosts a data server — no clean client placement"
  note "  co-located clients invalidated two multi-client runs on 2026-08-01:"
  note "  a client needs ~3.8 of 4 cores at 1 GB/s, and so does the DS beside it"
else
  ok "client can run on:$FREE"
fi
echo

# ── 4. MDS is actually up ───────────────────────────────────────────────
echo "4. MDS ready"
MDSREADY=$(kubectl get pods -n "$NS" -l app=flint-pnfs-mds \
  -o jsonpath='{range .items[*]}{.status.containerStatuses[*].ready}{"\n"}{end}' 2>/dev/null | grep -c true)
[ "${MDSREADY:-0}" -ge 1 ] && ok "MDS ready" || bad "no ready MDS pod"
echo

if [ "$FAIL" != 0 ]; then
  echo "✗ PREFLIGHT FAILED — do not benchmark this rig. Any number it"
  echo "  produces will be confidently wrong in a way that looks plausible."
  exit 1
fi
echo "✓ preflight passed — rig matches what a scaling benchmark assumes"
