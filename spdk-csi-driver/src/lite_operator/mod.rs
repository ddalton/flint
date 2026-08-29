//! The flint-lite operator — a fleet control plane for hub-per-volume
//! shares (plan of record: `docs/plans/flint-lite-operator-plan.md`).
//!
//! # What it is
//!
//! One `FlintShare` custom resource per volume; the controller renders
//! the same four objects the lite chart renders (ConfigMap, RWO PVC,
//! Service, single-replica Recreate Deployment) and keeps them
//! converged with server-side apply. The chart stays supported — the
//! render-parity golden test (`render::tests`) fails the build if the
//! two ever drift.
//!
//! # Why, in one line each
//!
//! - **No reusable release state.** Every reconcile re-renders from
//!   the CR plus operator defaults, which structurally kills the
//!   `--reuse-values` failure class (runbr) that helm-release-per-
//!   volume keeps alive.
//! - **The knobs are schema.** `spec.settings` is a typed mirror of
//!   [`crate::pnfs::config::TierKnobs`], so a typo is refused at
//!   admission instead of silently taking a default (the server's YAML
//!   parser ignores unknown keys — the chart's hand-written `$known`
//!   list exists for exactly this and is one more copy to drift).
//! - **Fleet operations become queries.** `kubectl get flintshares` is
//!   the fleet, and one controller can enforce cross-object invariants
//!   no per-release install can see (see [`conflict`]).
//!
//! # The three invariants
//!
//! 1. **The PVC never carries an ownerReference.** Owner GC does not
//!    know what `reclaim: Retain` means; for a tier-off share the PVC
//!    is the only copy of the data. Fail-safe by construction, not by
//!    reconcile correctness ([`reconcile`]).
//! 2. **The bucket is never touched.** No create, no delete, no
//!    lifecycle — the operator's blast radius stops at Kubernetes
//!    objects.
//! 3. **At most one share per (endpoint, bucket, prefix subtree).**
//!    Unarbitrated duplicates are not merely wasteful: when one hub
//!    dies for a lease window the other TAKES OVER the prefix and
//!    serves another tenant's bytes at its own address ([`conflict`]).

pub mod bootstrap;
pub mod conflict;
pub mod crd;
pub mod hubstatus;
pub mod idle;
pub mod persistence;
pub mod reconcile;
pub mod render;

/// The published guide must pin the chart it documents.
///
/// `docs/flint-lite-for-agent-fleets.md` is a copy-paste install: a
/// reader runs its `helm install --version X` verbatim. When a release
/// bumps the chart and the guide keeps the old pin, that reader
/// silently installs the PREVIOUS operator — every fix in the release
/// they just read about is absent, and nothing anywhere says so. That
/// is exactly how the guide came to advertise chart 0.2.7 / images
/// 1.35.1 on the day 0.2.8 / 1.36.0 shipped.
///
/// The doc drill (`tests/regression/agent-fleet-doc-drill.sh`) cannot
/// catch this: it supplies its OWN `CHART_VER` rather than reading the
/// guide's, so it proves the PROCEDURE works while the stated versions
/// drift freely. This is the missing half — it proves the NUMBERS are
/// the ones we ship.
///
/// The `.html` is checked with the `.md` because both are published and
/// the `.pdf` is rendered FROM the html, so html parity is the cheapest
/// place to catch all three going stale together.
#[cfg(test)]
mod guide_pins {
    const CHART: &str = include_str!("../../../flint-lite-operator-chart/Chart.yaml");
    const GUIDE_MD: &str = include_str!("../../../docs/flint-lite-for-agent-fleets.md");
    const GUIDE_HTML: &str = include_str!("../../../docs/flint-lite-for-agent-fleets.html");

    /// `key: value` from Chart.yaml, unquoted. Deliberately not a YAML
    /// parse: two fields, and a dependency here would be the only one.
    fn field(key: &str) -> String {
        CHART
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or_else(|| panic!("{key} missing from flint-lite-operator-chart/Chart.yaml"))
            .trim()
            .trim_matches('"')
            .to_string()
    }

    #[test]
    fn the_guide_pins_the_chart_and_images_it_documents() {
        let chart_version = field("version:");
        let app_version = field("appVersion:");

        // Guard the guard: if the chart ever stops reporting a real
        // version, every assertion below would pass against "".
        assert!(
            !chart_version.is_empty() && !app_version.is_empty(),
            "read empty versions from Chart.yaml — the assertions below would be vacuous"
        );

        for (needle, what) in [
            (format!("| Images | `{app_version}` |"), "the images row"),
            (
                format!("| Chart | `flint-lite-operator` `{chart_version}` |"),
                "the chart row",
            ),
            (format!("--version {chart_version} \\"), "the helm install pin"),
        ] {
            assert!(
                GUIDE_MD.contains(&needle),
                "docs/flint-lite-for-agent-fleets.md is stale: {what} does not say `{needle}`.\n\
                 The chart is version {chart_version} / appVersion {app_version}. A reader \
                 copy-pasting this guide would install the wrong operator.\n\
                 Fix the .md AND the .html, then re-render the .pdf from the html."
            );
        }

        for (needle, what) in [
            (
                format!("<span><b>IMAGES</b> {app_version}</span>"),
                "the html header images pin",
            ),
            (
                format!("<span><b>CHART</b> flint-lite-operator {chart_version}</span>"),
                "the html header chart pin",
            ),
            (
                format!("operator chart {chart_version} · images {app_version}"),
                "the html footer",
            ),
            // The line a reader actually COPIES. The .md's copy of this
            // was gated from the start and the .html's was not, so the
            // 1.42.0 bump left the html telling readers to install
            // 0.2.9 while the header above it said 0.2.10 — caught by
            // eye, not by this test. That is the same miss the doc
            // drill made: proving the PROCEDURE works while the stated
            // NUMBERS drift. Both copies are gated now.
            (
                format!("--version {chart_version}</span>"),
                "the html helm install pin",
            ),
        ] {
            assert!(
                GUIDE_HTML.contains(&needle),
                "docs/flint-lite-for-agent-fleets.html is stale: {what} does not say \
                 `{needle}`. Re-render the .pdf from the html once fixed."
            );
        }
    }
}
