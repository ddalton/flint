#!/bin/bash
# oci-ab: client-node setup (runs as root ON the node via SSM).
# Installs soci-snapshotter + nerdctl, wires containerd (config version 2
# AND 3 — trove AL2023 nodes run containerd 2.2.5/v3), configures plain-HTTP
# access to both in-cluster registries, and verifies.
# Env: REG_FLINT, REG_S3 (ip:port each).
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

# ── containerd wiring: write the drop-in where THIS containerd reads ─────
# The old version wrote /etc/containerd/oci-ab.toml and added an `imports`
# line only when none existed. trove's AL2023 nodes run containerd 2.2.5
# with `version = 3` and already carry
# `imports = ['/etc/containerd/conf.d/*.toml']` — where conf.d DOES NOT
# EXIST. So the grep found an imports line, the sed was skipped, and the
# drop-in was written to a path nothing ever read. `proxy_plugins.soci` was
# never loaded and every soci pull died "snapshotter not loaded: soci:
# invalid argument" while the daemon itself was healthy the whole time.
# (runby, 2026-09-01 — presented as G-PULL:pull-rc=1 on both lazy arms.)
#
# So: find the directory an existing import glob already points at and write
# there; only fall back to inventing an import when there is none.
IMPDIR=$(sed -n 's/.*imports *= *\[[^]]*[\"'"'"']\([^\"'"'"']*\)\/\*\.toml.*/\1/p' \
         /etc/containerd/config.toml 2>/dev/null | head -1)
if [ -n "$IMPDIR" ]; then
    mkdir -p "$IMPDIR"
    IMP="$IMPDIR/oci-ab.toml"
    echo "containerd imports $IMPDIR/*.toml — writing the drop-in there"
else
    IMP=/etc/containerd/oci-ab.toml
    grep -q '^imports' /etc/containerd/config.toml \
        || sed -i '1i imports = ["/etc/containerd/oci-ab.toml"]' /etc/containerd/config.toml
fi

# `[proxy_plugins]` is the only stanza the measurement needs, and its key is
# unchanged between config version 2 and 3. The CRI options below are 1.x/v2
# plugin paths that containerd 2.x does not read, and they matter only for
# CRI-driven pulls — the arms drive nerdctl directly — so they are written
# only where they are actually understood.
cat > "$IMP" <<'EOF'
[proxy_plugins.soci]
  type = "snapshot"
  address = "/run/soci-snapshotter-grpc/soci-snapshotter-grpc.sock"
EOF
if grep -qE '^version *= *2' /etc/containerd/config.toml 2>/dev/null; then
    cat >> "$IMP" <<'EOF'
[plugins."io.containerd.grpc.v1.cri".containerd]
  disable_snapshot_annotations = false
[plugins."io.containerd.grpc.v1.cri".registry]
  config_path = "/etc/containerd/certs.d"
EOF
fi

systemctl daemon-reload
systemctl enable --now soci-snapshotter
systemctl restart containerd
sleep 3

# ── verify — read state, not exit codes ──────────────────────────────
ctr plugin ls | grep -q soci || { echo "FAIL: soci plugin not registered"; containerd config dump | grep -A2 proxy_plugins; exit 1; }
# v2-only assertion: on containerd 2.x this key does not exist at all, and
# demanding it would fail a node that is correctly configured.
if grep -qE '^version *= *2' /etc/containerd/config.toml 2>/dev/null; then
  containerd config dump | grep -q 'disable_snapshot_annotations = false' || { echo "FAIL: snapshot annotations still disabled"; exit 1; }
fi
systemctl is-active kubelet >/dev/null || { echo "FAIL: kubelet unhappy after containerd restart"; exit 1; }
# A4 prerequisite: EROFS module (AL2023 6.1 may lack it — the mainline-swap
# recipe is the known fix; report, do not fail the whole setup)
modprobe erofs 2>/dev/null && echo "EROFS: ok" || echo "EROFS: MISSING (A4 needs the kernel swap)"
echo "node-soci-setup: OK"
