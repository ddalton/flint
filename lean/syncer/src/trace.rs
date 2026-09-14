//! The protocol event trace (`FLINT_SYNC_EVENT_TRACE=1`, or
//! `spec.eventTrace` on a deployed workspace).
//!
//! One JSON line per protocol step — each consume, upload outcome, claim
//! poll, observed-citation re-read, merge, CAS, GC delete, queue write,
//! release, fence, ack and sync — so that when several writers share a
//! workspace and something goes wrong, the interleaving can be rebuilt
//! from the evidence instead of re-run. A race that fails once on a live
//! rig may not fail again. The line format is pinned in
//! `lean/e2e/writers-live/README.md` §2: every line starts `{"ts_ms":`,
//! then `mono_ms`, `holder`, `ev`, then the event's own fields.
//!
//! Off by default. On, it costs a formatted line per step on stderr and,
//! for the holder id, one small local read per event. A writer that has
//! not claimed yet has no lease to name it, so the trace mints (and
//! persists) the pod's incarnation the way the first claim would: without
//! that, a pod's first barrier — its consume, scan and uploads — carried
//! `holder: null` and could not be joined to its own commit.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Where trace lines go.
#[derive(Clone)]
pub enum Sink {
    /// One line per event on stderr, beside the prose log.
    Stderr,
    /// Collected in memory — for tests that read their own trace.
    Memory(Arc<Mutex<Vec<String>>>),
}

static START: OnceLock<Instant> = OnceLock::new();

/// Format one trace line. `fields` must be a JSON object; its keys follow
/// the common prefix in the order serde_json emits them.
pub fn line(holder: Option<&str>, ev: &str, fields: serde_json::Value) -> String {
    let ts_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let mono_ms = START.get_or_init(Instant::now).elapsed().as_millis() as u64;
    let head = format!(
        "{{\"ts_ms\":{ts_ms},\"mono_ms\":{mono_ms},\"holder\":{},\"ev\":{}",
        serde_json::to_string(&holder).unwrap_or_else(|_| "null".into()),
        serde_json::to_string(ev).unwrap_or_else(|_| "\"?\"".into()),
    );
    let rest = match fields {
        serde_json::Value::Object(m) if !m.is_empty() => {
            let body = serde_json::to_string(&serde_json::Value::Object(m)).unwrap_or_default();
            format!(",{}", &body[1..body.len() - 1])
        }
        _ => String::new(),
    };
    format!("{head}{rest}}}")
}

impl super::Syncer {
    /// Emit one trace event, when the trace is on.
    pub fn trace(&self, ev: &str, fields: serde_json::Value) {
        let Some(sink) = &self.cfg.event_trace else { return };
        let holder = self
            .lease
            .as_ref()
            .map(|l| l.holder_id.clone())
            .or_else(|| super::lease::incarnation(self).ok().map(|i| i.holder_id));
        let l = line(holder.as_deref(), ev, fields);
        match sink {
            Sink::Stderr => eprintln!("{l}"),
            Sink::Memory(buf) => buf.lock().unwrap().push(l),
        }
    }

    /// The store's request counts, as a trace field.
    pub fn trace_requests(&self) -> serde_json::Value {
        match self.store.request_counts() {
            Some(c) => serde_json::to_value(c).unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_line_starts_with_the_pinned_prefix_and_parses() {
        let l = super::line(Some("h1"), "gc", serde_json::json!({"path": "a/b", "result": "deleted", "z": 1}));
        assert!(l.starts_with("{\"ts_ms\":"), "{l}");
        let v: serde_json::Value = serde_json::from_str(&l).expect("a trace line is JSON");
        assert_eq!(v["holder"], "h1");
        assert_eq!(v["ev"], "gc");
        assert_eq!(v["path"], "a/b");
        assert!(v["mono_ms"].is_u64());
        let empty = super::line(None, "barrier_start", serde_json::json!({}));
        let v: serde_json::Value = serde_json::from_str(&empty).expect("no fields still parses");
        assert!(v["holder"].is_null());
    }
}
