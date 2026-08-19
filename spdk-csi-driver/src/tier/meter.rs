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

        impl MeterSnapshot {
            /// Counter deltas since `prev` (monotonic counters;
            /// saturating_sub only papers over restarts). The A12
            /// reporter keys its silence on this.
            pub fn delta_since(&self, prev: &MeterSnapshot) -> MeterSnapshot {
                MeterSnapshot {
                    $( $name: self.$name.saturating_sub(prev.$name), )+
                }
            }

            pub fn is_zero(&self) -> bool {
                *self == MeterSnapshot::default()
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
    /// Flush scheduling (step 5, the A11 knobs at work). The skips are
    /// the economics gate's request-cost model made observable.
    flushes_skipped_floor,
    flushes_skipped_quiesce,
    flushes_skipped_inflight,
    /// Bucket content already matched local truth (CRC) — adopted
    /// clean with zero upload (the restart clean-skip).
    flushes_clean_match,
    /// The epoch machinery (step 7, A8).
    epoch_renews,
    epoch_renew_failures,
    /// Foreign holders judged dead and superseded (NOT self-recognition
    /// resumes — those are routine restarts).
    epoch_takeovers,
    /// In-flight assemblies aborted by a claim's fence sweep.
    takeover_mpu_aborts,
    /// Publishes refused because the guard was fenced (or re-verified
    /// fenced mid-flush).
    flushes_fenced,
    /// A10 backpressure: WRITE/ALLOCATE/COPY/CLONE refused NOSPC while
    /// headroom-minus-reserve was exhausted.
    nospc_write_refusals,
    /// A10 backpressure: OPEN-create/CREATE refused NOSPC.
    nospc_create_refusals,
    /// The state.db ballast was released at critical fullness.
    ballast_releases,
    /// Eviction (step 10, A12's refusals-by-reason).
    files_evicted,
    bytes_evicted,
    /// Refused: dirty in any form (captured/queued/durable bit).
    evict_refused_dirty,
    /// Refused: policy — no generation, not CRC-eligible, re-key
    /// pending, open writers.
    evict_refused_policy,
    /// Refused: verification — object missing/foreign, or local bytes
    /// diverge from the published CRC.
    evict_refused_verify,
    /// Content ops answered NFS4ERR_DELAY because the file is evicted
    /// (step 11 turns these into hydrations).
    evicted_op_delays,
    /// Hydration (step 11).
    hydrations_started,
    hydrations_completed,
    hydration_failures,
    hydration_bytes,
    /// A 412 on a ranged restore GET — S3-wins foreign adopts.
    hydration_foreign_adopts,
    /// Files evicted by the watermark pass (vs. drills/manual).
    auto_evictions,
    /// Step 12 (A12): DR manifests written at flush barriers (unchanged
    /// content skips — a write here means the RPO advanced).
    manifest_writes,
    /// Manifest barrier writes that failed (the RPO record is stale by
    /// one barrier; retried next tick).
    manifest_failures,
    /// Step 12 (A7): import-refresh — evicted stubs materialized from
    /// bucket objects.
    import_stubs,
    /// Keys the import REFUSED to re-ingest because their tombstone has
    /// not flushed (the resurrection guard).
    import_skipped_tombstoned,
    /// Wall-clock milliseconds of COMPLETED restores (A12 reporter:
    /// delta ÷ hydrations_completed delta = average restore latency).
    hydration_millis,
    /// Warm fill (bulk hydration): items dropped at the space bound
    /// (watermark − margin, pending bytes counted) — the fill stops
    /// rather than fight eviction.
    warm_skipped_space,
    /// Warm fill: items abandoned after WARM_MAX_ATTEMPTS failed
    /// restores (a demand touch will still retry them).
    warm_abandoned,
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
    FlushesSkippedFloor,
    FlushesSkippedQuiesce,
    FlushesSkippedInflight,
    FlushesCleanMatch,
    EpochRenews,
    EpochRenewFailures,
    EpochTakeovers,
    TakeoverMpuAborts,
    FlushesFenced,
    NospcWriteRefusals,
    NospcCreateRefusals,
    BallastReleases,
    FilesEvicted,
    BytesEvicted,
    EvictRefusedDirty,
    EvictRefusedPolicy,
    EvictRefusedVerify,
    EvictedOpDelays,
    HydrationsStarted,
    HydrationsCompleted,
    HydrationFailures,
    HydrationBytes,
    HydrationForeignAdopts,
    AutoEvictions,
    ManifestWrites,
    ManifestFailures,
    ImportStubs,
    ImportSkippedTombstoned,
    HydrationMillis,
    WarmSkippedSpace,
    WarmAbandoned,
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
            Counter::FlushesSkippedFloor => &flushes_skipped_floor,
            Counter::FlushesSkippedQuiesce => &flushes_skipped_quiesce,
            Counter::FlushesSkippedInflight => &flushes_skipped_inflight,
            Counter::FlushesCleanMatch => &flushes_clean_match,
            Counter::EpochRenews => &epoch_renews,
            Counter::EpochRenewFailures => &epoch_renew_failures,
            Counter::EpochTakeovers => &epoch_takeovers,
            Counter::TakeoverMpuAborts => &takeover_mpu_aborts,
            Counter::FlushesFenced => &flushes_fenced,
            Counter::NospcWriteRefusals => &nospc_write_refusals,
            Counter::NospcCreateRefusals => &nospc_create_refusals,
            Counter::BallastReleases => &ballast_releases,
            Counter::FilesEvicted => &files_evicted,
            Counter::BytesEvicted => &bytes_evicted,
            Counter::EvictRefusedDirty => &evict_refused_dirty,
            Counter::EvictRefusedPolicy => &evict_refused_policy,
            Counter::EvictRefusedVerify => &evict_refused_verify,
            Counter::EvictedOpDelays => &evicted_op_delays,
            Counter::HydrationsStarted => &hydrations_started,
            Counter::HydrationsCompleted => &hydrations_completed,
            Counter::HydrationFailures => &hydration_failures,
            Counter::HydrationBytes => &hydration_bytes,
            Counter::HydrationForeignAdopts => &hydration_foreign_adopts,
            Counter::AutoEvictions => &auto_evictions,
            Counter::ManifestWrites => &manifest_writes,
            Counter::ManifestFailures => &manifest_failures,
            Counter::ImportStubs => &import_stubs,
            Counter::ImportSkippedTombstoned => &import_skipped_tombstoned,
            Counter::HydrationMillis => &hydration_millis,
            Counter::WarmSkippedSpace => &warm_skipped_space,
            Counter::WarmAbandoned => &warm_abandoned,
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
