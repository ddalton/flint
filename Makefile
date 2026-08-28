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
	# The run's exit status decides this target's status. It used to end
	# in `|| true`, so a suite that collapsed to zero passes — or never
	# started — still exited 0 and the target looked green. Capture the
	# status on its OWN line (a pipeline's status is the last command's,
	# a trap this repo has paid for more than once), pull the JSON back
	# either way so a failure is diagnosable, and only then fail.
	# NO `set -e` here: it would abort on the failing run before the
	# status could be captured, which is the same class of mistake as
	# the `|| true` being removed.
	@limactl shell $(LIMA_VM) -- bash -lc '\
	    cd /opt/pynfs/nfs4.1 && \
	    python3 ./testserver.py $(LIMA_HOST_ADDR):$(NFS_PORT)/tmp \
	      --maketree --nocleanup --json=/tmp/pynfs.json all' \
	    > /tmp/flint-pynfs-run.log 2>&1; \
	  limactl cp $(LIMA_VM):/tmp/pynfs.json /tmp/flint-pynfs-results.json || true; \
	  tail -20 /tmp/flint-pynfs-run.log
	# The gate is the checker, NOT pynfs's exit status: the suite exits
	# non-zero while any test fails, and this one has known deferred
	# failures, so gating on it would be permanently red. The checker
	# fails on the outcomes that actually matter — a run that did not
	# happen, and a pass count below the recorded floor. Same shape as
	# check-pynfs42.py.
	@python3 scripts/check-pynfs.py /tmp/flint-pynfs-results.json

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

# ───────────── The HUB binary under the suite (leg C1) ──────────────────────
#
# Everything above drives `flint-nfs-server`. flint-lite ships
# `flint-pnfs-mds --standalone`, a DIFFERENT binary with a different
# bring-up, and until this target existed no external suite had ever
# been pointed at it. Three recovery mechanisms had already drifted far
# enough apart to ship as defects. A pynfs number for one front-end is
# not a pynfs number for the other, and this target is what makes the
# claim checkable rather than assumed.
MDS_VM_BIN     := $(CARGO_DIR)/target/$(LIMA_ARCH)-unknown-linux-musl/release/flint-pnfs-mds
MDS_VM_EXPORT  := /srv/flint-mds-export
MDS_VM_STATE   := /srv/flint-mds-state
MDS_VM_CONFIG  := tests/lima/pnfs/lite-pynfs.yaml

.PHONY: pnfs-mds-vm
pnfs-mds-vm: ## Build+run flint-pnfs-mds --standalone INSIDE the Lima VM as root (the flint-lite hub posture)
	cd $(CARGO_DIR) && cargo zigbuild --release \
	  --target $(LIMA_ARCH)-unknown-linux-musl --bin flint-pnfs-mds
	limactl copy $(MDS_VM_BIN) $(LIMA_VM):/tmp/flint-pnfs-mds-vm
	limactl copy $(MDS_VM_CONFIG) $(LIMA_VM):/tmp/lite-pynfs.yaml
	# Root, deliberately: the hub runs as uid 0 today (finding 7), and a
	# conformance run under a different uid would measure a posture we do
	# not ship. Any number taken here is a number for the shipped posture.
	limactl shell $(LIMA_VM) -- sudo bash -lc '\
	  systemctl stop flint-mds-vm 2>/dev/null || true; \
	  systemctl reset-failed flint-mds-vm 2>/dev/null || true; \
	  rm -rf $(MDS_VM_EXPORT) $(MDS_VM_STATE); \
	  mkdir -p $(MDS_VM_EXPORT)/tmp $(MDS_VM_STATE); \
	  chmod 0777 $(MDS_VM_EXPORT)/tmp; \
	  chmod +x /tmp/flint-pnfs-mds-vm; \
	  systemd-run --unit=flint-mds-vm --collect \
	    --setenv=FLINT_NFS_GRACE_SECS=900 \
	    --setenv=RUST_LOG=$${MDS_LOG:-info} \
	    /tmp/flint-pnfs-mds-vm --config /tmp/lite-pynfs.yaml'
	@sleep 4
	# Two assertions, not one. The listener proves something is up; the
	# STANDALONE banner proves it is the hub posture and not the pNFS one.
	# Without the second, this target would happily measure a server that
	# hands out layouts — a different code path — and report it as the
	# flint-lite number.
	@limactl shell $(LIMA_VM) -- sudo ss -lntp | grep -q ":$(NFS_PORT)" \
	  || { echo "FAIL: flint-pnfs-mds is not listening on $(NFS_PORT)"; \
	       limactl shell $(LIMA_VM) -- sudo journalctl -u flint-mds-vm --no-pager | tail -40; \
	       exit 1; }
	@limactl shell $(LIMA_VM) -- sudo journalctl -u flint-mds-vm --no-pager \
	  | grep -c "Posture: STANDALONE" > /tmp/flint-mds-posture.count; \
	  test "$$(cat /tmp/flint-mds-posture.count)" -ge 1 \
	  || { echo "FAIL: server is up but did NOT log the STANDALONE posture — \
	            this is not the flint-lite hub, and its number must not be quoted as one"; \
	       exit 1; }
	@echo "flint-pnfs-mds (STANDALONE) running INSIDE the VM on 0.0.0.0:$(NFS_PORT)"

