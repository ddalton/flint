cat > /mnt/nvme/rig/drill.sh <<'EOS'
#!/bin/bash
export BUCKET=flint-lean-door-20260924 PREFIX=door-20260924 ROOT=/mnt/nvme/drill BIN=/mnt/nvme/rig/flint-sync BIN_0912=/mnt/nvme/rig/flint-sync-f7d44444 WORKLOADS=small
cd /mnt/nvme/rig
./doors.sh seed > /mnt/nvme/seed.log 2>&1; echo "SEED rc=$?" >> /mnt/nvme/seed.log
grep -q "SEED rc=0" /mnt/nvme/seed.log || { aws s3 cp /mnt/nvme/seed.log s3://$BUCKET/_rig/logs/ --quiet; echo DRILLDONE-SEEDFAIL > /mnt/nvme/DONE; exit 1; }
./doors.sh run 3 > /mnt/nvme/run.log 2>&1; echo "RUN rc=$?" >> /mnt/nvme/run.log
aws s3 cp /mnt/nvme/seed.log s3://$BUCKET/_rig/logs/ --quiet; aws s3 cp /mnt/nvme/run.log s3://$BUCKET/_rig/logs/ --quiet
aws s3 cp --recursive /mnt/nvme/drill/results s3://$BUCKET/_rig/results/ --quiet
echo DRILLDONE > /mnt/nvme/DONE
EOS
chmod +x /mnt/nvme/rig/drill.sh; rm -f /mnt/nvme/DONE
setsid nohup /mnt/nvme/rig/drill.sh >/dev/null 2>&1 < /dev/null &
sleep 2; pgrep -fa drill.sh
