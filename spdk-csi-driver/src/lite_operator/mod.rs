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
pub mod noderoll;
pub mod persistence;
pub mod reconcile;
pub mod render;

/// The backoff for the controller's watch streams (`Controller::trigger_backoff`).
///
/// kube's Controller puts ONE backoff over all its watches, so any watch
/// error pauses every watch. Its default (0.8 s doubling to 30 s, reset
/// after 120 s quiet) made an apiserver restart at 10,000 shares cost ~4
/// minutes of deafness: six 410s drained one at a time behind it. This one
/// starts at 100 ms, doubles to a 5 s cap, and resets after a quiet minute:
/// six 410s cost seconds, and while the apiserver is down the operator's
/// watches still retry only every few seconds.
pub fn trigger_backoff() -> kube::runtime::utils::ResetTimerBackoff<TriggerBackoff> {
    kube::runtime::utils::ResetTimerBackoff::new(TriggerBackoff::new(), std::time::Duration::from_secs(60))
}

/// Exponential, 100 ms doubling to 5 s; each delay drawn from [d/2, d].
pub struct TriggerBackoff {
    next: std::time::Duration,
}

impl TriggerBackoff {
    const MIN: std::time::Duration = std::time::Duration::from_millis(100);
    const MAX: std::time::Duration = std::time::Duration::from_secs(5);

    fn new() -> Self {
        Self { next: Self::MIN }
    }
}

impl Iterator for TriggerBackoff {
    type Item = std::time::Duration;

    fn next(&mut self) -> Option<std::time::Duration> {
        use rand::Rng;
        let d = self.next;
        self.next = (d * 2).min(Self::MAX);
        Some(d.mul_f64(rand::thread_rng().gen_range(0.5..=1.0)))
    }
}

impl kube::runtime::utils::Backoff for TriggerBackoff {
    fn reset(&mut self) {
        self.next = Self::MIN;
    }
}

/// An error and every source under it, as one log line. kube's
/// `controller::Error::QueueError` displays as just "event queue error";
/// the watcher failure under it (a 410 that forces a relist, a 403) is
/// only in the chain. A source whose text the line already holds is
/// skipped: thiserror types often repeat their source in their own text.
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut line = e.to_string();
    let mut next = e.source();
    while let Some(s) = next {
        let t = s.to_string();
        if !line.contains(&t) {
            line.push_str(": ");
            line.push_str(&t);
        }
        next = s.source();
    }
    line
}

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

/// kube's `controller::Error::QueueError` displays as "event queue error"
/// and nothing more: the watcher error under it (a 410 "too old resource
/// version" that forced a relist, a 403) lives only in its `source()`.
/// The operator logged `{e}`, so every watch failure read the same, and
/// the step 6 rig could not tell a relist happened at 10,000 shares.
#[cfg(test)]
mod error_chain_tests {
    use super::error_chain;
    use kube::core::Status;
    use kube::runtime::{controller, watcher};

    #[test]
    fn a_410_under_a_queue_error_reaches_the_log_line() {
        let gone = Status {
            code: 410,
            message: "too old resource version: 80353 (80433)".into(),
            reason: "Expired".into(),
            ..Default::default()
        };
        let e: controller::Error<std::io::Error, watcher::Error> =
            controller::Error::QueueError(watcher::Error::WatchError(gone.boxed()));
        assert_eq!(e.to_string(), "event queue error", "the control: Display alone hides it");
        let line = error_chain(&e);
        assert!(line.starts_with("event queue error: "), "{line}");
        assert!(line.contains("too old resource version: 80353 (80433)"), "{line}");
        assert_eq!(line.matches("too old resource version").count(), 1, "no repeats: {line}");
    }
}

/// kube's Controller puts ONE backoff over all its watches (`StreamBackoff`
/// over the merged trigger streams): any watch error pauses EVERY watch.
/// With kube's default (0.8 s doubling to 30 s, reset only after 120 s
/// quiet) an apiserver restart at 10,000 shares cost ~4 minutes: the
/// connection errors during the outage pushed it to the cap, then six 410s
/// drained one at a time, ~35-45 s apiece, and no re-list could start until
/// the last one had. A watch that had resumed cleanly aged out while unread
/// and took its own 410 (step 6, `results-box-step6/D-relist-10k.txt`).
#[cfg(test)]
mod trigger_backoff_tests {
    use super::trigger_backoff;
    use std::time::Duration;

    /// Delays the controller sleeps for: `outage` failed retries while the
    /// apiserver is down, then the six 410s it answers with once it is back.
    fn after_outage(mut b: impl Iterator<Item = Duration>, outage: usize) -> (Vec<Duration>, Duration) {
        let during: Vec<Duration> = (&mut b).take(outage).collect();
        let six: Duration = b.take(6).sum();
        (during, six)
    }

    #[test]
    fn six_410s_after_an_apiserver_restart_cost_seconds_not_minutes() {
        // The control: kube's default spends minutes on them.
        let (_, kube_six) = after_outage(kube::runtime::watcher::DefaultBackoff::default(), 20);
        assert!(kube_six > Duration::from_secs(150), "kube default: {kube_six:?}");

        let (during, six) = after_outage(trigger_backoff(), 20);
        assert!(six <= Duration::from_secs(30), "six 410s cost {six:?}");
        assert!(during.iter().all(|d| *d <= Duration::from_secs(5)), "a delay above the cap: {during:?}");
        // Not a hot loop while the apiserver is down: by the tenth retry it
        // waits at least half the cap.
        assert!(during[10..].iter().all(|d| *d >= Duration::from_millis(2500)), "{during:?}");
    }

    #[test]
    fn the_first_retry_is_quick() {
        let first = trigger_backoff().next().unwrap();
        assert!(first <= Duration::from_millis(100), "{first:?}");
    }
}
