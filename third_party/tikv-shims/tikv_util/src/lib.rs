//! Shim: minimal subset of TiKV `tikv_util` needed by the tidb_query_* crates.
//! `buffer_vec`, `codec::{bytes,number}`, `macros`, `log` are copied verbatim
//! from tikv/components/tikv_util; everything else is a thin re-implementation.

#[macro_use]
pub mod log;
#[macro_use]
pub mod macros;
pub mod buffer_vec;
pub mod codec;

pub use either::Either;

pub fn escape(data: &[u8]) -> String {
    let mut escaped = Vec::with_capacity(data.len() * 4);
    for &c in data {
        match c {
            b'\n' => escaped.extend_from_slice(br"\n"),
            b'\r' => escaped.extend_from_slice(br"\r"),
            b'\t' => escaped.extend_from_slice(br"\t"),
            b'"' => escaped.extend_from_slice(b"\\\""),
            b'\\' => escaped.extend_from_slice(br"\\"),
            _ => {
                if (0x20..0x7f).contains(&c) {
                    escaped.push(c);
                } else {
                    escaped.push(b'\\');
                    escaped.push(b'0' + (c >> 6));
                    escaped.push(b'0' + ((c >> 3) & 7));
                    escaped.push(b'0' + (c & 7));
                }
            }
        }
    }
    unsafe { String::from_utf8_unchecked(escaped) }
}

#[inline]
pub fn is_even(n: usize) -> bool {
    n & 1 == 0
}

pub mod time {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Wrapper over std Instant (TiKV's version supports a coarse clock).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub struct Instant(std::time::Instant);
    impl Instant {
        pub fn now() -> Self { Instant(std::time::Instant::now()) }
        pub fn now_coarse() -> Self { Self::now() }
        pub fn saturating_elapsed(&self) -> Duration { self.0.elapsed() }
        pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
            self.0.saturating_duration_since(earlier.0)
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub struct Timespec { pub sec: i64, pub nsec: i32 }
    impl Timespec { pub fn new(sec: i64, nsec: i32) -> Self { Timespec { sec, nsec } } }

    pub fn get_time() -> Timespec {
        let dur = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        Timespec::new(dur.as_secs() as i64, dur.subsec_nanos() as i32)
    }

    pub fn duration_to_ns(d: Duration) -> u64 {
        d.as_secs() * 1_000_000_000 + u64::from(d.subsec_nanos())
    }
}

pub mod deadline {
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct DeadlineError;
    impl std::fmt::Display for DeadlineError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("deadline exceeded") }
    }
    impl std::error::Error for DeadlineError {}
}
