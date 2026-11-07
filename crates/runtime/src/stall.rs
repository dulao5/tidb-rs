//! Stall classification (PR #9 review): why a poll ran past the stall threshold.
//!
//! The stall detector times polls with the wall clock, so on an oversubscribed machine a
//! worker taken off its core in the middle of a poll looks like a stall, although the code did
//! nothing wrong. With classification on, each poll also reads the thread's CPU time (and, on
//! Linux, its context-switch counts) before and after; a stall is then:
//!
//! | cause | meaning | N3 §5 |
//! |---|---|---|
//! | `on_cpu` | the thread was running for most of the poll: a loop missing a checkpoint, or heavy synchronous work | must be 0 |
//! | `blocked` | it gave up the CPU voluntarily during the poll (blocking syscall, sleeping on a lock; M-BLOCK / M-LOCK) | must be 0 |
//! | `preempted` | it was mostly off-CPU and only switched involuntarily: the OS scheduler, i.e. machine load | reported |
//! | `off_cpu` | mostly off-CPU, cause unknown: the platform gives no per-thread switch counts (macOS) | reported |
//! | `unclassified` | classification is off | reported |
//!
//! "Most of the poll" is CPU time at least half of the wall time.
//!
//! Cost: one `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` (plus, on Linux, one
//! `getrusage(RUSAGE_THREAD)`) before every poll, and one more of each after a stalled poll:
//! ~200 ns on macOS, ~400 ns on Linux per poll, a few % of a point get's ~10 µs of polls. So it
//! is off by default; tests and benchmarks switch it on with [`SliceConfig::classify_stalls`]
//! (crate::SliceConfig) or the environment variable `TIDB_RS_STALL_CLASSIFY=1`.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Why a poll stalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallCause {
    OnCpu,
    Blocked,
    Preempted,
    OffCpu,
    Unclassified,
}

impl StallCause {
    pub const ALL: [StallCause; 5] = [
        StallCause::OnCpu,
        StallCause::Blocked,
        StallCause::Preempted,
        StallCause::OffCpu,
        StallCause::Unclassified,
    ];

    // glue: metric label
    pub fn as_str(self) -> &'static str {
        match self {
            StallCause::OnCpu => "on_cpu",
            StallCause::Blocked => "blocked",
            StallCause::Preempted => "preempted",
            StallCause::OffCpu => "off_cpu",
            StallCause::Unclassified => "unclassified",
        }
    }

    /// Stalls of this cause fail N3 §5 (the code's fault, not the machine's).
    // glue: N3 §5 acceptance
    pub fn is_code(self) -> bool {
        matches!(self, StallCause::OnCpu | StallCause::Blocked)
    }

    // glue: index into the counters
    fn idx(self) -> usize {
        self as usize
    }
}

/// The calling thread's CPU time and context switches at one instant.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ThreadSample {
    cpu: Duration,
    /// Voluntary / involuntary context switches; `None` where the platform has no per-thread
    /// counts.
    switches: Option<(i64, i64)>,
}

/// Sample the calling thread; `None` if the clock is unavailable.
// glue: stall classification
pub(crate) fn sample() -> Option<ThreadSample> {
    let ts = nix::time::clock_gettime(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID).ok()?;
    let cpu = Duration::new(
        u64::try_from(ts.tv_sec()).unwrap_or(0),
        u32::try_from(ts.tv_nsec()).unwrap_or(0),
    );
    Some(ThreadSample {
        cpu,
        switches: switches(),
    })
}

#[cfg(target_os = "linux")]
// glue: stall classification (Linux per-thread rusage)
fn switches() -> Option<(i64, i64)> {
    let r = nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_THREAD).ok()?;
    Some((
        r.voluntary_context_switches(),
        r.involuntary_context_switches(),
    ))
}

#[cfg(not(target_os = "linux"))]
// glue: no per-thread context-switch counts on this platform
fn switches() -> Option<(i64, i64)> {
    None
}

/// Classify a stall of `wall` between two samples of the same thread.
// glue: stall classification
pub(crate) fn classify(wall: Duration, before: &ThreadSample, after: &ThreadSample) -> StallCause {
    let cpu = after.cpu.saturating_sub(before.cpu);
    if cpu * 2 >= wall {
        return StallCause::OnCpu;
    }
    match (before.switches, after.switches) {
        (Some((v0, _)), Some((v1, _))) if v1 > v0 => StallCause::Blocked,
        (Some((_, i0)), Some((_, i1))) if i1 > i0 => StallCause::Preempted,
        // Off-CPU with no switch recorded (or no counts at all): cannot tell why.
        _ => StallCause::OffCpu,
    }
}

static BY_CAUSE: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

// glue: per-cause counters
pub(crate) fn count(c: StallCause) {
    BY_CAUSE[c.idx()].fetch_add(1, Ordering::Relaxed);
}

/// Process-wide stalls of cause `c` so far.
// glue: per-cause counters
pub fn stall_count_by(c: StallCause) -> u64 {
    BY_CAUSE[c.idx()].load(Ordering::Relaxed)
}

/// Whether `TIDB_RS_STALL_CLASSIFY` asks for classification (`1`, `true`, `on`).
// glue: configuration
pub(crate) fn enabled_by_env() -> bool {
    std::env::var("TIDB_RS_STALL_CLASSIFY")
        .map(|v| matches!(v.as_str(), "1" | "true" | "on"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(cpu_ms: u64, sw: Option<(i64, i64)>) -> ThreadSample {
        ThreadSample {
            cpu: Duration::from_millis(cpu_ms),
            switches: sw,
        }
    }

    #[test]
    fn classify_causes() {
        let wall = Duration::from_millis(20);
        // Ran 15 of 20 ms: on CPU, whatever the switches.
        assert_eq!(
            classify(wall, &s(0, Some((0, 0))), &s(15, Some((0, 1)))),
            StallCause::OnCpu
        );
        // Off CPU, a voluntary switch: blocked (even with involuntary ones too).
        assert_eq!(
            classify(wall, &s(0, Some((3, 0))), &s(1, Some((4, 2)))),
            StallCause::Blocked
        );
        // Off CPU, only involuntary: preempted.
        assert_eq!(
            classify(wall, &s(0, Some((3, 0))), &s(1, Some((3, 1)))),
            StallCause::Preempted
        );
        // No counts (macOS): off CPU, cause unknown.
        assert_eq!(classify(wall, &s(0, None), &s(1, None)), StallCause::OffCpu);
    }

    #[test]
    fn sample_measures_thread_cpu() {
        let a = sample().expect("thread CPU clock");
        let until = std::time::Instant::now() + Duration::from_millis(20);
        while std::time::Instant::now() < until {
            std::hint::spin_loop();
        }
        let b = sample().unwrap();
        assert!(b.cpu.saturating_sub(a.cpu) >= Duration::from_millis(10));
        let c = sample().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let d = sample().unwrap();
        assert!(d.cpu.saturating_sub(c.cpu) < Duration::from_millis(5));
    }
}
