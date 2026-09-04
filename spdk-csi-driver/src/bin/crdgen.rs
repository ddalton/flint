//! Print a CRD.
//!
//!     cargo run --bin crdgen > flint-lite-operator-chart/crds/flintshares.yaml
//!     cargo run --bin crdgen -- lean > flint-lean-chart/crds/flintleanworkspaces.yaml
//!     cargo run --bin crdgen -- forge > flint-forge-chart/crds/flintrepos.yaml
//!
//! The checked-in copies are INSTALL-TIME BOOTSTRAP only — helm never
//! upgrades `crds/`, so each operator applies its own compiled-in copy
//! at startup (see `lite_operator::bootstrap` and the lean operator's
//! main). Both come from this same artifact, which is why the schema
//! cannot drift between them.

use spdk_csi_driver::{forge_operator, lean_operator, lite_operator::bootstrap};

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "share".into());
    let yaml = match which.as_str() {
        "share" => serde_yaml::to_string(&bootstrap::desired_crd()),
        "lean" => serde_yaml::to_string(&lean_operator::crd::crd()),
        "forge" => serde_yaml::to_string(&forge_operator::crd::crd()),
        other => {
            eprintln!("crdgen: unknown CRD {other:?} (want: share | lean | forge)");
            std::process::exit(2);
        }
    };
    print!("{}", yaml.expect("CRD serializes"));
}
