set -u
export AWS_ACCESS_KEY_ID=flint AWS_SECRET_ACCESS_KEY=flintsecret AWS_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true
AWS="docker run --rm -i --network host -v $HOME/oz:/data -e AWS_ACCESS_KEY_ID=flint -e AWS_SECRET_ACCESS_KEY=flintsecret -e AWS_DEFAULT_REGION=us-east-1 amazon/aws-cli --endpoint-url http://127.0.0.1:9878"
MS3=~/oz/mp/bin/mount-s3; S3G=http://127.0.0.1:9878
step() { echo; echo "== $1 $(date -u +%T)"; }
step "seed with the AWS CLI: 10 small + 300 tiny (a paginated listing) + 128 MiB"
mkdir -p ~/oz/seed/imagenet ~/oz/seed/many; for i in 00 01 02 03 04 05 06 07 08 09; do echo "seeded-object-$i" > ~/oz/seed/imagenet/shard-$i.txt; done
for i in $(seq -w 1 300); do echo "k$i" > ~/oz/seed/many/k$i; done
head -c 134217728 /dev/urandom > ~/oz/seed/m1
$AWS s3 cp /data/seed/imagenet s3://ptoz/datasets/imagenet/ --recursive --only-show-errors && echo "imagenet seeded"
$AWS s3 cp /data/seed/many s3://ptoz/many/ --recursive --only-show-errors && echo "many seeded"
$AWS s3 cp /data/seed/m1 s3://ptoz/mid/m1 --only-show-errors && echo "m1 seeded"
$AWS s3api list-objects-v2 --bucket ptoz --prefix datasets/imagenet/ --query "KeyCount" --output text
step "A: read-only mount — list, read, stat, a 300-key directory"
rm -rf ~/oz/mnt-ro/* ~/oz/logs/*
$MS3 ptoz ~/oz/mnt-ro --endpoint-url $S3G --force-path-style --read-only --prefix datasets/ --log-directory ~/oz/logs 2>&1 | grep -v WARN | head -3
sleep 1; mountpoint -q ~/oz/mnt-ro && echo mounted
echo "ls imagenet: $(ls ~/oz/mnt-ro/imagenet | wc -l) entries"; echo "cat: $(cat ~/oz/mnt-ro/imagenet/shard-03.txt)"; stat -c "%s bytes mode %a" ~/oz/mnt-ro/imagenet/shard-03.txt
fusermount -u ~/oz/mnt-ro 2>/dev/null; sleep 1
$MS3 ptoz ~/oz/mnt-ro --endpoint-url $S3G --force-path-style --read-only --prefix many/ --log-directory ~/oz/logs 2>&1 | grep -v WARN | head -3
sleep 1; echo "ls many: $(ls ~/oz/mnt-ro | wc -l) entries (300 seeded)"; echo "k150: $(cat ~/oz/mnt-ro/k150)"
fusermount -u ~/oz/mnt-ro 2>/dev/null; sleep 1
step "D: read-only with --cache: cold then warm read of the 128 MiB object"
rm -rf ~/oz/cache; mkdir -p ~/oz/cache
$MS3 ptoz ~/oz/mnt-ro --endpoint-url $S3G --force-path-style --read-only --prefix mid/ --cache ~/oz/cache --max-cache-size 768 --log-directory ~/oz/logs 2>&1 | grep -v WARN | head -3
sleep 1; ls -la ~/oz/mnt-ro | tail -1
for r in cold warm; do t0=$(date +%s%N); cat ~/oz/mnt-ro/m1 | md5sum | cut -c1-8; t1=$(date +%s%N); echo "$r read: $(( (t1-t0)/1000000 )) ms"; done
echo "cache on disk: $(du -sh ~/oz/cache | cut -f1)"; md5sum ~/oz/seed/m1 | cut -c1-8
fusermount -u ~/oz/mnt-ro 2>/dev/null
step "E: write flags Ozone needs? re-check f72 objects and their ETags"
$AWS s3api head-object --bucket ptoz --key f72/late.bin --query "[ContentLength,ETag,ChecksumCRC32C]" --output text
$AWS s3api head-object --bucket ptoz --key f72c/late.bin --query "[ContentLength,ETag]" --output text
echo "--- mount-s3 log lines mentioning error/checksum:"; grep -h -i -E "error|checksum" ~/oz/logs/*.log 2>/dev/null | grep -v "IMDS" | tail -5
echo; echo "== DONE $(date -u +%T)"