.PHONY: pnfs-mds-vm-stop
pnfs-mds-vm-stop: ## Stop the in-VM flint-pnfs-mds
	-limactl shell $(LIMA_VM) -- sudo systemctl stop flint-mds-vm
	-limactl shell $(LIMA_VM) -- sudo systemctl reset-failed flint-mds-vm

.PHONY: test-nfs-protocol-mds
test-nfs-protocol-mds: pnfs-mds-vm ## Leg C1: full pynfs 4.1 suite against the HUB binary (flint-pnfs-mds --standalone), in-VM, as root
	-limactl shell $(LIMA_VM) -- sudo rm -f /tmp/pynfs-mds.json
	rm -f /tmp/flint-pynfs-mds-results.json
	# Same shape as test-nfs-protocol: capture the status on its OWN line
	# (a pipeline's status is the last command's), pull the JSON back
	# either way so a failure is diagnosable, and let the checker decide.
	@limactl shell $(LIMA_VM) -- bash -lc '\
	    cd /opt/pynfs/nfs4.1 && \
	    python3 ./testserver.py 127.0.0.1:$(NFS_PORT)/tmp \
	      --maketree --nocleanup --json=/tmp/pynfs-mds.json all' \
	    > /tmp/flint-pynfs-mds-run.log 2>&1; \
	  limactl cp $(LIMA_VM):/tmp/pynfs-mds.json /tmp/flint-pynfs-mds-results.json || true; \
	  tail -20 /tmp/flint-pynfs-mds-run.log
	# The hub carries its OWN floor. Sharing the standalone server's
	# baseline would hide exactly the divergence this leg exists to
	# measure: if the two binaries drift, one gate over one number cannot
	# say which one moved.
	@python3 scripts/check-pynfs.py /tmp/flint-pynfs-mds-results.json \
	  tests/lima/pynfs-mds-baseline.json

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

