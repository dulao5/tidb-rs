//! go-file: pkg/server/server.go
//!
//! What the server start sets from the cluster before it accepts connections (part 2 of the
//! server block D03.1 ports the rest of this file).

/// Copies the system time zone loaded from `mysql.tidb` into the instance variable
/// `system_time_zone`, or keeps its default ("CST") and logs why.
// go: setSystemTimeZoneVariable L2 (Go's sync.Once: the server start calls it once)
pub fn set_system_time_zone_variable() {
    match tidb_base::timeutil::get_system_tz() {
        Err(err) => {
            let default = tidb_sessionctx::variable::get_sys_var("system_time_zone")
                .map(|v| v.value.clone())
                .unwrap_or_default();
            tidb_base::log_error!(
                "Error getting SystemTZ, use default value instead",
                "error" => err.as_str(),
                "default system_time_zone" => default.as_str(),
            );
        }
        Ok(tz) => tidb_sessionctx::variable::set_sys_var("system_time_zone", &tz),
    }
}
