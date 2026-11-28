//! go-file: pkg/util/timeutil/time_zone.go
//!
//! Timezone helper for DAG request construction (D00.3, carried with D12.1 Part 2A).

use std::sync::{Arc, Once, RwLock};

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

/// systemTZ is current TiDB's system timezone name. Go's `init` stores "System" while it is
/// still empty: an empty value reads as "System" here ([`system_tz_load`]).
static SYSTEM_TZ: RwLock<String> = RwLock::new(String::new());

/// `systemTZ.Load()`, with the "System" Go's `init` stores.
// glue: systemTZ.Load() after the systemTZ part of time_zone.go's init
fn system_tz_load() -> String {
    let tz = SYSTEM_TZ.read().unwrap_or_else(|e| e.into_inner());
    if tz.is_empty() {
        "System".to_owned()
    } else {
        tz.clone()
    }
}

static SET_SYS_TZ_ONCE: Once = Once::new();

/// SetSystemTZ sets systemTZ by the value loaded from mysql.tidb.
// go: SetSystemTZ L1
pub fn set_system_tz(name: &str) {
    SET_SYS_TZ_ONCE.call_once(|| {
        *SYSTEM_TZ.write().unwrap_or_else(|e| e.into_inner()) = name.to_owned();
    });
}

/// GetSystemTZ gets the value of systemTZ, an error is returned if systemTZ is not properly set.
// go: GetSystemTZ L1
pub fn get_system_tz() -> Result<String, String> {
    let system_tz = system_tz_load();
    if system_tz == "System" || system_tz.is_empty() {
        return Err("variable `systemTZ` is not properly set".to_owned());
    }
    Ok(system_tz)
}

#[cfg(test)]
mod system_tz_tests {
    use super::*;

    #[test]
    fn set_once_then_get() {
        assert_eq!(
            get_system_tz(),
            Err("variable `systemTZ` is not properly set".to_owned())
        );
        set_system_tz("Asia/Tokyo");
        set_system_tz("UTC");
        assert_eq!(get_system_tz(), Ok("Asia/Tokyo".to_owned()));
    }
}
