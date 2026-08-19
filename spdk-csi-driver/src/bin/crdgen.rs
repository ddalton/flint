//! Print the FlintShare CRD.
//!
//!     cargo run --bin crdgen > flint-lite-operator-chart/crds/flintshares.yaml
//!
//! The checked-in copy is INSTALL-TIME BOOTSTRAP only — helm never
//! upgrades `crds/`, so the operator applies its own compiled-in copy
//! at startup (see `lite_operator::bootstrap`). Both come from this
//! same artifact, which is why the schema cannot drift between them.

use spdk_csi_driver::lite_operator::bootstrap;

fn main() {
    print!(
        "{}",
        serde_yaml::to_string(&bootstrap::desired_crd()).expect("CRD serializes")
    );
}