.PHONY: test-authz-drill
test-authz-drill: ## Leg A1: cross-uid authorization gate (seconds) — runs against flint AND knfsd as a control
	# Two arms, and the control is the point. knfsd MUST be green: an
	# assertion it also fails is this drill being wrong about POSIX, not
	# the server being wrong. Measured 2026-08-24: knfsd 9/9, flint 2/9.
	limactl copy tests/lima/pnfs/access-authz-drill.sh $(LIMA_VM):/tmp/access-authz-drill.sh
	# §0 rule 4: assert the mounts BEFORE trusting either arm. Both paths
	# are pre-existing mounts this target does not create, and an
	# unmounted /mnt/pjd (the ordinary state after a VM reboot — and two
	# sessions share this VM) leaves both arms exercising local ext4 and
	# reporting 9/9 green, having never spoken to flint or to knfsd.
	# A drill that measures whoever last mounted the VM is worse than no
	# drill, because it reports.
	@limactl shell $(LIMA_VM) -- sudo bash -lc 'for m in /mnt/knfsd/pjd /mnt/pjd/tmp/pjd; do \
	     t=$$(findmnt -n -o FSTYPE -T "$$m" 2>/dev/null || true); \
	     case "$$t" in nfs4|nfs) ;; \
	       *) echo "VOID: $$m is fstype '\''$$t'\'', not nfs4 — nothing was mounted, so this drill would have measured local disk"; exit 1;; \
	     esac; done'
	@limactl shell $(LIMA_VM) -- sudo bash -lc 'chmod +x /tmp/access-authz-drill.sh; \
	   /tmp/access-authz-drill.sh /mnt/knfsd/pjd knfsd' \
	  || { echo "VOID: the knfsd CONTROL arm failed — the drill is wrong about POSIX, fix it there"; exit 1; }
	@limactl shell $(LIMA_VM) -- sudo bash -lc '/tmp/access-authz-drill.sh /mnt/pjd/tmp/pjd flint'

.PHONY: test-pjdfstest
test-pjdfstest: ## Leg A0: full pjdfstest (8798 assertions) vs flint, differenced against a knfsd control (~7 min)
	bash tests/lima/pnfs/pjdfstest-differential.sh

.PHONY: test-perf-differential
test-perf-differential: ## Leg L-perf: throughput + metadata vs a knfsd control, ratio-gated (~5 min)
	# The repo's only performance gate. It reports RATIOS against knfsd
	# measured in the same session with the arms interleaved, because
	# absolute MiB/s from this VM is not a comparable quantity — the rig
	# has been measured drifting ~2x within one session.
	#
	# It is RED until someone records a baseline from a run they have
	# inspected on a quiet rig, and that is the correct resting state:
	# the alternative is the xfstests trap, where a missing baseline
	# meant a 40-minute suite that could not fail.
	bash tests/lima/pnfs/perf-differential.sh
	python3 scripts/check-perf.py tests/lima/perf-latest.json
	# The falsifiability arm must FAIL. A gate that cannot see a mount
	# deliberately crippled to 4 KiB rsize cannot see a regression
	# either, and every green run above would mean nothing.
	@if python3 scripts/check-perf.py tests/lima/perf-latest-crippled.json >/dev/null 2>&1; then \
	    echo "VOID: the crippled control PASSED — this gate cannot fail, so it is not a gate"; \
	    exit 1; \
	else \
	    echo "falsifiability arm correctly refused"; \
	fi

.PHONY: test-xfstests
test-xfstests: ## Leg C10: xfstests generic/ vs flint, differenced against a knfsd control (~40 min)
	# The suite every in-tree Linux filesystem is gated on. Runs on a
	# PRIVATE port (24490) and a PRIVATE binary path: 20490 and
	# /tmp/flint-pnfs-mds-vm belong to the pynfs rig, and this VM is
	# shared. Override with XFS_GROUP= to scope the run.
	bash tests/lima/pnfs/xfstests-differential.sh

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
	# The `-` prefixes this used to carry made the target INCAPABLE of
	# failing: make ignored all three results and the recipe's status
	# became nfs-server-stop's. The intent was "stop the server even if a
	# test fails", which is right; this keeps that and still reports.
	@rc=0; \
	$(MAKE) test-nfs-mount    || rc=1; \
	$(MAKE) test-nfs-protocol || rc=1; \
	$(MAKE) test-nfs-frag     || rc=1; \
	$(MAKE) nfs-server-stop; \
	exit $$rc

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

.PHONY: test-pnfs-mds-restart
test-pnfs-mds-restart: build-pnfs ## MDS restart survival e2e (Phase B: kill MDS, restart over same state.db, mount keeps working)
	tests/lima/pnfs/restart.sh

.PHONY: test-pnfs-crosscluster
test-pnfs-crosscluster: build-pnfs ## Two DISTINCT NFS clients (netns+UTS = two "clusters") share one volume: close-to-open, cross-client locks, sqlite/git battery, mid-flight DS-direct census, metadata rates
	tests/lima/pnfs/cross-cluster-drill.sh

