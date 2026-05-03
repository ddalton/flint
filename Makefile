# Flint top-level Makefile.
#
# Most useful targets for NFS protocol-level testing:
#
#   make lima-up                 — start the Linux test VM (one-time, ~3 min)
#   make lima-down               — stop and delete the VM
#   make nfs-server              — build and run flint-nfs-server on the host
#   make test-nfs-protocol       — run pynfs (NFSv4.1 conformance) from the VM
#   make test-nfs-mount          — sanity: mount export from the VM and write a file
#   make test-nfs-frag           — exercise fragmented-RPC code path (T1)
#
# Most NFS protocol tests do NOT need Kubernetes. K8s/CSI tests live in
# tests/system/ and are orchestrated separately.

SHELL          := /bin/bash
.SHELLFLAGS    := -eu -o pipefail -c

LIMA_VM        := flint-nfs-client
LIMA_CFG       := tests/lima/nfs-client.yaml

# We use a non-privileged port so the server can run without sudo on macOS.
NFS_PORT       ?= 20490
NFS_BIND       ?= 0.0.0.0
NFS_EXPORT     ?= /tmp/flint-nfs-export
NFS_VOLUME_ID  ?= test-vol

CARGO          := cargo
CARGO_DIR      := spdk-csi-driver
SERVER_BIN     := $(CARGO_DIR)/target/release/flint-nfs-server

# Host address as seen from inside Lima. host.lima.internal is the gateway.
LIMA_HOST_ADDR ?= host.lima.internal

.PHONY: help
help:
	@grep -E '^[a-zA-Z_-]+:.*?##' $(MAKEFILE_LIST) | \
	  awk 'BEGIN {FS = ":.*?##"}; {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}'

# ───────────────────────────── Lima VM ───────────────────────────────────────

.PHONY: lima-check
lima-check:
	@command -v limactl >/dev/null 2>&1 || { \
	  echo "limactl not found. Install with: brew install lima"; exit 1; }

.PHONY: lima-up
lima-up: lima-check ## Start the Ubuntu test VM with pynfs preinstalled
	@if limactl list --quiet | grep -qx '$(LIMA_VM)'; then \
	  echo "VM $(LIMA_VM) already exists. Run: make lima-shell"; \
	else \
	  limactl start --name=$(LIMA_VM) --tty=false $(LIMA_CFG); \
	fi

.PHONY: lima-down
lima-down: lima-check ## Stop and delete the test VM
	-limactl stop -f $(LIMA_VM)
	-limactl delete $(LIMA_VM)

.PHONY: lima-shell
lima-shell: lima-check ## Open an interactive shell in the test VM
	limactl shell $(LIMA_VM)

# ───────────────────────────── NFS server ────────────────────────────────────

$(NFS_EXPORT):
	mkdir -p $@

.PHONY: build-nfs-server
build-nfs-server: ## Build flint-nfs-server (release)
	cd $(CARGO_DIR) && $(CARGO) build --release --bin flint-nfs-server

.PHONY: nfs-server
nfs-server: build-nfs-server $(NFS_EXPORT) ## Run flint-nfs-server in foreground
	@echo "Serving $(NFS_EXPORT) on $(NFS_BIND):$(NFS_PORT)"
	@echo "From the Lima VM, mount with:"
	@echo "  sudo mount -t nfs4 -o minorversion=1,proto=tcp,port=$(NFS_PORT) \\"
	@echo "       $(LIMA_HOST_ADDR):/ /mnt/flint"
	$(SERVER_BIN) \
	  --bind-addr $(NFS_BIND) \
	  --port $(NFS_PORT) \
	  --export-path $(NFS_EXPORT) \
	  --volume-id $(NFS_VOLUME_ID) \
	  --verbose

.PHONY: nfs-server-bg
nfs-server-bg: build-nfs-server $(NFS_EXPORT) ## Run flint-nfs-server in background; PID in /tmp/flint-nfs.pid
	@if [ -f /tmp/flint-nfs.pid ] && kill -0 $$(cat /tmp/flint-nfs.pid) 2>/dev/null; then \
	  echo "Server already running, pid=$$(cat /tmp/flint-nfs.pid)"; \
	else \
	  nohup $(SERVER_BIN) \
	    --bind-addr $(NFS_BIND) --port $(NFS_PORT) \
	    --export-path $(NFS_EXPORT) --volume-id $(NFS_VOLUME_ID) \
	    >/tmp/flint-nfs.log 2>&1 & echo $$! > /tmp/flint-nfs.pid; \
	  sleep 1; \
	  echo "Started, pid=$$(cat /tmp/flint-nfs.pid), log=/tmp/flint-nfs.log"; \
	fi

.PHONY: nfs-server-stop
nfs-server-stop: ## Stop the background flint-nfs-server
	@if [ -f /tmp/flint-nfs.pid ]; then \
	  kill $$(cat /tmp/flint-nfs.pid) 2>/dev/null || true; \
	  rm -f /tmp/flint-nfs.pid; \
	  echo "Stopped."; \
	fi

# ───────────────────────────── Tests ─────────────────────────────────────────

