.PHONY: help install run logs clean

KUBECONFIG_PATH ?= $(CURDIR)/../system/config/kubeconfig
export KUBECONFIG=$(KUBECONFIG_PATH)

help: ## Show this help message
	@echo "CSI Sanity Test Suite - Available Commands:"
	@echo ""
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'

install: ## Install csi-sanity tool (requires Go)
	@echo "Installing csi-sanity..."
	go install github.com/kubernetes-csi/csi-test/cmd/csi-sanity@latest
	@echo "✅ csi-sanity installed"
	@which csi-sanity

run: ## Run CSI sanity tests in Kubernetes
	@echo "Deploying sanity test pod..."
	kubectl apply -f sanity-test-job.yaml
	@echo ""
	@echo "Waiting for pod to start..."
	kubectl wait --for=condition=Ready pod/csi-sanity-test -n flint-system --timeout=60s || true
	@echo ""
	@echo "✅ Sanity tests running!"
	@echo ""
	@echo "View logs with:"
	@echo "  make logs"
	@echo ""
	@echo "Or follow logs:"
	@echo "  kubectl logs -n flint-system csi-sanity-test -f"

logs: ## Show sanity test logs
	@kubectl logs -n flint-system csi-sanity-test --tail=100

logs-live: ## Follow sanity test logs in real-time
	kubectl logs -n flint-system csi-sanity-test -f

clean: ## Clean up sanity test pod
	@echo "Cleaning up sanity test pod..."
	kubectl delete pod csi-sanity-test -n flint-system --ignore-not-found
	@echo "✅ Cleanup complete"

status: ## Check sanity test pod status
	@kubectl get pod csi-sanity-test -n flint-system 2>/dev/null || echo "❌ Sanity test pod not found"

exec: ## Exec into sanity test pod
	kubectl exec -n flint-system -it csi-sanity-test -- /bin/bash

