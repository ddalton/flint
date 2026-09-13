set -u
export HOME=/root
LOG=/mnt/nvme/drill.log; : > $LOG
say() { echo "$*" | tee -a $LOG; }
run() { # run <label> <cmd...> : capture stderr+stdout to log, print exit + last lines
  local label=$1; shift; say "--- $label"; "$@" > /mnt/nvme/step.out 2>&1; local rc=$?; cat /mnt/nvme/step.out >> $LOG; say "exit=$rc"; tail -n 6 /mnt/nvme/step.out | cut -c1-400; return $rc; }
# binaries from the staging bucket (instance role creds)
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
aws s3 cp s3://flint-ozone-probe/rig/flint-sync /usr/local/bin/flint-sync --quiet && chmod +x /usr/local/bin/flint-sync && md5sum /usr/local/bin/flint-sync | cut -c1-32
# Ozone env
export AWS_ACCESS_KEY_ID=flint AWS_SECRET_ACCESS_KEY=flintsecret AWS_REGION=us-east-1 AWS_DEFAULT_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true
S3G=http://localhost:9878
export FLINT_SYNC_ENDPOINT=$S3G FLINT_SYNC_BUCKET=lean FLINT_SYNC_PREFIX=ws1
A=/mnt/nvme/wsA; B=/mnt/nvme/wsB; C=/mnt/nvme/wsC; rm -rf $A $B $C; mkdir -p $A $B $C
say "=== 1. wire-level conditional checks (aws cli $(aws --version | cut -d' ' -f1)) ==="
echo one > /tmp/one; echo two > /tmp/two
aws --endpoint-url $S3G s3api delete-object --bucket lean --key cond/k >/dev/null 2>&1
run "put if-none-match:* (fresh, expect 200)" aws --endpoint-url $S3G s3api put-object --bucket lean --key cond/k --body /tmp/one --if-none-match '*'
run "put if-none-match:* (exists, expect 412)" aws --endpoint-url $S3G s3api put-object --bucket lean --key cond/k --body /tmp/two --if-none-match '*'
et=$(aws --endpoint-url $S3G s3api head-object --bucket lean --key cond/k --query ETag --output text); say "etag now $et"
run "put if-match STALE (expect 412)" aws --endpoint-url $S3G s3api put-object --bucket lean --key cond/k --body /tmp/two --if-match '"0000000000000000000000000000dead"'
run "put if-match CURRENT (expect 200)" aws --endpoint-url $S3G s3api put-object --bucket lean --key cond/k --body /tmp/two --if-match "$et"
run "get if-match STALE (expect 412)" aws --endpoint-url $S3G s3api get-object --bucket lean --key cond/k --if-match '"0000000000000000000000000000dead"' /tmp/got
run "get if-match CURRENT (expect 200)" aws --endpoint-url $S3G s3api get-object --bucket lean --key cond/k --if-match "$(aws --endpoint-url $S3G s3api head-object --bucket lean --key cond/k --query ETag --output text)" /tmp/got
run "head with checksum-mode (which checksum headers come back?)" aws --endpoint-url $S3G s3api head-object --bucket lean --key cond/k --checksum-mode ENABLED
run "put with CRC32 checksum (cli default trailer)" aws --endpoint-url $S3G s3api put-object --bucket lean --key cond/crc32 --body /tmp/one --checksum-algorithm CRC32
run "put with CRC64NVME checksum header" aws --endpoint-url $S3G s3api put-object --bucket lean --key cond/crc64 --body /tmp/one --checksum-algorithm CRC64NVME
run "put with WRONG crc32 (does the store validate? expect 400 if it does)" aws --endpoint-url $S3G s3api put-object --bucket lean --key cond/badcrc --body /tmp/one --checksum-crc32 'AAAAAA=='
run "head cond/badcrc (present means the wrong checksum was ACCEPTED)" aws --endpoint-url $S3G s3api head-object --bucket lean --key cond/badcrc
say "=== 2. flint-store probes through the SDK ==="
export FLINT_SYNC_ROOT=$A
run "probe-conditional" flint-sync probe-conditional
run "probe-copy (CopyObject arm)" flint-sync probe-copy
FLINT_SYNC_COPY_WHOLE_MAX_MB=0 run "probe-copy (MPU + UploadPartCopy arm)" flint-sync probe-copy
say "=== 3. lean flow: publish from A, checkout into B ==="
export FLINT_SYNC_ROOT=$A
run "A checkout (empty prefix)" flint-sync checkout
mkdir -p $A/src $A/data
for i in $(seq 1 300); do head -c 8192 /dev/urandom > $A/src/f$i.bin; done
head -c 20971520 /dev/urandom > $A/data/twenty.bin
head -c 73400320 /dev/urandom > $A/data/seventy.bin
(cd $A && find . -type f -not -path './.flint-sync/*' | sort | xargs sha256sum > /mnt/nvme/A.sha)
run "A barrier (300 x 8 KiB + 20 MiB + 70 MiB; MPU above 64 MiB)" flint-sync barrier
aws --endpoint-url $S3G s3api list-objects-v2 --bucket lean --prefix ws1/ --query 'length(Contents)' --output text | sed 's/^/objects under ws1: /' | tee -a $LOG
export FLINT_SYNC_ROOT=$B
run "B checkout (fresh; ranged arm for the two big files)" flint-sync checkout
(cd $B && find . -type f -not -path './.flint-sync/*' | sort | xargs sha256sum > /mnt/nvme/B.sha); diff /mnt/nvme/A.sha /mnt/nvme/B.sha > /dev/null && say "B identical to A: $(wc -l < /mnt/nvme/B.sha) files" || { say "B DIFFERS from A"; diff /mnt/nvme/A.sha /mnt/nvme/B.sha | head -5 | tee -a $LOG; }
say "=== 4. a foreign overwrite, then a fresh reader (cited etag moved) ==="
echo "FOREIGN BYTES" > /tmp/foreign; aws --endpoint-url $S3G s3 cp /tmp/foreign s3://lean/ws1/files/src/f1.bin --quiet
export FLINT_SYNC_ROOT=$C
run "C checkout (f1.bin moved: 412->adopt if If-Match holds on GET; CRC refusal if it does not)" flint-sync checkout
say "C f1.bin content: $(cat $C/src/f1.bin 2>/dev/null | head -c 20 | od -c | head -1)"
say "=== 5. sync on B after A publishes more ==="
export FLINT_SYNC_ROOT=$A; echo "v2 content" > $A/src/new.txt; run "A barrier #2" flint-sync barrier
export FLINT_SYNC_ROOT=$B; run "B sync" flint-sync sync; say "B new.txt: $(cat $B/src/new.txt 2>/dev/null)"
say "=== 6. manifest as stored ==="
aws --endpoint-url $S3G s3api list-objects-v2 --bucket lean --prefix ws1/.flint --query 'Contents[].Key' --output text | tr '\t' '\n' | head -8 | tee -a $LOG
say "=== 7. small-file checkout rate on Ozone (2,000 x 8 KiB, fanout 128, drivers auto) ==="
D=/mnt/nvme/wsD; rm -rf $D; mkdir -p $D/many; export FLINT_SYNC_ROOT=$D FLINT_SYNC_PREFIX=ws2
run "D checkout (empty)" flint-sync checkout
for i in $(seq 1 2000); do head -c 8192 /dev/urandom > $D/many/f$i.bin; done
s=$(date +%s.%N); run "D barrier (2000 uploads)" flint-sync barrier; e=$(date +%s.%N); say "publish: $(python3 -c "print(f'{2000/($e-$s):.0f} files/s')")"
for r in 1 2 3; do E=/mnt/nvme/wsE$r; rm -rf $E; mkdir -p $E; export FLINT_SYNC_ROOT=$E; sync; echo 3 > /proc/sys/vm/drop_caches; s=$(date +%s.%N); flint-sync checkout > /mnt/nvme/co$r.out 2>&1; rc=$?; e=$(date +%s.%N); say "checkout run $r: exit=$rc $(python3 -c "print(f'{2000/($e-$s):.0f} files/s')") ($(find $E/many -type f | wc -l) files)"; done
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_EC2_METADATA_DISABLED
aws s3 cp $LOG s3://flint-ozone-probe/out/drill.log --quiet --region us-west-1 && echo "log uploaded"
