# Flint Dashboard

React + TypeScript + Vite frontend for the Flint storage dashboard. It is
served by nginx, which proxies `/api/` to the dashboard backend (the
`csi-driver` binary with `ENABLE_DASHBOARD=true`,
`spdk-csi-driver/src/spdk_dashboard_backend_minimal.rs`) in the same pod.

## Authentication

The backend enforces bearer-token auth on every `/api/*` route except
`/api/login`. Credentials come from the `spdk-dashboard-auth` Secret (chart
values `dashboard.auth.*`):

- `admin` — full access, including destructive disk operations.
- `viewer` — optional read-only role; login disabled unless a viewer
  password is configured.

If no admin password is configured, the chart generates one at install time
(preserved across upgrades). Read it with:

```sh
kubectl -n flint-system get secret spdk-dashboard-auth \
  -o jsonpath='{.data.admin-password}' | base64 -d
```

Sessions live in backend memory: a backend restart (or token expiry,
default 12h) sends the SPA back to the login page.

## Development

```sh
# Point the vite dev proxy at a real backend:
kubectl -n flint-system port-forward deploy/spdk-dashboard 8080:8080
npm run dev
```

## Build

The production image is built from `Dockerfile.frontend` (used by
`scripts/release.sh`):

```sh
docker build -t spdk-dashboard-frontend . -f Dockerfile.frontend
docker run -p 8080:3000 spdk-dashboard-frontend
```

Note: the Helm chart overrides `nginx.conf` with the
`spdk-dashboard-nginx-config` ConfigMap; keep the two in sync.
