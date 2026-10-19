#!/usr/bin/env bash
# runsc on every worker of a trove cluster, and the `gvisor` RuntimeClass.
# The same steps as s3csi/e2e/local-access.sh `setup` with NODE_EXEC=nodesh
# (download + sha512 check ON the node, the runsc handler added to
# containerd's config, systrap platform), as a forge variant that walks
# every worker. Two defects it found on trove's AL2023 nodes (containerd
# 2.3.5, gVisor release-20261012.0), fixed here and in local-access.sh:
#
# 1. containerd 2.3 writes `version = 4`. Schemas 3 and 4 both name the CRI
#    runtime plugin io.containerd.cri.v1.runtime; local-access.sh tested for
#    "= 3" alone, so on a version-4 config the handler lands under
#    io.containerd.grpc.v1.cri, which containerd ignores ("no runtime for
#    runsc is configured").
# 2. Current gVisor releases run the sentry as a sidecar binary,
#    /usr/local/bin/gvisor-bin/gvisor_sentry. Extracting only runsc and the
#    shim gives "sidecar gvisor_sentry not usable: stat ... no such file".
#    The whole tarball is installed here.
#
# Idempotent per node: binaries if gvisor_sentry is missing, the handler if
# `containerd config dump` (what containerd actually loaded) lacks it.
set -euo pipefail
REPO=$(cd "$(dirname "$0")/../../.." && pwd)
GVISOR_URL=https://storage.googleapis.com/gvisor/releases/release/latest
ns(){ "$REPO/scripts/nodesh.sh" "$@" 2>/dev/null; }
for NODE in $(kubectl get nodes -l '!node-role.kubernetes.io/control-plane' -o jsonpath='{.items[*].metadata.name}'); do
  GARCH=$(ns "$NODE" 'uname -m' | tail -1)
  if ! ns "$NODE" 'test -x /usr/local/bin/gvisor-bin/gvisor_sentry && test -x /usr/local/bin/runsc'; then
    echo "$NODE: installing gVisor ($GARCH)"
    ns "$NODE" - <<EOS
set -e
mkdir -p /opt/gvisor && cd /opt/gvisor && rm -rf x && mkdir x
curl -fsSLO "$GVISOR_URL/$GARCH/gvisor.tar.bz2" && curl -fsSLO "$GVISOR_URL/$GARCH/gvisor.tar.bz2.sha512"
sha512sum -c gvisor.tar.bz2.sha512
tar xjf gvisor.tar.bz2 -C x 2>/dev/null || python3 -c "import tarfile; tarfile.open('gvisor.tar.bz2').extractall('x')"
install -m 0755 x/runsc x/containerd-shim-runsc-v1 /usr/local/bin/
if [ -d x/gvisor-bin ]; then rm -rf /usr/local/bin/gvisor-bin && cp -a x/gvisor-bin /usr/local/bin/gvisor-bin; fi
rm -rf x gvisor.tar.bz2
EOS
  fi
  if ! ns "$NODE" "containerd config dump | grep -q 'runtimes.runsc'"; then
    echo "$NODE: adding the runsc handler"
    ns "$NODE" - <<'EOS'
set -e
cfg=/etc/containerd/config.toml
v=$(sed -n 's/^version *= *\([0-9]*\).*/\1/p' "$cfg" | head -1)
if [ "${v:-2}" -ge 3 ]; then plug='io.containerd.cri.v1.runtime'; else plug='io.containerd.grpc.v1.cri'; fi
cat >> "$cfg" <<TOML

[plugins."$plug".containerd.runtimes.runsc]
  runtime_type = "io.containerd.runsc.v1"
  [plugins."$plug".containerd.runtimes.runsc.options]
    TypeUrl = "io.containerd.runsc.v1.options"
    ConfigPath = "/etc/containerd/runsc.toml"
TOML
printf '[runsc_config]\n  systemd-cgroup = "true"\n  platform = "systrap"\n' > /etc/containerd/runsc.toml
systemctl restart containerd
EOS
    sleep 15
  fi
  echo "$NODE: $(ns "$NODE" 'runsc --version | head -1; ls /usr/local/bin/gvisor-bin | tr "\n" " "')"
done
printf 'apiVersion: node.k8s.io/v1\nkind: RuntimeClass\nmetadata: { name: gvisor }\nhandler: runsc\n' | kubectl apply -f - >/dev/null
