set -u
export HOME=/root
echo "host=$(hostname) nproc=$(nproc) mem_gb=$(free -g | awk '/Mem/{print $2}') disk=$(df -h / | awk 'NR==2{print $4}')"
dnf -y -q install docker python3 tar gzip binutils >/dev/null 2>&1
systemctl enable --now docker >/dev/null 2>&1
docker version --format 'docker {{.Server.Version}}'
mkdir -p /usr/local/lib/docker/cli-plugins
curl -sSL -o /usr/local/lib/docker/cli-plugins/docker-compose https://github.com/docker/compose/releases/latest/download/docker-compose-linux-x86_64 && chmod +x /usr/local/lib/docker/cli-plugins/docker-compose
docker compose version
dev=$(lsblk -dno NAME,TYPE,MOUNTPOINT | awk '$2=="disk" && $3=="" {print "/dev/"$1}' | grep -v nvme0n1 | head -1)
if [ -n "$dev" ] && ! mountpoint -q /mnt/nvme; then blkid "$dev" >/dev/null 2>&1 || mkfs.ext4 -q -F "$dev"; mkdir -p /mnt/nvme && mount "$dev" /mnt/nvme && chmod 1777 /mnt/nvme; fi
df -h /mnt/nvme | tail -1
mkdir -p /opt/oz && cd /opt/oz && curl -sS -o ozone.tgz https://dlcdn.apache.org/ozone/2.2.1/ozone-2.2.1.tar.gz && tar xzf ozone.tgz && rm ozone.tgz && echo "ozone tarball extracted: $(ls /opt/oz)"
echo "=== compose dirs ===" && ls /opt/oz/ozone-2.2.1/compose/
echo "=== compose/ozone ===" && ls /opt/oz/ozone-2.2.1/compose/ozone/
echo "=== .env ===" && cat /opt/oz/ozone-2.2.1/compose/ozone/.env 2>/dev/null
echo "=== docker-compose.yaml ===" && cat /opt/oz/ozone-2.2.1/compose/ozone/docker-compose.yaml
echo "=== docker-config ===" && cat /opt/oz/ozone-2.2.1/compose/ozone/docker-config 2>/dev/null | head -60
echo "=== pull ===" && (cd /opt/oz/ozone-2.2.1/compose/ozone && docker compose pull -q 2>&1 | tail -3; docker images --format '{{.Repository}}:{{.Tag}} {{.Size}}')
