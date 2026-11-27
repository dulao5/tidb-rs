//! go-file: pkg/util/timeutil/time_zone.go
//!
//! Timezone helper for DAG request construction (D00.3, carried with D12.1 Part 2A).

use std::sync::Arc;

use crate::gostd::time::{self, Location};

/// Zone returns the current timezone name and timezone offset in seconds.
/// In compatible with MySQL, we change `SystemLocation` to `System`.
// go: Zone L1
pub fn zone(loc: &Arc<Location>) -> (String, i64) {
    let (_, offset) = time::now().in_loc(loc).zone();
    let mut name = loc.string().to_owned();
    // when we found name is "System", we have no choice but push down
    // "System" to TiKV side.
    if name == "Local" {
        name = "System".to_owned();
    }

    (name, i64::from(offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_utc_fixed_and_local() {
        assert_eq!(zone(&time::utc()), ("UTC".to_owned(), 0));
        let fixed = time::fixed_zone("", 8 * 3600);
        assert_eq!(zone(&fixed), (String::new(), 8 * 3600));
        let named = time::fixed_zone("Asia/Shanghai", 8 * 3600);
        assert_eq!(zone(&named), ("Asia/Shanghai".to_owned(), 8 * 3600));
        let local_loc = time::local();
        let (local_name, local_offset) = zone(&local_loc);
        let (_, expected_offset) = time::now().in_loc(&local_loc).zone();
        assert_eq!(local_offset, i64::from(expected_offset));
        if local_loc.string() == "Local" {
            assert_eq!(local_name, "System");
        } else {
            assert_eq!(local_name, local_loc.string());
        }
    }
}
