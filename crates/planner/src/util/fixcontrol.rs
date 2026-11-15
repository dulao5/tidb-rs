//! go-file: pkg/planner/util/fixcontrol/get.go
//!
//! Part of `fixcontrol` (block D09.13): the boolean lookups that ranger (D10.4) reads,
//! ported early under constitution §4.5 (no outgoing edges) and counted for D09.13. The
//! other getters and the full list of fix numbers come with D09.13; only the numbers
//! ranger, the point get fast path and the plan-cache checks (D13.1) read are here.

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
/// Fix33031 controls whether to disallow plan cache for partitioned
/// tables (both prepared statments and non-prepared statements)
pub const Fix33031: u64 = 33031;
/// Fix44823 controls the maximum number of parameters for a query that can be cached in the Plan Cache.
pub const Fix44823: u64 = 44823;
/// Fix45798 controls whether to cache plans that access generated columns.
pub const Fix45798: u64 = 45798;
/// Fix49736 controls whether to force the optimizer to use plan cache even if there is risky optimization.
/// This fix-control is test-only.
pub const Fix49736: u64 = 49736;

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

/// GetInt fetches the given key from the fix control map as an int64 type: the value, whether
/// the key exists and the parse error (Go's `strconv.ParseInt(rawValue, 10, 64)`).
// go: GetInt L1
pub fn get_int(fix_control_map: &HashMap<u64, String>, key: u64) -> (i64, bool, bool) {
    let Some(raw_value) = fix_control_map.get(&key) else {
        return (0, false, false);
    };
    // The same as TidbOptInt64 in sessionctx/variable.
    let (value, parse_err) = tidb_base::gostd::strconv::parse_int(raw_value.as_bytes(), "ParseInt");
    (value, true, parse_err.is_err())
}

/// GetIntWithDefault fetches the given key from the fix control map as an int64 type,
/// and a default value would be returned when fail to fetch the expected key.
// go: GetIntWithDefault L1
pub fn get_int_with_default(
    fix_control_map: &HashMap<u64, String>,
    key: u64,
    default_val: i64,
) -> i64 {
    let (value, exists, parse_err) = get_int(fix_control_map, key);
    if !exists || parse_err {
        return default_val;
    }
    value
}
