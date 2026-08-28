#!/bin/bash
# Install pynfs so it can speak RPCSEC_GSS, on a host that already has
# the KDC from setup-kdc.sh.
#
# The repo has shipped pynfs since v1.23.0 and has NEVER run it over
# Kerberos: tests/lima/nfs-client.yaml installs the bindings with
# `pip install gssapi || true` under the comment "sec=sys is enough for
# us". The first GSS run found a shipped bug (RPC NULL over RPCSEC_GSS
# answered GARBAGE_ARGS), so it was not enough for us.
set -euo pipefail

# gssapi needs Kerberos headers and a compiler; without these the pip
# install fails, and under `|| true` it fails SILENTLY.
sudo DEBIAN_FRONTEND=noninteractive apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
  build-essential python3-dev python3-venv libkrb5-dev krb5-user pkg-config

if [ ! -d /opt/pynfs ]; then
  sudo git clone --depth=1 https://git.linux-nfs.org/projects/bfields/pynfs.git /opt/pynfs \
    || sudo git clone --depth=1 https://github.com/kofemann/pynfs.git /opt/pynfs
fi
sudo git config --global --add safe.directory /opt/pynfs || true

sudo python3 -m venv /opt/pynfs/.venv
sudo /opt/pynfs/.venv/bin/pip install -q --upgrade pip setuptools wheel
sudo /opt/pynfs/.venv/bin/pip install -q ply gssapi

# TRAP: xdrlib was REMOVED from the stdlib in Python 3.13 (PEP 594), and
# pynfs imports it directly — so on a modern distro `import rpc.security`
# dies before any flavor question is even asked. xdrlib3 is the
# maintained backport; pynfs wants the old module name.
sudo /opt/pynfs/.venv/bin/pip install -q xdrlib3
SP=$(sudo /opt/pynfs/.venv/bin/python -c 'import site; print(site.getsitepackages()[0])')
sudo tee "$SP/xdrlib.py" >/dev/null <<'SHIM'
"""Shim: xdrlib was removed from the stdlib in Python 3.13 (PEP 594).
pynfs imports it directly; xdrlib3 is the maintained backport."""
from xdrlib3 import *          # noqa: F401,F403
from xdrlib3 import Packer, Unpacker, Error, ConversionError  # noqa: F401
SHIM

# TRAP: there is NO nfs4.1/Makefile, so the repo Makefile's
# `cd nfs4.1 && make` was always a no-op hidden by `|| true`. The build
# is setup.py at the ROOT, which chdirs into xdr/rpc/nfs4.1/nfs4.0 and
# shells out with os.system -- so sub-build failures are INVISIBLE. Run a
# subdir build directly if you need to see an error.
cd /opt/pynfs
sudo /opt/pynfs/.venv/bin/python setup.py build >/dev/null

# The gate testserver.py itself checks: AuthGss must be in the table.
PYTHONPATH=/opt/pynfs /opt/pynfs/.venv/bin/python -c "
import rpc.security as s
assert 6 in s.supported, 'RPCSEC_GSS missing: %r' % (s.supported,)
print('pynfs can speak RPCSEC_GSS:', s.supported[6].__name__)
"
