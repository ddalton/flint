set -u
export HOME=/root
LOG=/mnt/nvme/drill2.log; : > $LOG
say() { echo "$*" | tee -a $LOG; }
run() { local label=$1; shift; say "--- $label"; "$@" > /mnt/nvme/step.out 2>&1; local rc=$?; cat /mnt/nvme/step.out >> $LOG; say "exit=$rc"; tail -n 6 /mnt/nvme/step.out | cut -c1-400; return $rc; }
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
aws s3 cp s3://flint-ozone-probe/rig/flint-lean-gateway /usr/local/bin/flint-lean-gateway --quiet --region us-west-1 && chmod +x /usr/local/bin/flint-lean-gateway
export AWS_ACCESS_KEY_ID=flint AWS_SECRET_ACCESS_KEY=flintsecret AWS_REGION=us-east-1 AWS_DEFAULT_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true
S3G=http://localhost:9878; export FLINT_SYNC_ENDPOINT=$S3G FLINT_SYNC_BUCKET=lean FLINT_SYNC_PREFIX=ws1
A=/mnt/nvme/wsA; B=/mnt/nvme/wsB; H=/mnt/nvme/wsH; rm -rf $H; mkdir -p $H
TOKEN=0123456789abcdef0123456789
pkill -x flint-lean-gateway 2>/dev/null; sleep 1
FLINT_LEAN_GW_LISTEN=127.0.0.1:8091 FLINT_LEAN_GW_BUCKET=lean FLINT_LEAN_GW_ENDPOINT=$S3G FLINT_LEAN_GW_TOKEN=$TOKEN FLINT_LEAN_GW_WORKSPACES=ws1=ws1 setsid nohup flint-lean-gateway > /mnt/nvme/gw.log 2>&1 < /dev/null &
sleep 3; say "gateway: $(curl -s -m 3 -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8091/lean/v1/ws1/files/nope) $(head -c 300 /mnt/nvme/gw.log | tr '\n' ' ')"
say "=== 1. HITL PUT of a NEW file through the gateway ==="
run "PUT docs/hitl.txt (If-None-Match:*)" curl -s -w '\nhttp=%{http_code}' -X PUT -H "Authorization: Bearer $TOKEN" -H 'If-None-Match: *' -H 'x-flint-author: dilip' --data-binary 'hitl bytes via gateway' http://127.0.0.1:8091/lean/v1/ws1/files/docs/hitl.txt
run "HEAD of the HITL object on Ozone (checksum headers?)" aws --endpoint-url $S3G s3api head-object --bucket lean --key ws1/files/docs/hitl.txt --checksum-mode ENABLED
say "inbox entries carrying crc64_b64: $(aws --endpoint-url $S3G s3api list-objects-v2 --bucket lean --prefix ws1/.flint --query 'Contents[].Key' --output text | tr '\t' '\n' | grep -i inbox | head -1 | xargs -I{} aws --endpoint-url $S3G s3 cp s3://lean/{} - 2>/dev/null | python3 -c 'import sys,json; d=json.load(sys.stdin); print([(e.get("path"),e.get("crc64_b64")) for e in d.get("entries",[])])' 2>&1 | head -c 300)"
say "=== 2. A's barrier consumes it (verify vs the gateway's CRC) and re-cites it ==="
export FLINT_SYNC_ROOT=$A; run "A barrier (consume + citation repair)" flint-sync barrier
say "A docs/hitl.txt: $(cat $A/docs/hitl.txt 2>/dev/null)"
say "A baseline entry: $(f=$(find $A/.flint -name 'baseline*' | head -1); python3 -c 'import json,sys; b=json.load(open(sys.argv[1])); print(b["entries"].get("docs/hitl.txt"))' "$f" 2>&1 | head -c 300)"
say "manifest cites docs/hitl.txt with: $(for k in $(aws --endpoint-url $S3G s3api list-objects-v2 --bucket lean --prefix ws1/.flint --query 'Contents[].Key' --output text); do aws --endpoint-url $S3G s3 cp s3://lean/$k - 2>/dev/null | python3 -c 'import sys,json
try:
  d=json.load(sys.stdin); e=(d.get("entries") or {}).get("docs/hitl.txt") if isinstance(d.get("entries"),dict) else None
  print(e) if e else None
except Exception: pass' 2>/dev/null; done | head -c 400)"
say "=== 3. a fresh reader verifies the HITL bytes against that CRC ==="
export FLINT_SYNC_ROOT=$H; run "H checkout" flint-sync checkout; say "H docs/hitl.txt: $(cat $H/docs/hitl.txt 2>/dev/null)"
say "=== 4. HITL OVERWRITE of a published file, then B sync ==="
et=$(aws --endpoint-url $S3G s3api head-object --bucket lean --key ws1/files/src/new.txt --query ETag --output text)
run "PUT src/new.txt (If-Match $et)" curl -s -w '\nhttp=%{http_code}' -X PUT -H "Authorization: Bearer $TOKEN" -H "If-Match: $et" --data-binary 'v3 via gateway' http://127.0.0.1:8091/lean/v1/ws1/files/src/new.txt
run "PUT src/new.txt with NO If-Match (expect 428)" curl -s -w '\nhttp=%{http_code}' -X PUT -H "Authorization: Bearer $TOKEN" --data-binary 'v4 via gateway' http://127.0.0.1:8091/lean/v1/ws1/files/src/new.txt
export FLINT_SYNC_ROOT=$B; run "B sync (inbox overlay -> verified against the gateway's CRC)" flint-sync sync; say "B src/new.txt: $(cat $B/src/new.txt 2>/dev/null)"
say "=== 5. gateway log tail ==="; tail -n 5 /mnt/nvme/gw.log | cut -c1-300 | tee -a $LOG
pkill -x flint-lean-gateway 2>/dev/null
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_EC2_METADATA_DISABLED
aws s3 cp $LOG s3://flint-ozone-probe/out/drill2.log --quiet --region us-west-1 && echo "log uploaded"
