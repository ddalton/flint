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
	# pynfs assumes a freshly started server: grace-period tests (RECC3)
	# expect NFS4ERR_GRACE, which is only correct inside the server's
	# post-boot grace window. Restart with a clean state DB so every run
	# sees the same server age.
	$(MAKE) nfs-server-stop
	# Wiping .flint-nfs resets the state DB — but it ALSO deletes the F30
	# identity marker, and without that the server refuses to serve
	# ("empty/foreign dir, not volume ..."), so this target could not run
	# at all. Re-stamp the marker after the wipe. Found 2026-08-01 while
	# gating v1.23.0; the breakage predates v1.22.0.
	rm -rf $(NFS_EXPORT)/.flint-nfs
	# Leftovers from an aborted previous run (dangling symlinks, per-test
	# dirs) make pynfs's own clean_dir abort the whole suite before any
	# test runs; start from an empty test area. --maketree rebuilds it.
	rm -rf $(NFS_EXPORT)/tmp
	# pynfs's grace tests (RECC3 et al.) assume the server is in grace
	# whenever they run; the suite outlasts the RFC-default 90s window.
	mkdir -p $(NFS_EXPORT)/.flint-nfs
	printf '%s' '$(NFS_VOLUME_ID)' > $(NFS_EXPORT)/.flint-nfs/volume-id
	FLINT_NFS_GRACE_SECS=900 $(MAKE) nfs-server-bg
	# `--maketree` builds the test directory ($(NFS_EXPORT)/tmp/tree) of
	# regular file, dir, symlink, socket/fifo/block/char stand-ins that
	# the suite expects. Without it most tests SKIP. Pre-create /tmp on
	# the export so the build step has a writable parent.
	mkdir -p $(NFS_EXPORT)/tmp
	chmod 0777 $(NFS_EXPORT)/tmp
	-limactl shell $(LIMA_VM) -- sudo rm -f /tmp/pynfs.json
	rm -f /tmp/flint-pynfs-results.json
	limactl shell $(LIMA_VM) -- bash -lc '\
	  cd /opt/pynfs/nfs4.1 && \
	  python3 ./testserver.py $(LIMA_HOST_ADDR):$(NFS_PORT)/tmp \
	    --maketree --nocleanup --json=/tmp/pynfs.json all || true'
	limactl cp $(LIMA_VM):/tmp/pynfs.json /tmp/flint-pynfs-results.json
	@echo "Results: /tmp/flint-pynfs-results.json"

# ─────────────────── NFS server INSIDE the Lima VM ───────────────────────────
#
# The targets above run flint-nfs-server on the macOS HOST. That is fine for
# protocol conformance, but it CANNOT test the space-management operations:
# the real bodies of SEEK/ALLOCATE/DEALLOCATE are #[cfg(target_os = "linux")]
# and every other target returns NOTSUPP unconditionally. A run against a
# darwin-hosted server therefore measures the PLATFORM, not the code — it
# fails ALLOC1-3 no matter what the code does.
#
# These targets cross-compile the same musl binary we ship and run it inside
# the VM, so client AND server are Linux.
LIMA_ARCH      := $(shell test "$$(uname -m)" = arm64 && echo aarch64 || echo x86_64)
VM_SERVER_BIN  := $(CARGO_DIR)/target/$(LIMA_ARCH)-unknown-linux-musl/release/flint-nfs-server
VM_EXPORT      := /srv/flint-nfs-export

.PHONY: nfs-server-vm
nfs-server-vm: ## Build+run flint-nfs-server INSIDE the Lima VM (real Linux server)
	cd $(CARGO_DIR) && cargo zigbuild --release \
	  --target $(LIMA_ARCH)-unknown-linux-musl --bin flint-nfs-server
	limactl copy $(VM_SERVER_BIN) $(LIMA_VM):/tmp/flint-nfs-server-vm
	limactl shell $(LIMA_VM) -- sudo bash -lc '\
	  systemctl stop flint-nfs-vm 2>/dev/null || true; \
	  systemctl reset-failed flint-nfs-vm 2>/dev/null || true; \
	  chmod +x /tmp/flint-nfs-server-vm; \
	  rm -rf $(VM_EXPORT); mkdir -p $(VM_EXPORT)/.flint-nfs $(VM_EXPORT)/tmp; \
	  printf "%s" "$(NFS_VOLUME_ID)" > $(VM_EXPORT)/.flint-nfs/volume-id; \
	  chmod 0777 $(VM_EXPORT)/tmp; \
	  systemd-run --unit=flint-nfs-vm --collect /tmp/flint-nfs-server-vm \
	    --bind-addr 127.0.0.1 --port $(NFS_PORT) \
	    --export-path $(VM_EXPORT) --volume-id $(NFS_VOLUME_ID)'
	@sleep 3
	@limactl shell $(LIMA_VM) -- sudo ss -lntp | grep -q ":$(NFS_PORT)" \
	  && echo "flint-nfs-server running INSIDE the VM on 127.0.0.1:$(NFS_PORT)"

