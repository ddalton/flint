.PHONY: help install test test-verbose test-single clean check-kuttl check-kubeconfig

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

check-kuttl: ## Check if kuttl is installed
	@which kubectl-kuttl > /dev/null || (echo "❌ Kuttl not found. Install with: kubectl krew install kuttl" && exit 1)
	@echo "✅ Kuttl is installed"

install: ## Install kuttl (requires kubectl krew)
	@echo "Installing kuttl..."
	kubectl krew install kuttl
	@echo "✅ Kuttl installed successfully"

test: check-kubeconfig check-kuttl ## Run all tests (standard tests in parallel, clean-shutdown separately)
	@echo "Running standard CSI driver tests..."
	kubectl kuttl test --config kuttl-testsuite.yaml
	@echo ""
	@echo "Running clean-shutdown test (must run in isolation)..."
	kubectl kuttl test --config kuttl-testsuite-clean-shutdown.yaml

test-verbose: check-kubeconfig check-kuttl ## Run all tests with verbose output
	@echo "Running all CSI driver tests (verbose)..."
	kubectl kuttl test --config kuttl-testsuite.yaml --suppress=

test-single: check-kubeconfig check-kuttl ## Run a single test (usage: make test-single TEST=rwo-pvc-migration)
	@if [ -z "$(TEST)" ]; then \
		echo "❌ Please specify TEST variable. Example: make test-single TEST=rwo-pvc-migration"; \
		exit 1; \
	fi
	@echo "Running test: $(TEST)..."
	kubectl kuttl test --test $(TEST)

test-rwo: check-kubeconfig check-kuttl ## Run RWO PVC migration test
	kubectl kuttl test --test rwo-pvc-migration

test-multi-replica: check-kubeconfig check-kuttl ## Run multi-replica test
	kubectl kuttl test --test multi-replica

test-expand: check-kubeconfig check-kuttl ## Run volume expansion test
	kubectl kuttl test --test volume-expansion

test-clean-shutdown: check-kubeconfig check-kuttl ## Run clean shutdown test (runs in isolation)
	kubectl kuttl test --config kuttl-testsuite-clean-shutdown.yaml

test-snapshot: check-kubeconfig check-kuttl ## Run snapshot restore test
	kubectl kuttl test --test snapshot-restore

clean: ## Clean up test namespaces (kuttl-test-*)
	@echo "Cleaning up test namespaces..."
	@kubectl get ns | grep kuttl-test | awk '{print $$1}' | xargs -r kubectl delete ns
	@echo "✅ Cleanup complete"

logs: ## Show logs from test pods (usage: make logs NS=kuttl-test-xxx)
	@if [ -z "$(NS)" ]; then \
		echo "Available test namespaces:"; \
		kubectl get ns | grep kuttl-test; \
		echo ""; \
		echo "Usage: make logs NS=kuttl-test-xxx"; \
	else \
		kubectl get pods -n $(NS) && \
		echo "" && \
		read -p "Enter pod name: " POD && \
		kubectl logs $$POD -n $(NS); \
	fi

debug: check-kubeconfig check-kuttl ## Keep failed test resources for debugging
	kubectl kuttl test --config kuttl-testsuite.yaml --skip-delete

validate: ## Validate YAML syntax
	@echo "Validating YAML files..."
	@find tests -name "*.yaml" -exec echo "Checking {}" \; -exec kubectl apply --dry-run=client -f {} \; 2>&1 | grep -v "configured (dry run)"
	@echo "✅ Validation complete"

list-tests: ## List all available tests
	@echo "Available tests:"
	@find tests -mindepth 1 -maxdepth 1 -type d -exec basename {} \;

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
