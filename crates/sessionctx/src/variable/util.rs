//! Helpers from other Go packages that the validation hooks call and that no block has
//! ported yet (pkg/util/timeutil, pkg/util, pkg/util/servicescope; files of D00.3).

#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::dbterror::ClassVariable;
use tidb_base::errno;
use tidb_base::gostd;
use tidb_base::gostd::time::Location;

/// timeutil.ErrUnknownTimeZone.
static ErrUnknownTimeZone: std::sync::LazyLock<Error> =
    std::sync::LazyLock::new(|| ClassVariable.new_std(errno::ErrUnknownTimeZone));

/// ParseTimeZone parses the time zone string, returns the location and whether the time zone is valid.
// glue: port of pkg/util/timeutil.ParseTimeZone (D00.3 file time_zone.go, not yet ported)
#[allow(non_upper_case_globals)]
pub(crate) fn parse_time_zone(s: &str) -> Result<Arc<Location>, Error> {
    if gostd::equal_fold(s, "SYSTEM") {
        return Ok(gostd::time::local()); // timeutil.SystemLocation()
    }
    if let Ok(loc) = gostd::time::load_location(s) {
        return Ok(loc);
    }
    // The value can be given as a string indicating an offset from UTC, such as '+10:00' or '-6:00'.
    // The time zone's value should in [-12:59,+14:00].
    if s.starts_with('+') || s.starts_with('-') {
        let ctx = &*tidb_types::context::DefaultStmtNoWarningContext;
        let (d, _, err) = tidb_types::time::parse_duration(ctx, &s.as_bytes()[1..], 0);
        if err.is_ok() {
            const HOUR: i64 = 3_600_000_000_000;
            const MINUTE: i64 = 60_000_000_000;
            if s.as_bytes()[0] == b'-' {
                if d.duration > 12 * HOUR + 59 * MINUTE {
                    return Err(ErrUnknownTimeZone.gen_with_stack_by_args(vec![s.to_owned().into()]));
                }
            } else if d.duration > 14 * HOUR {
                return Err(ErrUnknownTimeZone.gen_with_stack_by_args(vec![s.to_owned().into()]));
            }
            let mut ofst = (d.duration / 1_000_000_000) as i32;
            if s.as_bytes()[0] == b'-' {
                ofst = -ofst;
            }
            return Ok(gostd::time::fixed_zone("", ofst));
        }
    }
    Err(ErrUnknownTimeZone.gen_with_stack_by_args(vec![s.to_owned().into()]))
}

/// IsInCorrectIdentifierName checks if the identifier is incorrect.
/// See https://dev.mysql.com/doc/refman/5.7/en/identifiers.html
// glue: port of pkg/util.IsInCorrectIdentifierName (D00.3 file util.go, not yet ported)
pub(crate) fn is_in_correct_identifier_name(name: &str) -> bool {
    if name.is_empty() {
        return true;
    }
    if name.as_bytes()[name.len() - 1] == b' ' {
        return true;
    }
    false
}

/// CheckServiceScope check if the tidb-service-scope set by users is valid
/// (`^[a-zA-Z0-9_-]{0,64}$`).
// glue: port of pkg/util/servicescope.CheckServiceScope (not yet ported)
pub(crate) fn check_service_scope(scope: &str) -> Result<(), Error> {
    let valid = scope.len() <= 64
        && scope.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !valid {
        return Err(tidb_base::errors::new(format!(
            "the tidb-service-scope value '{scope}' is invalid. It must be 64 characters or fewer and consist only of letters (a-z, A-Z), numbers (0-9), hyphens (-), and underscores (_)"
        )));
    }
    Ok(())
}
