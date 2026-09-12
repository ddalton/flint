set -u
export HOME=/root
cd /opt/oz/ozone-2.2.1/compose/ozone
docker compose up -d 2>&1 | tail -3
for i in $(seq 1 60); do curl -s -m 2 -o /dev/null -w '%{http_code}' http://localhost:9878/ 2>/dev/null | grep -q '^[0-9]' && break; sleep 3; done
echo "s3g answered after ~$((i*3))s: $(curl -s -m 3 -o /dev/null -w '%{http_code}' http://localhost:9878/)"
docker compose ps --format 'table {{.Service}}\t{{.Status}}' 2>&1 | head -12
export AWS_ACCESS_KEY_ID=flint AWS_SECRET_ACCESS_KEY=flintsecret AWS_DEFAULT_REGION=us-east-1
aws --version 2>&1 | head -1
S3G=http://localhost:9878
for i in $(seq 1 30); do aws --endpoint-url $S3G s3api create-bucket --bucket lean >/dev/null 2>&1 && break; sleep 5; done
echo "bucket create attempts: $i"
aws --endpoint-url $S3G s3api list-buckets --query 'Buckets[].Name' --output text
echo "=== s3gateway jar: rename / custom headers ===" && jar=$(ls /opt/oz/ozone-2.2.1/share/ozone/lib/ozone-s3gateway-*.jar | head -1); echo "$jar"
unzip -l "$jar" | awk '{print $4}' | grep -i -E "rename|Endpoint" | head -20
unzip -p "$jar" 'org/apache/hadoop/ozone/s3/*' 'org/apache/hadoop/ozone/s3/**/*' 2>/dev/null | strings | grep -i -E "x-ozone|ozone-rename|rename|x-amz-checksum|x-amz-sdk-checksum|if-match|if-none-match" | sort -u | head -40
