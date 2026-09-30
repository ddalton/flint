set -u
export PATH=$HOME/bin:$PATH TMPDIR=$HOME/tmp
export AWS_ACCESS_KEY_ID=flint AWS_SECRET_ACCESS_KEY=flintsecret AWS_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true
S3G=http://127.0.0.1:9878
MC="docker run --rm -i --network host -e MC_HOST_oz=http://flint:flintsecret@127.0.0.1:9878 cgr.dev/chainguard/minio-client:latest-dev"
step() { echo; echo "== $1 $(date -u +%T)"; }
step "packages"
sudo -n apt-get install -y -q fuse libfuse2t64 >/dev/null 2>&1 || sudo -n apt-get install -y -q fuse libfuse2 >/dev/null 2>&1; ldconfig -p | grep -c "libfuse.so.2"; command -v fusermount
step "ozone 2.2.1"
mkdir -p ~/oz && cd ~/oz
[ -d ozone-2.2.1 ] || { curl -sS -o ozone.tgz https://dlcdn.apache.org/ozone/2.2.1/ozone-2.2.1.tar.gz && tar xzf ozone.tgz && rm ozone.tgz; }
cd ~/oz/ozone-2.2.1/compose/ozone && docker compose up -d 2>&1 | head -3
for i in $(seq 1 90); do curl -s -m 2 -o /dev/null -w "%{http_code}" $S3G/ 2>/dev/null | grep -q "^[0-9]" && break; sleep 3; done
echo "s3g answered after ~$((i*3))s: $(curl -s -m 3 -o /dev/null -w "%{http_code}" $S3G/)"
docker compose ps --format "table {{.Service}}\t{{.Status}}" | head -8
step "bucket + seed"
for i in $(seq 1 30); do $MC mb oz/ptoz >/dev/null 2>&1 && break; sleep 5; done; echo "mb attempts: $i"
for i in 00 01 02 03 04 05 06 07 08 09; do echo "seeded-object-$i" | $MC pipe oz/ptoz/datasets/imagenet/shard-$i.txt >/dev/null 2>&1; done
$MC ls oz/ptoz/datasets/imagenet/ | head -3
step "mount-s3 1.24.0"
cd ~/oz && [ -x mp/bin/mount-s3 ] || { curl -sS -o ms3.tgz https://s3.amazonaws.com/mountpoint-s3-release/1.24.0/x86_64/mount-s3-1.24.0-x86_64.tar.gz && echo "a99bea20510eaabaf9d7cbfe95ab221a11005cacaa77cfc311a2912bbbdbfd72  ms3.tgz" | sha256sum -c - && mkdir -p mp && tar xzf ms3.tgz -C mp && rm ms3.tgz; }
MS3=~/oz/mp/bin/mount-s3; $MS3 --version
mkdir -p ~/oz/mnt-ro ~/oz/mnt-rw ~/oz/logs
step "A: read-only mount"
rm -rf ~/oz/mnt-ro/* ~/oz/logs/*; $MS3 ptoz ~/oz/mnt-ro --endpoint-url $S3G --force-path-style --read-only --prefix datasets/imagenet/ --log-directory ~/oz/logs 2>&1 | head -3
sleep 2; ls ~/oz/mnt-ro | head -3; echo "cat: $(cat ~/oz/mnt-ro/shard-03.txt)"; stat -c "%s bytes %U" ~/oz/mnt-ro/shard-03.txt
fusermount -u ~/oz/mnt-ro && echo "unmounted"
step "B: read-write mount, default checksums (CRC32C trailing), a 48 MiB multipart write + a small single PUT"
rm -rf ~/oz/mnt-rw/* ~/oz/logs/*; $MS3 ptoz ~/oz/mnt-rw --endpoint-url $S3G --force-path-style --allow-delete --allow-overwrite --prefix f72/ --log-directory ~/oz/logs 2>&1 | head -3
sleep 2; mountpoint -q ~/oz/mnt-ro || mountpoint -q ~/oz/mnt-rw || { echo MOUNT-FAILED; tail -3 ~/oz/logs/*.log; }
echo hi > ~/oz/mnt-rw/small.txt; echo "small write rc=$?"
head -c 50331648 /dev/urandom > ~/oz/mnt-rw/late.bin; echo "48 MiB write rc=$?"
sleep 2; $MC stat oz/ptoz/f72/late.bin 2>&1 | grep -E "Size|Name" | head -2; $MC stat oz/ptoz/f72/small.txt 2>&1 | grep -E "Size" | head -1
echo "incomplete: $($MC ls --incomplete oz/ptoz/f72/ 2>&1 | wc -l)"
rm -f ~/oz/mnt-rw/small.txt; echo "delete rc=$?"
fusermount -u ~/oz/mnt-rw && echo "unmounted"
echo "--- mount-s3 log (errors/warnings):"; grep -h -i -E "error|warn|checksum" ~/oz/logs/*.log 2>/dev/null | tail -8
step "C: read-write mount with --upload-checksums off (control if B failed; a comparison if it did not)"
rm -f ~/oz/logs/*.log
rm -rf ~/oz/mnt-rw/* ~/oz/logs/*; $MS3 ptoz ~/oz/mnt-rw --endpoint-url $S3G --force-path-style --allow-delete --allow-overwrite --prefix f72c/ --upload-checksums off --log-directory ~/oz/logs 2>&1 | head -3
sleep 2; mountpoint -q ~/oz/mnt-ro || mountpoint -q ~/oz/mnt-rw || { echo MOUNT-FAILED; tail -3 ~/oz/logs/*.log; }
head -c 50331648 /dev/urandom > ~/oz/mnt-rw/late.bin; echo "48 MiB write rc=$?"
sleep 2; $MC stat oz/ptoz/f72c/late.bin 2>&1 | grep -E "Size" | head -1; echo "incomplete: $($MC ls --incomplete oz/ptoz/f72c/ 2>&1 | wc -l)"
fusermount -u ~/oz/mnt-rw && echo "unmounted"
grep -h -i -E "error|warn" ~/oz/logs/*.log 2>/dev/null | tail -5
step "D: read-only with the block cache (--cache) and a cold/warm read of a 128 MiB object"
head -c 134217728 /dev/urandom | $MC pipe oz/ptoz/mid/m1 >/dev/null 2>&1
mkdir -p ~/oz/cache && rm -rf ~/oz/cache/*
rm -rf ~/oz/mnt-ro/* ~/oz/logs/*; $MS3 ptoz ~/oz/mnt-ro --endpoint-url $S3G --force-path-style --read-only --prefix mid/ --cache ~/oz/cache --max-cache-size 768 --log-directory ~/oz/logs 2>&1 | head -3
sleep 2; mountpoint -q ~/oz/mnt-ro || mountpoint -q ~/oz/mnt-rw || { echo MOUNT-FAILED; tail -3 ~/oz/logs/*.log; }
for r in cold warm; do t0=$(date +%s%N); cat ~/oz/mnt-ro/m1 > /dev/null; t1=$(date +%s%N); echo "$r read: $(( (t1-t0)/1000000 )) ms"; done
du -sh ~/oz/cache | cut -f1
fusermount -u ~/oz/mnt-ro && echo "unmounted"
echo; echo "== DONE $(date -u +%T)"
