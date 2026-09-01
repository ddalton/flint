#!/bin/bash
# oci-ab: client-node setup (runs as root ON the node via SSM).
# Installs soci-snapshotter + nerdctl, wires containerd (1.7 classic
# annotations path), configures plain-HTTP access to both in-cluster
# registries, and verifies. Env: REG_FLINT, REG_S3 (ip:port each).
set -eux

SOCI_V=${SOCI_V:-0.11.1}
NERDCTL_V=${NERDCTL_V:-1.7.7}
ARCH=amd64

# ── binaries ─────────────────────────────────────────────────────────
cd /tmp
curl -fsSLo soci.tgz "https://github.com/awslabs/soci-snapshotter/releases/download/v${SOCI_V}/soci-snapshotter-${SOCI_V}-linux-${ARCH}.tar.gz"
tar -xzf soci.tgz -C /usr/local/bin soci soci-snapshotter-grpc
curl -fsSLo nerdctl.tgz "https://github.com/containerd/nerdctl/releases/download/v${NERDCTL_V}/nerdctl-${NERDCTL_V}-linux-${ARCH}.tar.gz"
tar -xzf nerdctl.tgz -C /usr/local/bin nerdctl

# ── plain-HTTP registry hosts (ctr/nerdctl/soci all read certs.d) ────
for reg in "$REG_FLINT" "$REG_S3"; do
  mkdir -p "/etc/containerd/certs.d/$reg"
  cat > "/etc/containerd/certs.d/$reg/hosts.toml" <<EOF
server = "http://$reg"
[host."http://$reg"]
  capabilities = ["pull", "resolve", "push"]
  skip_verify = true
EOF
done

# ── soci daemon config + unit ────────────────────────────────────────
mkdir -p /etc/soci-snapshotter-grpc
cat > /etc/soci-snapshotter-grpc/config.toml <<'EOF'
[cri_keychain]
enable_keychain = false
[registry]
config_path = "/etc/containerd/certs.d"
EOF
cat > /etc/systemd/system/soci-snapshotter.service <<'EOF'
[Unit]
Description=soci snapshotter
After=network.target
[Service]
ExecStart=/usr/local/bin/soci-snapshotter-grpc --config /etc/soci-snapshotter-grpc/config.toml
Restart=always
[Install]
WantedBy=multi-user.target
EOF

# ── containerd wiring via imports (verify after; fall back to sed) ───
IMP=/etc/containerd/oci-ab.toml
cat > "$IMP" <<'EOF'
[proxy_plugins.soci]
  type = "snapshot"
  address = "/run/soci-snapshotter-grpc/soci-snapshotter-grpc.sock"
[plugins."io.containerd.grpc.v1.cri".containerd]
  disable_snapshot_annotations = false
[plugins."io.containerd.grpc.v1.cri".registry]
  config_path = "/etc/containerd/certs.d"
EOF
grep -q '^imports' /etc/containerd/config.toml || sed -i '1i imports = ["/etc/containerd/oci-ab.toml"]' /etc/containerd/config.toml

systemctl daemon-reload
systemctl enable --now soci-snapshotter
systemctl restart containerd
sleep 3

# ── verify — read state, not exit codes ──────────────────────────────
ctr plugin ls | grep -q soci || { echo "FAIL: soci plugin not registered"; containerd config dump | grep -A2 proxy_plugins; exit 1; }
containerd config dump | grep -q 'disable_snapshot_annotations = false' || { echo "FAIL: snapshot annotations still disabled"; exit 1; }
systemctl is-active kubelet >/dev/null || { echo "FAIL: kubelet unhappy after containerd restart"; exit 1; }
# A4 prerequisite: EROFS module (AL2023 6.1 may lack it — the mainline-swap
# recipe is the known fix; report, do not fail the whole setup)
modprobe erofs 2>/dev/null && echo "EROFS: ok" || echo "EROFS: MISSING (A4 needs the kernel swap)"
echo "node-soci-setup: OK"