.PHONY: nfs-server-vm-stop
nfs-server-vm-stop: ## Stop the in-VM flint-nfs-server
	-limactl shell $(LIMA_VM) -- sudo systemctl stop flint-nfs-vm
	-limactl shell $(LIMA_VM) -- sudo systemctl reset-failed flint-nfs-vm

.PHONY: test-nfs-42
test-nfs-42: ## Run the NFSv4.2 conformance tests (ALLOC1-3, COPY5)
	# WHY THIS TARGET EXISTS. testserver.py's --minorversion DEFAULTS TO 1
	# (testserver.py:77) and it skips any test whose declared version range
	# excludes it (:193). st_sparse and st_copy are 4.2-only, so the `all`
	# run above skipped ALLOC1/2/3 and COPY5 in every one of the 25
	# archived artifacts — not because they failed, but because nobody
	# asked for 4.2. With --minorversion=2 they PASS (verified 2026-08-01).
	#
	# These four are the ENTIRE 4.2 surface pynfs has. A gate naming
	# COPY1..COPY4 would fail for the wrong reason: those codes exist in no
	# artifact and in no version of the suite here.
	# Runs against the IN-VM server: ALLOC1-3 exercise fallocate, which does
	# not exist off Linux, so a host-run server fails them unconditionally.
	$(MAKE) nfs-server-vm
	# Delete BOTH copies first. A stale results file is not a harmless
	# leftover: if the run cannot write its JSON (e.g. the file is owned by
	# root from an earlier sudo run) the copy below silently ships the
	# PREVIOUS run's results and the gate passes on them. Observed
	# 2026-08-01 while gating v1.23.0.
	-limactl shell $(LIMA_VM) -- sudo rm -f /tmp/pynfs42.json
	rm -f /tmp/flint-pynfs42-results.json
	limactl shell $(LIMA_VM) -- bash -lc '\
	  cd /opt/pynfs/nfs4.1 && \
	  python3 ./testserver.py 127.0.0.1:$(NFS_PORT)/tmp \
	    --maketree --nocleanup --minorversion=2 \
	    --json=/tmp/pynfs42.json sparse copy || true'
	limactl cp $(LIMA_VM):/tmp/pynfs42.json /tmp/flint-pynfs42-results.json
	@python3 scripts/check-pynfs42.py /tmp/flint-pynfs42-results.json

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

.PHONY: test-pnfs-restart
test-pnfs-restart: build-pnfs ## F67 restart drill (binding survives restart; orphan fails loud)
	tests/lima/pnfs/restart-drill.sh

.PHONY: test-pnfs-csi
test-pnfs-csi: build-pnfs ## End-to-end pNFS CSI integration test (gRPC create → mount → I/O → delete)
	tests/lima/pnfs/csi-e2e.sh

.PHONY: test-pnfs-placement
test-pnfs-placement: build-pnfs ## Fleet-growth placement drill (durable-DS plan Phase 0: pin survives new DS)
	tests/lima/pnfs/placement-drill.sh

.PHONY: test-pnfs-recall
test-pnfs-recall: build-pnfs ## DS-death CB_LAYOUTRECALL e2e (kill DS1, assert MDS recall fires)
	tests/lima/pnfs/recall.sh

.PHONY: test-pnfs-restart
test-pnfs-restart: build-pnfs ## MDS restart survival e2e (Phase B: kill MDS, restart over same state.db, mount keeps working)
	tests/lima/pnfs/restart.sh

.PHONY: test-pnfs-nconnect
test-pnfs-nconnect: build-pnfs ## Single-host nconnect sweep — exposes per-TCP-serial RPC ceiling (loopback only; cross-host is a separate bench)
	tests/lima/pnfs/nconnect.sh

