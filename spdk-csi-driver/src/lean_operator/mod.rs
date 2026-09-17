//! flint-lean's control plane: the FlintLeanWorkspace CRD (`crd`), the
//! claim/adopt/refuse reconcile (`reconcile`), the boundary-verb
//! validation (`boundary`), and the syncer environment (`sync_env`) the
//! s3.csi.chert.us node plugin hands to a lean worker. The webhook and the
//! syncer injector are gone: a workspace reaches a pod as ONE `csi:`
//! volume (docs/plans/csi-node-mount-design.md §3.5, §5).

pub mod boundary;
pub mod crd;
pub mod reconcile;
pub mod sync_env;

/// The lean guide's version pins must be the ones we actually ship.
///
/// The mirror of `lite_operator::guide_pins`, and it exists because the
/// lean guide had no such guard and rotted exactly the way that one's
/// comment predicted. At v1.56.0 the guide's PROSE said "`flint-lean`
/// 0.13.0 and `flint-s3-csi` 0.3.2" while the commands three lines below
/// it — the lines a reader COPIES — still said 0.12.0 and 0.3.1, and the
/// `.html` prose said 0.12.0/0.3.1 too. A reader following the guide
/// installed the previous release's charts.
///
/// That is not a hypothetical: it is why "the 1.56.0 tag is missing"
/// was reported. `flint-lean` is a CHART repo, so its tags are chart
/// semver (0.13.0) and 1.56.0 is only its `appVersion` — a guide that
/// names the wrong chart version is the one place a reader learns which
/// number to ask for.
///
/// The `.html` is checked with the `.md` because both are published and
/// the `.pdf` renders FROM the html.
#[cfg(test)]
mod guide_pins {
    const LEAN_CHART: &str = include_str!("../../../flint-lean-chart/Chart.yaml");
    const S3_CHART: &str = include_str!("../../../flint-s3-csi-chart/Chart.yaml");
    const GUIDE_MD: &str = include_str!("../../../docs/flint-lean-for-agent-fleets.md");
    const GUIDE_HTML: &str = include_str!("../../../docs/flint-lean-for-agent-fleets.html");

    fn field(chart: &str, which: &str, key: &str) -> String {
        chart
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or_else(|| panic!("{key} missing from {which}/Chart.yaml"))
            .trim()
            .trim_matches('"')
            .to_string()
    }

    #[test]
    fn the_guide_pins_the_charts_and_images_it_documents() {
        let lean = field(LEAN_CHART, "flint-lean-chart", "version:");
        let s3 = field(S3_CHART, "flint-s3-csi-chart", "version:");
        let app = field(LEAN_CHART, "flint-lean-chart", "appVersion:");

        // Guard the guard: empty reads would make every needle below
        // match trivially-shaped strings and prove nothing.
        assert!(
            !lean.is_empty() && !s3.is_empty() && !app.is_empty(),
            "read an empty version from a Chart.yaml — the assertions below would be vacuous"
        );
        // The two charts ship as one release; a guide naming one app
        // version for both is only correct while that is true.
        assert_eq!(
            app,
            field(S3_CHART, "flint-s3-csi-chart", "appVersion:"),
            "flint-lean and flint-s3-csi disagree on appVersion — the guide names ONE \
             images version for both and can no longer be right about both"
        );

        for (needle, what) in [
            (
                format!("dilipdalton/flint-lean --version {lean} \\"),
                "the flint-lean helm install pin",
            ),
            (
                format!("dilipdalton/flint-s3-csi --version {s3} \\"),
                "the flint-s3-csi helm install pin",
            ),
            (
                format!("`flint-lean` {lean} and `flint-s3-csi` {s3} (images {app})"),
                "the prose that states which versions the commands pin",
            ),
        ] {
            assert!(
                GUIDE_MD.contains(&needle),
                "docs/flint-lean-for-agent-fleets.md is stale: {what} does not say \
                 `{needle}`.\nThe charts are flint-lean {lean} / flint-s3-csi {s3}, \
                 appVersion {app}. A reader copy-pasting this guide would install the \
                 wrong release.\nFix the .md AND the .html, then re-render the .pdf."
            );
        }

        for (needle, what) in [
            (
                format!("dilipdalton/flint-lean --version {lean} \\"),
                "the html flint-lean install pin",
            ),
            (
                format!("dilipdalton/flint-s3-csi --version {s3} \\"),
                "the html flint-s3-csi install pin",
            ),
            (
                format!(
                    "<code>flint-lean</code> {lean} and <code>flint-s3-csi</code> {s3} \
                     (images {app})"
                ),
                "the html prose",
            ),
            (
                format!("charts flint-lean {lean} + flint-s3-csi {s3} · images {app}"),
                "the html footer",
            ),
        ] {
            assert!(
                GUIDE_HTML.contains(&needle),
                "docs/flint-lean-for-agent-fleets.html is stale: {what} does not say \
                 `{needle}`. Re-render the .pdf from the html once fixed."
            );
        }
    }
}