.PHONY: test-nfs-mount
test-nfs-mount: ## Sanity: mount and write a file from the VM (requires nfs-server-bg)
	limactl shell $(LIMA_VM) -- sudo bash -lc '\
	  set -eux; \
	  mkdir -p /mnt/flint; \
	  mountpoint -q /mnt/flint && umount /mnt/flint || true; \
	  mount -t nfs4 -o minorversion=1,proto=tcp,port=$(NFS_PORT) \
	    $(LIMA_HOST_ADDR):/ /mnt/flint; \
	  echo hello > /mnt/flint/sanity.txt; \
	  cat /mnt/flint/sanity.txt; \
	  ls -la /mnt/flint; \
	  umount /mnt/flint'

.PHONY: test-nfs-protocol
test-nfs-protocol: ## Run full pynfs NFSv4.1 conformance suite (--maketree)
	# `--maketree` builds the test directory ($(NFS_EXPORT)/tmp/tree) of
	# regular file, dir, symlink, socket/fifo/block/char stand-ins that
	# the suite expects. Without it most tests SKIP. Pre-create /tmp on
	# the export so the build step has a writable parent.
	mkdir -p $(NFS_EXPORT)/tmp
	chmod 0777 $(NFS_EXPORT)/tmp
	limactl shell $(LIMA_VM) -- bash -lc '\
	  cd /opt/pynfs/nfs4.1 && \
	  python3 ./testserver.py $(LIMA_HOST_ADDR):$(NFS_PORT)/tmp \
	    --maketree --nocleanup --json=/tmp/pynfs.json all || true'
	limactl cp $(LIMA_VM):/tmp/pynfs.json /tmp/flint-pynfs-results.json
	@echo "Results: /tmp/flint-pynfs-results.json"

.PHONY: test-nfs-frag
test-nfs-frag: ## Force fragmented WRITE (T1) — large file via dd over NFS
	limactl shell $(LIMA_VM) -- sudo bash -lc '\
	  set -eux; \
	  mkdir -p /mnt/flint; \
	  mountpoint -q /mnt/flint && umount /mnt/flint || true; \
	  mount -t nfs4 -o minorversion=1,proto=tcp,port=$(NFS_PORT),wsize=1048576,rsize=1048576 \
	    $(LIMA_HOST_ADDR):/ /mnt/flint; \
	  dd if=/dev/urandom of=/mnt/flint/big.bin bs=1M count=8 oflag=direct; \
	  dd if=/mnt/flint/big.bin of=/dev/null bs=1M; \
	  rm -f /mnt/flint/big.bin; \
	  umount /mnt/flint'

.PHONY: test-nfs-all
test-nfs-all: nfs-server-bg ## Run mount + protocol + frag tests, then stop server
	-$(MAKE) test-nfs-mount
	-$(MAKE) test-nfs-protocol
	-$(MAKE) test-nfs-frag
	$(MAKE) nfs-server-stop

# ───────────────────────────── pNFS tests ────────────────────────────────────
#
# The pNFS suite spins up flint-pnfs-mds + 2× flint-pnfs-ds on the host, each
# with config files under tests/lima/pnfs/. The test scripts manage their own
# server lifecycle (start, run, stop), so these targets don't share the
# nfs-server-bg machinery above.

.PHONY: build-pnfs
build-pnfs: ## Build flint-pnfs-mds, flint-pnfs-ds, pnfs-csi-cli (release)
	cd $(CARGO_DIR) && $(CARGO) build --release \
	  --bin flint-pnfs-mds --bin flint-pnfs-ds --bin pnfs-csi-cli

.PHONY: test-pnfs-smoke
test-pnfs-smoke: build-pnfs ## End-to-end pNFS data-path smoke test (mount + write + checksum)
	tests/lima/pnfs/smoke.sh

.PHONY: test-pnfs-pynfs
test-pnfs-pynfs: build-pnfs ## Run pynfs `pnfs` conformance subset against the MDS
	tests/lima/pnfs/pynfs.sh

.PHONY: test-pnfs-csi
test-pnfs-csi: build-pnfs ## End-to-end pNFS CSI integration test (gRPC create → mount → I/O → delete)
	tests/lima/pnfs/csi-e2e.sh

.PHONY: test-pnfs-recall
test-pnfs-recall: build-pnfs ## DS-death CB_LAYOUTRECALL e2e (kill DS1, assert MDS recall fires)
	tests/lima/pnfs/recall.sh

.PHONY: test-pnfs-restart
test-pnfs-restart: build-pnfs ## MDS restart survival e2e (Phase B: kill MDS, restart over same state.db, mount keeps working)
	tests/lima/pnfs/restart.sh

.PHONY: test-pnfs-nconnect
test-pnfs-nconnect: build-pnfs ## Single-host nconnect sweep — exposes per-TCP-serial RPC ceiling (loopback only; cross-host is a separate bench)
	tests/lima/pnfs/nconnect.sh

.PHONY: test-pnfs-all
test-pnfs-all: ## Run smoke + pynfs + csi-e2e + recall + restart tests in sequence
	$(MAKE) test-pnfs-smoke
	$(MAKE) test-pnfs-pynfs
	$(MAKE) test-pnfs-csi
	$(MAKE) test-pnfs-recall
	$(MAKE) test-pnfs-restart
