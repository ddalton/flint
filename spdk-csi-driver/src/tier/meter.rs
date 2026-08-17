//! The tier meter — L2 step 4 seeds it, every later step grows it
//! (design review A12).
//!
//! DataPathMeter-style: cheap relaxed atomics at the hot sites, a
//! snapshot for tests/observability now, the silent-when-idle reporter
//! arrives with the flusher (step 5) when there is activity worth
//! reporting. The A12 gauges that need state the tier does not have
//! yet (dirty backlog vs PVC headroom, oldest-unflushed age, epoch
//! lease age) land with the steps that create that state.

use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! counters {
    ($($(#[$doc:meta])* $name:ident),+ $(,)?) => {
        $(
            $(#[$doc])*
            #[allow(non_upper_case_globals)] // named for their snapshot fields
            static $name: AtomicU64 = AtomicU64::new(0);
        )+

        /// One consistent-enough view (relaxed loads; the meter is
        /// telemetry, not arbitration).
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct MeterSnapshot {
            $( pub $name: u64, )+
        }

        pub fn snapshot() -> MeterSnapshot {
            MeterSnapshot {
                $( $name: $name.load(Ordering::Relaxed), )+
            }
        }
    };
}

counters! {
    /// Generations published (put_whole or compose Complete landed).
    publishes,
    /// Publish attempts that failed for any reason.
    publish_failures,
    /// 412s on the publish path (arbitration inputs, NOT operator
    /// events — A6).
    publish_412s,
    /// Arbitration verdicts by flavor.
    arbitrate_adopt_own,
    arbitrate_retry_from_base,
    arbitrate_foreign,
    /// Multipart mechanics.
    parts_uploaded,
    parts_copied,
    bytes_uploaded,
    bytes_copied,
    mpu_aborts,
}

#[inline]
pub fn add(counter: Counter, n: u64) {
    counter.cell().fetch_add(n, Ordering::Relaxed);
}

#[inline]
pub fn bump(counter: Counter) {
    add(counter, 1);
}

/// Named handles so call sites read as English and the statics stay
/// private.
#[derive(Debug, Clone, Copy)]
pub enum Counter {
    Publishes,
    PublishFailures,
    Publish412s,
    ArbitrateAdoptOwn,
    ArbitrateRetryFromBase,
    ArbitrateForeign,
    PartsUploaded,
    PartsCopied,
    BytesUploaded,
    BytesCopied,
    MpuAborts,
}

impl Counter {
    fn cell(self) -> &'static AtomicU64 {
        match self {
            Counter::Publishes => &publishes,
            Counter::PublishFailures => &publish_failures,
            Counter::Publish412s => &publish_412s,
            Counter::ArbitrateAdoptOwn => &arbitrate_adopt_own,
            Counter::ArbitrateRetryFromBase => &arbitrate_retry_from_base,
            Counter::ArbitrateForeign => &arbitrate_foreign,
            Counter::PartsUploaded => &parts_uploaded,
            Counter::PartsCopied => &parts_copied,
            Counter::BytesUploaded => &bytes_uploaded,
            Counter::BytesCopied => &bytes_copied,
            Counter::MpuAborts => &mpu_aborts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_into_the_snapshot() {
        // Process-global (parallel tests share them): assert deltas.
        let before = snapshot();
        bump(Counter::Publishes);
        add(Counter::BytesUploaded, 4096);
        bump(Counter::ArbitrateAdoptOwn);
        let after = snapshot();
        assert!(after.publishes >= before.publishes + 1);
        assert!(after.bytes_uploaded >= before.bytes_uploaded + 4096);
        assert!(after.arbitrate_adopt_own >= before.arbitrate_adopt_own + 1);
    }
}
