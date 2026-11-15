//! go-file: pkg/planner/util/fixcontrol/get.go
//!
//! Part of `fixcontrol` (block D09.13): the boolean lookups that ranger (D10.4) reads,
//! ported early under constitution §4.5 (no outgoing edges) and counted for D09.13. The
//! other getters and the full list of fix numbers come with D09.13; only the numbers
//! ranger and the point get fast path read are here.

// Go constant names (M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashMap;

/// Fix44389 controls whether to consider non-point ranges of some CNF item when building ranges.
pub const Fix44389: u64 = 44389;
/// Fix54337 controls whether to apply or not range intersection for index access.
pub const Fix54337: u64 = 54337;
/// Fix52592 controls whether to disallow fast path for Get and BatchGet (the point get fast
/// path, D10.8).
pub const Fix52592: u64 = 52592;

/// GetBool fetches the given key from the fix control map as a boolean type.
///
/// Go's nil map is an empty map here (both have no keys).
// go: GetBool L1
pub fn get_bool(fix_control_map: &HashMap<u64, String>, key: u64) -> (bool, bool) {
    let Some(raw_value) = fix_control_map.get(&key) else {
        return (false, false);
    };
    // The same as TiDBOptOn in sessionctx/variable.
    let value = raw_value.eq_ignore_ascii_case("ON") || raw_value == "1";
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
