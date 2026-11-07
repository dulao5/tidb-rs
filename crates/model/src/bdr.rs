//! go-file: pkg/meta/model/bdr.go

use tidb_base::gostd::time::{Time, local, unix};

/// TSConvert2Time converts timestamp to time: the physical part (the bits above the 18
/// logical bits) in milliseconds, in the local time zone (`time.UnixMilli`).
// go: TSConvert2Time L1
pub fn ts_convert2_time(ts: u64) -> Time {
    let t = (ts >> 18) as i64; // 18 is for the logical time.
    let (sec, msec) = (t.div_euclid(1000), t.rem_euclid(1000));
    unix(sec, msec * 1_000_000).in_loc(&local())
}
