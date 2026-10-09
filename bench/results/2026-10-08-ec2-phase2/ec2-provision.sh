#!/usr/bin/env bash
# ec2-provision.sh -- the Phase 2 cluster (bench3): 3 x i4i.2xlarge, all spot
# (control plane included), us-east-2, through trove. Approved 2026-10-08:
# competitors only (Mayastor, Longhorn v2, Rook-Ceph); Flint re-runs after F74.
#
# Every server row must exist BEFORE the commit: trove commits whatever rows
# there are, and a first attempt (project 172) committed a zero-worker cluster
# after three malformed servers/create bodies (bash 3.2 mangled a nested
# `$(curl ... -d "{\"...\"}")`; bodies are now built with printf first).
set -u
API=https://localhost:8080/api/v1; H='Authorization: Bearer trove-dummy-token'; J='Content-Type: application/json'
OUT="$(cd "$(dirname "$0")" && pwd)"
log() { echo "$(date -u +%FT%TZ) $*" | tee -a "$OUT/provision.log" >&2; }
post() { curl -sk -m 60 -w ' http=%{http_code}' -H "$H" -H "$J" -X POST "$API/$1" -d "$2"; }

resp=$(post projects '{"name":"bench3","cloudCredentialId":2,"aws":{"region":"us-east-2","controlPlaneInstanceType":"i4i.2xlarge","controlPlaneNodeType":"aws_spot","workerNodeType":"aws_spot","workerInstanceType":"i4i.2xlarge"}}')
log "projects: $resp"
P=$(printf '%s' "${resp% http=*}" | python3 -c 'import json,sys; d=json.load(sys.stdin); d=d.get("data",d); print(int(d["id"]))') || { log "no project id -- nothing committed"; exit 1; }

for r in Kubemaster:bench3-cp Kubeworker:bench3-w1 Kubeworker:bench3-w2; do
  body=$(printf '{"projectId":%s,"name":"%s","role":"%s","nodeType":"aws_spot","awsRegion":"us-east-2","awsInstanceType":"i4i.2xlarge"}' "$P" "${r#*:}" "${r%%:*}")
  out=$(post servers/create "$body")
  log "servers/create $r: $out"
  case "$out" in *'"id"'*' http=200') ;; *) log "server row for $r failed -- NOT committing project $P; delete it: POST projects/delete {\"projectId\":$P}"; exit 1 ;; esac
done

out=$(post project-deployment/commit "{\"projectId\":$P}")
log "commit: $out"
case "$out" in *' http=200') echo "$P" > "$OUT/project.id" ;; *) log "commit failed"; exit 1 ;; esac
