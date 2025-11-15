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

test: check-kubeconfig check-kuttl ## Run all tests
	@echo "Running all CSI driver tests..."
	kubectl kuttl test --config kuttl-testsuite.yaml

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

test-rwx: check-kubeconfig check-kuttl ## Run RWX multi-pod test
	kubectl kuttl test --test rwx-multi-pod

test-expand: check-kubeconfig check-kuttl ## Run volume expansion test
	kubectl kuttl test --test volume-expansion

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
