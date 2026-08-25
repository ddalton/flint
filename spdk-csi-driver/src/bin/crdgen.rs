//! Print a CRD.
//!
//!     cargo run --bin crdgen > flint-lite-operator-chart/crds/flintshares.yaml
//!     cargo run --bin crdgen -- lean > flint-lean-chart/crds/flintleanworkspaces.yaml
//!
//! The checked-in copies are INSTALL-TIME BOOTSTRAP only — helm never
//! upgrades `crds/`, so each operator applies its own compiled-in copy
//! at startup (see `lite_operator::bootstrap` and the lean operator's
//! main). Both come from this same artifact, which is why the schema
//! cannot drift between them.

use spdk_csi_driver::{lean_operator, lite_operator::bootstrap};

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "share".into());
    let yaml = match which.as_str() {
        "share" => serde_yaml::to_string(&bootstrap::desired_crd()),
        "lean" => serde_yaml::to_string(&lean_operator::crd::crd()),
        other => {
            eprintln!("crdgen: unknown CRD {other:?} (want: share | lean)");
            std::process::exit(2);
        }
    };
    print!("{}", yaml.expect("CRD serializes"));
}