.PHONY: test-pnfs-lite
test-pnfs-lite: build-pnfs ## Flint-lite L0: ONE standalone hub (mode: standalone, no DS fleet) serves two distinct clients — coherence, locks, sqlite/git, no-LAYOUTGET oracle, MDS-lane baselines
	tests/lima/pnfs/lite-drill.sh

.PHONY: test-tier-drill
test-tier-drill: build-pnfs ## S3-tier e2e vs MinIO (docker): capture→flush→manifest, tombstones, restart, evict, hydrate under a kernel client, DR-from-bucket
	tests/lima/pnfs/tier-drill.sh

.PHONY: test-tier-chaos
test-tier-chaos: build-pnfs ## S3-tier CHAOS drill vs MinIO: split-brain, outage, foreign hands, space pressure, crash loops, endurance, two writers, zombie hub, neighbor prefixes, versioned recovery, restart storms (~13 min; CRASH_ITERS/ENDURE_SECS/PHASES tunable)
	tests/lima/pnfs/tier-chaos.sh

.PHONY: test-tier-scale
test-tier-scale: build-pnfs ## S3-tier SCALE drill vs MinIO: 10k files through flush/manifest/DR-import + one 2 GiB file through multipart/evict/hydrate, wall-times and peak-RSS record (FILES/BIGMB tunable)
	tests/lima/pnfs/tier-scale.sh

.PHONY: test-lite-kind-e2e
test-lite-kind-e2e: ## Flint-lite L1 e2e: the CHART's hub on kind (default-SC PVC, NodePort) serves the Lima VM's REAL kernel client — battery + bytes-on-PVC + restart-under-mount. Builds the hub image from the working tree
	tests/regression/lite-kind-e2e.sh

.PHONY: test-lite-kind-tier-e2e
test-lite-kind-tier-e2e: ## Flint-lite L3 e2e: the CHART's tier config live — in-cluster MinIO, Secret creds via envFrom, epoch claim, flush-to-bucket, restart re-claim, DR-from-bucket with hydrate-on-read (~10 min)
	tests/regression/lite-kind-tier-e2e.sh

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

.PHONY: test-pnfs-ptpl-rig
test-pnfs-ptpl-rig: ## PTPL survives a TGT restart: reservation restored from ptpl_file on ns re-add, fence survives (block-rig FENCE=1 TGT_RESTART=1)
	FENCE=1 TGT_RESTART=1 tests/lima/pnfs/block-rig.sh

.PHONY: test-pnfs-fenced-record-rig
test-pnfs-fenced-record-rig: ## Durable fenced record: fence survives a tgt restart WITH the ptpl_file destroyed — re-acquired from sqlite (block-rig FENCE=1 TGT_RESTART=1 PTPL_LOSS=1)
	FENCE=1 TGT_RESTART=1 PTPL_LOSS=1 tests/lima/pnfs/block-rig.sh

.PHONY: test-pnfs-unfence-rig
test-pnfs-unfence-rig: ## The fence is REVERSIBLE: UnfenceBlockClient releases the reservation and the frozen device counter moves again (block-rig FENCE=1 UNFENCE=1)
	FENCE=1 UNFENCE=1 tests/lima/pnfs/block-rig.sh

.PHONY: test-kind-chart-pass
test-kind-chart-pass: ## Validate the pnfs-block CHART surface against a real API server on kind (renders, refuses what it must; no images, no data path — the docker VM kernel is below the 6.11 floor)
	tests/regression/kind-chart-pass.sh

.PHONY: test-kind-witness
test-kind-witness: ## The composition witness against a REAL API server, as the chart's ServiceAccount: proves the resourceVersion CAS actually refuses a stale write, and that the Role grants every verb the store calls
	tests/regression/kind-witness-pass.sh

.PHONY: test-pnfs-unfence-noreboot-rig
test-pnfs-unfence-noreboot-rig: ## Unfence WITHOUT the reboot (only valid when the fenced writer errored): proves recovery re-registers the client's reservation key, so the never-re-registers hazard is unreachable (block-rig FENCE=1 UNFENCE=1 NOREBOOT=1)
	FENCE=1 UNFENCE=1 NOREBOOT=1 tests/lima/pnfs/block-rig.sh