.PHONY: test-pnfs-cross-host
test-pnfs-cross-host: ## Multi-host pNFS perf bench against a real K8s cluster — see tests/k8s/pnfs-bench/README.md for required env (KUBECONFIG, PNFS_IMAGE, MDS_NODE, DS_NODES, CLIENT_NODE)
	tests/k8s/pnfs-bench/cross-host-bench.sh

.PHONY: test-pnfs-identity
test-pnfs-identity: build-pnfs ## DS identity ↔ volume binding guard (Phase 2: stamp, verify, refuse foreign volume)
	tests/lima/pnfs/identity-drill.sh

.PHONY: test-pnfs-fsx
test-pnfs-fsx: build-pnfs ## fsx + fsstress torture (data integrity across namespace ops)
	tests/lima/pnfs/fsx-drill.sh

.PHONY: test-pnfs-mdsbench
test-pnfs-mdsbench: build-pnfs ## MDS metadata-perf bench (LABEL=, MDS_ENV= for A/B; not in the gate)
	tests/lima/pnfs/mdsbench.sh

.PHONY: test-pnfs-block-rig
test-pnfs-block-rig: ## pnfs-block kernel-client rig: stock ≥6.11 kernel does raw NVMe extent I/O (needs cross-built MDS + ~/rig-spdk, see script header)
	tests/lima/pnfs/block-rig.sh

.PHONY: test-pnfs-fence-rig
test-pnfs-fence-rig: ## FenceReaches drill: MDS reservation preempt stops a LIVE raw-path writer at the device (block-rig FENCE=1)
	FENCE=1 tests/lima/pnfs/block-rig.sh

.PHONY: test-pnfs-fence-restart-rig
test-pnfs-fence-restart-rig: ## MdsRestart re-acquire: the fence SURVIVES an MDS restart via stable identity + durable eviction (block-rig FENCE=1 RESTART=1)
	FENCE=1 RESTART=1 tests/lima/pnfs/block-rig.sh

.PHONY: test-pnfs-enospc
test-pnfs-enospc: build-pnfs ## Capacity truth + clean bounded ENOSPC on a 64MB DS (P0-4)
	tests/lima/pnfs/enospc-drill.sh

.PHONY: test-pnfs-fallback
test-pnfs-fallback: build-pnfs ## Bounded-DELAY fallback escalation (DELAY-livelock fix: fast EIO, parked under ceiling, sprung past it, self-recovery)
	tests/lima/pnfs/fallback-drill.sh

.PHONY: test-pnfs-restart-load
test-pnfs-restart-load: build-pnfs ## MDS kill -9 under load (Phase 3: one-heartbeat re-register, zero recalls, I/O rides through)
	tests/lima/pnfs/mds-restart-load.sh

.PHONY: test-pnfs-shard
test-pnfs-shard: build-pnfs ## MDS sharding: 2 shards / shared DS fleet — fan-out, distinct identity, disjoint file_ids, scoped cleanup, blast radius, restart recovery
	tests/lima/pnfs/shard-drill.sh

.PHONY: test-pnfs-shard-bench
test-pnfs-shard-bench: build-pnfs ## MDS sharding aggregate throughput (SHARDS=, P=; not in the gate). Server-parallelism via MDS cores-busy; wall-clock host-capped
	tests/lima/pnfs/shard-bench.sh

.PHONY: test-pnfs-all
test-pnfs-all: ## Run smoke + pynfs + csi-e2e + placement + recall + restart + identity + fallback + enospc + fsx + shard tests in sequence
	$(MAKE) test-pnfs-smoke
	$(MAKE) test-pnfs-pynfs
	$(MAKE) test-pnfs-csi
	$(MAKE) test-pnfs-placement
	$(MAKE) test-pnfs-recall
	$(MAKE) test-pnfs-restart
	$(MAKE) test-pnfs-identity
	$(MAKE) test-pnfs-fallback
	$(MAKE) test-pnfs-enospc
	$(MAKE) test-pnfs-fsx
	$(MAKE) test-pnfs-shard
	# test-pnfs-restart-load is NOT in the gate yet: its core Phase 3
	# assertions pass (one-heartbeat NACK re-register, zero recalls,
	# boot grace) but the final error-free-client-I/O clause exposes an
	# OPEN kill-9-recovery bug — post-restart the kernel client's DS
	# session trunking fails (same-IP rig shape), it abandons the pNFS
	# path, and MDS-fallback writes DELAY until writeback surfaces EIO.
	# Run it standalone; wire it in when the recovery bug is fixed.
