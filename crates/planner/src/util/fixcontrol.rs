//! go-file: pkg/planner/util/fixcontrol/get.go
//!
//! Go package `pkg/planner/util/fixcontrol` (block D09.13): the `tidb_opt_fix_control`
//! switches. So far the boolean getters and the switch the point get fast path reads
//! (VS1-11); the other getters and switch numbers come with their users.

#![allow(non_upper_case_globals)]

use std::collections::HashMap;

/// Fix52592 controls whether to disallow fast path for Get and BatchGet.
pub const Fix52592: u64 = 52592;

/// GetBool fetches the given key from the fix control map as a boolean type.
///
/// Go's nil map is an empty one here (the session's map is never nil-observable).
// go: GetBool L1
pub fn get_bool(fix_control_map: &HashMap<u64, String>, key: u64) -> (bool, bool) {
    let Some(raw_value) = fix_control_map.get(&key) else {
        return (false, false);
    };
    // The same as TiDBOptOn in sessionctx/variable.
    let value = tidb_base::gostd::equal_fold(raw_value, "ON") || raw_value == "1";
    (value, true)
}

/// GetBoolWithDefault fetches the given key from the fix control map as a boolean type,
/// and a default value would be returned when fail to fetch the expected key.
// go: GetBoolWithDefault L1
pub fn get_bool_with_default(
    fix_control_map: &HashMap<u64, String>,
    key: u64,
    default_val: bool,
) -> bool {
    let (value, exists) = get_bool(fix_control_map, key);
    if !exists {
        return default_val;
    }
    value
}