.PHONY: test-pnfs-reconcile-rig
test-pnfs-reconcile-rig: ## A tgt-ONLY restart repairs WITHOUT an MDS roll: the periodic export reconcile loop rebuilds the export chain from sqlite (block-rig RECONCILE=1)
	RECONCILE=1 tests/lima/pnfs/block-rig.sh

.PHONY: test-pnfs-sweep-rig
test-pnfs-sweep-rig: ## Lease-sweep partition drill: NFS port dropped under a live raw writer, the sweep fences/revokes/auto-unfences on the timer, successor recovers leverless (block-rig SWEEP=1)
	SWEEP=1 tests/lima/pnfs/block-rig.sh

.PHONY: test-pnfs-replica-rig
test-pnfs-replica-rig: ## Two-target composition on real spdk-tgt: placement, frame, sparse rebuild, byte-for-byte mirror, degrade barrier, rejoin, and the COMPOSER-DEATH failover with the client redirecting itself
	tests/lima/pnfs/replica-rig.sh

.PHONY: test-pnfs-replica-fs-rig
test-pnfs-replica-fs-rig: ## The same failover with a MOUNTED ext4 and I/O in flight (replica-rig FS=1): durability across the redirect is asserted, whether the mount rides it live is MEASURED
	FS=1 tests/lima/pnfs/replica-rig.sh

.PHONY: test-pnfs-replica-mdsdeath-rig
test-pnfs-replica-mdsdeath-rig: ## The NODE-death shape (replica-rig MDS_DEATH=1): MDS-A dies WITH tgt-A, so the re-attach must be answered by a shard that never created the volume — the defect that stranded runbq's client
	MDS_DEATH=1 tests/lima/pnfs/replica-rig.sh

.PHONY: test-pnfs-preempt-rig
test-pnfs-preempt-rig: ## Foreign-holder fence arm: an adversarial registered key + WE reservation is preempted away, and a second volume on the same tgt never notices (block-rig PREEMPT=1)
	PREEMPT=1 tests/lima/pnfs/block-rig.sh

.PHONY: test-pnfs-zombie-rig
test-pnfs-zombie-rig: ## Frozen-VM zombie: a second VM is SIGSTOPped mid-write, swept, its extents reused by a successor — whose bytes must survive the resume (block-rig ZOMBIE=1)
	ZOMBIE=1 tests/lima/pnfs/block-rig.sh

.PHONY: test-pnfs-multi-rig
test-pnfs-multi-rig: ## TWO real client hosts on one block volume: additive admission, disjoint extents across hosts, same-file contention refused, per-client fence (block-rig MULTI=1)
	MULTI=1 tests/lima/pnfs/block-rig.sh

.PHONY: test-pnfs-expand-rig
test-pnfs-expand-rig: ## Block capacity is real: a full volume reports ENOSPC to the app, then a live expand grows the lvol, the kernel's namespace and the arena ceiling (block-rig EXPAND=1)
	EXPAND=1 tests/lima/pnfs/block-rig.sh

.PHONY: test-pnfs-expand-bounce-rig
test-pnfs-expand-bounce-rig: ## The expand drill with an MDS RESTART between the device fetch and the expand: the durable notify book must still reach the client through its NEW session (block-rig EXPAND=1 MDS_BOUNCE=1)
	EXPAND=1 MDS_BOUNCE=1 tests/lima/pnfs/block-rig.sh

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
test-pnfs-all: ## Run smoke + pynfs + csi-e2e + placement + recall + restart (F67) + mds-restart + identity + fallback + enospc + fsx + shard tests in sequence
	$(MAKE) test-pnfs-smoke
	$(MAKE) test-pnfs-pynfs
	$(MAKE) test-pnfs-csi
	$(MAKE) test-pnfs-placement
	$(MAKE) test-pnfs-recall
	$(MAKE) test-pnfs-restart
	$(MAKE) test-pnfs-mds-restart
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
