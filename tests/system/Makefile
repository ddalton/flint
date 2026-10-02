.PHONY: help install test test-verbose test-single clean check-chainsaw check-kubeconfig test-nfs-only test-kind-nfs-only

# Configuration
KUBECONFIG_PATH ?= $(CURDIR)/config/kubeconfig
export KUBECONFIG=$(KUBECONFIG_PATH)

help: ## Show this help message
	@echo "CSI Driver Test Framework - Available Commands:"
	@echo ""
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'

check-kubeconfig: ## Check if kubeconfig is configured
	@echo "Checking for kubeconfig..."
	@if [ -n "$$KUBECONFIG" ] && [ -f "$$KUBECONFIG" ]; then \
		echo "✅ Using KUBECONFIG from environment: $$KUBECONFIG"; \
	elif [ -f "$(KUBECONFIG_PATH)" ]; then \
		echo "✅ Using kubeconfig: $(KUBECONFIG_PATH)"; \
	elif [ -f "$$HOME/.kube/config" ]; then \
		echo "⚠️  No local kubeconfig found, using default: $$HOME/.kube/config"; \
		echo "   Consider creating config/kubeconfig for this project"; \
	else \
		echo "❌ No kubeconfig found!"; \
		echo ""; \
		echo "Please provide a kubeconfig file using one of these methods:"; \
		echo "  1. Create config/kubeconfig:"; \
		echo "     cp config/kubeconfig.example config/kubeconfig"; \
		echo "     # Edit with your cluster details"; \
		echo ""; \
		echo "  2. Set KUBECONFIG environment variable:"; \
		echo "     export KUBECONFIG=/path/to/your/kubeconfig"; \
		echo ""; \
		echo "  3. Symlink to your default kubectl config:"; \
		echo "     ln -s ~/.kube/config config/kubeconfig"; \
		echo ""; \
		exit 1; \
	fi

check-chainsaw: ## Check if chainsaw is installed
	@which chainsaw > /dev/null || (echo "❌ chainsaw not found. Install with: make install (or brew install kyverno/chainsaw/chainsaw)" && exit 1)
	@echo "✅ chainsaw is installed"

install: ## Install chainsaw (go install, the version CI pins)
	@echo "Installing chainsaw..."
	go install github.com/kyverno/chainsaw@v0.2.15
	@echo "✅ chainsaw installed successfully"

test: check-kubeconfig check-chainsaw ## Run all tests (standard tests in parallel, clean-shutdown + replica-rebuild separately)
	@echo "Running standard CSI driver tests..."
	chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard
	@echo ""
	@echo "Running clean-shutdown test (must run in isolation)..."
	chainsaw test --config chainsaw-clean-shutdown.yaml --test-dir tests
	@echo ""
	@echo "Running incremental-replica-rebuild test (must run in isolation)..."
	chainsaw test --config chainsaw-replica-rebuild.yaml --test-dir tests-replica-rebuild

test-verbose: check-kubeconfig check-chainsaw ## Run all tests with verbose output
	@echo "Running all CSI driver tests (verbose)..."
	chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard

test-single: check-kubeconfig check-chainsaw ## Run a single test (usage: make test-single TEST=rwo-pvc-migration)
	@if [ -z "$(TEST)" ]; then \
		echo "❌ Please specify TEST variable. Example: make test-single TEST=rwo-pvc-migration"; \
		exit 1; \
	fi
	@echo "Running test: $(TEST)..."
	chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard/$(TEST)

test-rwo: check-kubeconfig check-chainsaw ## Run RWO PVC migration test
	chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard/rwo-pvc-migration

test-multi-replica: check-kubeconfig check-chainsaw ## Run multi-replica test
	chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard/multi-replica

test-expand: check-kubeconfig check-chainsaw ## Run volume expansion test
	chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard/volume-expansion

test-clean-shutdown: check-kubeconfig check-chainsaw ## Run clean shutdown test (runs in isolation)
	chainsaw test --config chainsaw-clean-shutdown.yaml --test-dir tests

test-replica-rebuild: check-kubeconfig check-chainsaw ## Run incremental replica rebuild test (runs in isolation)
	chainsaw test --config chainsaw-replica-rebuild.yaml --test-dir tests-replica-rebuild

test-nfs-only: check-kubeconfig check-chainsaw ## Run the nfs-only suite (no SPDK; needs the flint-nfs StorageClass)
	chainsaw test --config chainsaw-nfs-only.yaml --test-dir tests-nfs-only

test-kind-nfs-only: ## Build this tree's driver, run the nfs-only suite on a throwaway kind cluster (what CI runs)
	./kind-nfs-only.sh

test-snapshot: check-kubeconfig check-chainsaw ## Run snapshot restore test
	chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard/snapshot-restore

clean: ## Clean up test namespaces left by --skip-delete (chainsaw-*)
	@echo "Cleaning up test namespaces..."
	@kubectl get ns -o name | grep '^namespace/chainsaw-' | xargs -r kubectl delete
	@echo "✅ Cleanup complete"

logs: ## Show logs from test pods (usage: make logs NS=chainsaw-xxx)
	@if [ -z "$(NS)" ]; then \
		echo "Available test namespaces:"; \
		kubectl get ns | grep chainsaw-; \
		echo ""; \
		echo "Usage: make logs NS=chainsaw-xxx"; \
	else \
		kubectl get pods -n $(NS) && \
		echo "" && \
		read -p "Enter pod name: " POD && \
		kubectl logs $$POD -n $(NS); \
	fi

debug: check-kubeconfig check-chainsaw ## Keep failed test resources for debugging
	chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard --skip-delete

validate: check-chainsaw ## Lint every Chainsaw test and configuration
	@for f in $$(find tests tests-* -name chainsaw-test.yaml) ; do echo "Checking $$f"; chainsaw lint test -f $$f || exit 1; done
	@for f in chainsaw-*.yaml ; do echo "Checking $$f"; chainsaw lint configuration -f $$f || exit 1; done
	@echo "✅ Validation complete"

list-tests: ## List all available tests
	@echo "Available tests:"
	@find tests tests-* -name chainsaw-test.yaml | sed 's|/chainsaw-test.yaml||' 

restart-csi: check-kubeconfig ## Restart Flint CSI driver pods (required after loading ublk module)
	@echo "Restarting Flint CSI driver pods..."
	@echo "Deleting node agent pods..."
	kubectl delete pods -n flint-system -l app=flint-csi-node --ignore-not-found
	@echo "Deleting controller pods..."
	kubectl delete pods -n flint-system -l app=flint-csi-controller --ignore-not-found
	@echo "Waiting for pods to restart..."
	@kubectl wait --for=condition=ready pod -l app=flint-csi-node -n flint-system --timeout=120s || true
	@kubectl wait --for=condition=ready pod -l app=flint-csi-controller -n flint-system --timeout=120s || true
	@echo "✅ CSI driver pods restarted"
	@echo ""
	@kubectl get pods -n flint-system
