// Emits the dashboard backend's OpenAPI document on stdout.
//
// The committed spec and the SPA's generated types come from here:
//   cargo run -q --bin dashboard-openapi > ../spdk-dashboard/api/openapi.json
//   (cd ../spdk-dashboard && npm run gen:api)
// spdk-dashboard/scripts/check-api-types.sh verifies both are fresh.

fn main() {
    println!("{}", spdk_csi_driver::spdk_dashboard_backend_minimal::openapi_json());
}
