//! go-file: pkg/sessionctx/variable/variable.go
//!
//! The system variable type, its validation, and the registry (N8 §3.1).
//!
//! Mapping:
//! - The registry is static (N8 §3.1, option c): the static fields of the 944 variables are
//!   generated from Go ([`super::sysvar_data`]), the hooks are hand-written functions
//!   ([`super::hooks`]). Go's `sysVars` map plus `RegisterSysVar` becomes a table built once,
//!   plus the instance overlay of `SetSysVar` (N8 §3.1 item 4, L2).
//! - Go's closures are `fn` pointers. A Go hook returns `(string, error)` and both are used
//!   (`Validate` returns the value next to an error): [`ValResult`] keeps both.
//! - Only `Validation` hooks are ported in D05.1. `SetSession` / `SetGlobal` / `GetSession` /
//!   `GetGlobal` / `GetStateValue` need the session state of block D05.2; the registry records
//!   which of them Go has (`go_hooks`) so that D05.2 can check its table against Go's.

// Go names for constants (M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;

use parking_lot::RwLock;
use tidb_base::Error;
use tidb_base::gofmt::Arg;
use tidb_base::gostd;
use tidb_base::mysql;
use tidb_types::datum::Datum;

use super::error::*;
use super::hooks;
use super::session::SessionVars;
use super::sysvar_data::SYSVAR_DATA;
use super::timeparse;

/// ScopeFlag is for system variable whether can be changed in global/session dynamically or not.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct ScopeFlag(pub u8);

/// TypeFlag is the SysVar type, which doesn't exactly match MySQL types.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct TypeFlag(pub u8);

/// ScopeNone means the system variable can not be changed dynamically.
pub const ScopeNone: ScopeFlag = ScopeFlag(0);
/// ScopeGlobal means the system variable can be changed globally.
pub const ScopeGlobal: ScopeFlag = ScopeFlag(1 << 0);
/// ScopeSession means the system variable can only be changed in current session.
pub const ScopeSession: ScopeFlag = ScopeFlag(1 << 1);
/// ScopeInstance means it is similar to global but doesn't propagate to other TiDB servers.
pub const ScopeInstance: ScopeFlag = ScopeFlag(1 << 2);

// Go declares the type flags in the same const block as the scopes, after four entries, so
// `iota` starts at 4.
/// TypeStr is the default
pub const TypeStr: TypeFlag = TypeFlag(4);
/// TypeBool for boolean
pub const TypeBool: TypeFlag = TypeFlag(5);
/// TypeInt for integer
pub const TypeInt: TypeFlag = TypeFlag(6);
/// TypeEnum for Enum
pub const TypeEnum: TypeFlag = TypeFlag(7);
/// TypeFloat for Double
pub const TypeFloat: TypeFlag = TypeFlag(8);
/// TypeUnsigned for Unsigned integer
pub const TypeUnsigned: TypeFlag = TypeFlag(9);
/// TypeTime for time of day (a TiDB extension)
pub const TypeTime: TypeFlag = TypeFlag(10);
/// TypeDuration for a golang duration (a TiDB extension)
pub const TypeDuration: TypeFlag = TypeFlag(11);

/// On is the canonical string for ON
pub const On: &str = "ON";
/// Off is the canonical string for OFF
pub const Off: &str = "OFF";
/// Warn means return warnings
pub const Warn: &str = "WARN";
/// IntOnly means enable for int type
pub const IntOnly: &str = "INT_ONLY";
/// Marker is a special log redact behavior
pub const Marker: &str = "MARKER";
/// AssertionStrictStr is a choice of variable TiDBTxnAssertionLevel that means full assertions should be performed,
/// even if the performance might be slowed down.
pub const AssertionStrictStr: &str = "STRICT";
/// AssertionFastStr is a choice of variable TiDBTxnAssertionLevel that means assertions that doesn't affect
/// performance should be performed.
pub const AssertionFastStr: &str = "FAST";
/// AssertionOffStr is a choice of variable TiDBTxnAssertionLevel that means no assertion should be performed.
pub const AssertionOffStr: &str = "OFF";
/// OOMActionCancel constants represents the valid action configurations for OOMAction "CANCEL".
pub const OOMActionCancel: &str = "CANCEL";
/// OOMActionLog constants represents the valid action configurations for OOMAction "LOG".
pub const OOMActionLog: &str = "LOG";
/// TSOClientRPCModeDefault is a choice of variable TiDBTSOClientRPCMode.
pub const TSOClientRPCModeDefault: &str = "DEFAULT";
/// TSOClientRPCModeParallel is a choice of variable TiDBTSOClientRPCMode.
pub const TSOClientRPCModeParallel: &str = "PARALLEL";
/// TSOClientRPCModeParallelFast is a choice of variable TiDBTSOClientRPCMode.
pub const TSOClientRPCModeParallelFast: &str = "PARALLEL-FAST";
/// Global config name list.
pub const GlobalConfigEnableTopSQL: &str = "enable_resource_metering";
/// Global config name list.
pub const GlobalConfigSourceID: &str = "source_id";

impl std::fmt::Display for ScopeFlag {
    // go: ScopeFlag.String L1
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = *self;
        if s == ScopeNone {
            return f.write_str("NONE");
        }
        let mut scopes: Vec<&str> = Vec::new();
        if s.0 & ScopeSession.0 != 0 {
            scopes.push("SESSION");
        }
        if s.0 & ScopeGlobal.0 != 0 {
            scopes.push("GLOBAL");
        }
        if s.0 & ScopeInstance.0 != 0 {
            scopes.push("INSTANCE");
        }
        f.write_str(&scopes.join(","))
    }
}

/// The result of a validation: Go returns the value and the error together (`return
/// normalizedValue, err`) and callers use the value even when there is an error.
pub type ValResult = (String, Result<(), Error>);

/// `SysVar.Validation`: a callback after the type validation has been performed, but before
/// the Set function (`(vars, normalizedValue, originalValue, scope)`).
pub type ValidationFn = fn(&mut SessionVars, &str, &str, ScopeFlag) -> ValResult;

// ---------------------------------------------------------------- generated table

/// `SysVarData.flags`: the boolean fields of `SysVar`.
pub const FLAG_AUTO_CONVERT_NEGATIVE_BOOL: u16 = 1 << 0;
/// See [`FLAG_AUTO_CONVERT_NEGATIVE_BOOL`].
pub const FLAG_READ_ONLY: u16 = 1 << 1;
/// See [`FLAG_AUTO_CONVERT_NEGATIVE_BOOL`].
pub const FLAG_ALLOW_EMPTY: u16 = 1 << 2;
/// See [`FLAG_AUTO_CONVERT_NEGATIVE_BOOL`].
pub const FLAG_ALLOW_EMPTY_ALL: u16 = 1 << 3;
/// See [`FLAG_AUTO_CONVERT_NEGATIVE_BOOL`].
pub const FLAG_ALLOW_AUTO_VALUE: u16 = 1 << 4;
/// `IsHintUpdatableVerified` as `setHintUpdatable` left it after init.
pub const FLAG_HINT_UPDATABLE_VERIFIED: u16 = 1 << 5;
/// See [`FLAG_AUTO_CONVERT_NEGATIVE_BOOL`].
pub const FLAG_HIDDEN: u16 = 1 << 6;
/// See [`FLAG_AUTO_CONVERT_NEGATIVE_BOOL`].
pub const FLAG_DEPENDED: u16 = 1 << 7;
/// See [`FLAG_AUTO_CONVERT_NEGATIVE_BOOL`].
pub const FLAG_SKIP_INIT: u16 = 1 << 8;
/// Set for every entry of `noopSysVars` (variable.go init).
pub const FLAG_IS_NOOP: u16 = 1 << 9;

/// `SysVarData.hooks`: which hook fields Go's entry has.
pub const HOOK_VALIDATION: u8 = 1 << 0;
/// See [`HOOK_VALIDATION`].
pub const HOOK_SET_SESSION: u8 = 1 << 1;
/// See [`HOOK_VALIDATION`].
pub const HOOK_SET_GLOBAL: u8 = 1 << 2;
/// See [`HOOK_VALIDATION`].
pub const HOOK_GET_SESSION: u8 = 1 << 3;
/// See [`HOOK_VALIDATION`].
pub const HOOK_GET_GLOBAL: u8 = 1 << 4;
/// See [`HOOK_VALIDATION`].
pub const HOOK_GET_STATE_VALUE: u8 = 1 << 5;
/// See [`HOOK_VALIDATION`].
pub const HOOK_REQUIRE_DYNAMIC_PRIVILEGES: u8 = 1 << 6;

/// One entry of the generated table: the static fields of a Go `*SysVar` after init.
pub struct SysVarData {
    /// Go file of the entry (`sysvar.go` or `noop.go`).
    pub go_file: &'static str,
    /// Go line of the entry.
    pub go_line: u32,
    /// Scope is for whether can be changed or not
    pub scope: ScopeFlag,
    /// Name is the variable name.
    pub name: &'static str,
    /// Value is the variable value (as evaluated by Go's init on the generating host).
    pub value: &'static str,
    /// The Go expression of a value that depends on the build target or the loaded config.
    pub computed: Option<&'static str>,
    /// Type is the MySQL type (optional)
    pub typ: TypeFlag,
    /// MinValue will automatically be validated when specified (optional)
    pub min_value: i64,
    /// MaxValue will automatically be validated when specified (optional)
    pub max_value: u64,
    /// PossibleValues applies to ENUM type
    pub possible_values: &'static [&'static str],
    /// Aliases is a list of sysvars that should also be updated when this sysvar is updated.
    pub aliases: &'static [&'static str],
    /// GlobalConfigName is the global config name of this global variable.
    pub global_config_name: &'static str,
    /// `FLAG_*`.
    pub flags: u16,
    /// `HOOK_*`.
    pub hooks: u8,
}

// ---------------------------------------------------------------- SysVar

/// SysVar is for system variable.
/// All the fields of SysVar should be READ ONLY after created.
#[derive(Clone, Debug)]
pub struct SysVar {
    /// Scope is for whether can be changed or not
    pub scope: ScopeFlag,
    /// Name is the variable name.
    pub name: &'static str,
    /// Value is the variable value.
    pub value: String,
    /// Type is the MySQL type (optional)
    pub typ: TypeFlag,
    /// MinValue will automatically be validated when specified (optional)
    pub min_value: i64,
    /// MaxValue will automatically be validated when specified (optional)
    pub max_value: u64,
    /// AutoConvertNegativeBool applies to boolean types (optional)
    pub auto_convert_negative_bool: bool,
    /// ReadOnly applies to all types
    pub read_only: bool,
    /// PossibleValues applies to ENUM type
    pub possible_values: &'static [&'static str],
    /// AllowEmpty is a special TiDB behavior which means "read value from config" (do not use)
    pub allow_empty: bool,
    /// AllowEmptyAll is a special behavior that only applies to TiDBCapturePlanBaseline, TiDBTxnMode (do not use)
    pub allow_empty_all: bool,
    /// AllowAutoValue means that the special value "-1" is permitted, even when outside of range.
    pub allow_auto_value: bool,
    /// Validation is a callback after the type validation has been performed, but before the Set function
    pub validation: Option<ValidationFn>,
    /// IsHintUpdatableVerified indicate whether we've confirmed that SET_VAR() hint is worked for this hint.
    pub is_hint_updatable_verified: bool,
    /// Deprecated: Hidden previously meant that the variable still responds to SET but doesn't show up in SHOW VARIABLES
    pub hidden: bool,
    /// Aliases is a list of sysvars that should also be updated when this sysvar is updated.
    pub aliases: &'static [&'static str],
    /// Depended indicates whether other variables depend on this one.
    pub depended: bool,
    /// skipInit defines if the sysvar should be loaded into the session on init.
    pub(crate) skip_init: bool,
    /// IsNoop defines if the sysvar is a noop included for MySQL compatibility
    pub is_noop: bool,
    /// GlobalConfigName is the global config name of this global variable.
    pub global_config_name: &'static str,
    /// The `HOOK_*` hooks Go's entry has (the session hooks are block D05.2).
    pub go_hooks: u8,
    /// Go source position of the entry (`file:line`), for diagnostics.
    pub go_pos: (&'static str, u32),
}

/// `localDayTimeFormat` / `FullDayTimeFormat`.
const localDayTimeFormat: &str = "15:04";
/// FullDayTimeFormat is the full format of analyze start time and end time.
pub const FullDayTimeFormat: &str = "15:04 -0700";

impl SysVar {
    /// HasNoneScope returns true if the scope for the sysVar is None.
    // go: SysVar.HasNoneScope L1
    pub fn has_none_scope(&self) -> bool {
        self.scope == ScopeNone
    }

    /// HasSessionScope returns true if the scope for the sysVar includes session.
    // go: SysVar.HasSessionScope L1
    pub fn has_session_scope(&self) -> bool {
        self.scope.0 & ScopeSession.0 != 0
    }

    /// HasGlobalScope returns true if the scope for the sysVar includes global.
    // go: SysVar.HasGlobalScope L1
    pub fn has_global_scope(&self) -> bool {
        self.scope.0 & ScopeGlobal.0 != 0
    }

    /// HasInstanceScope returns true if the scope for the sysVar includes instance
    // go: SysVar.HasInstanceScope L1
    pub fn has_instance_scope(&self) -> bool {
        self.scope.0 & ScopeInstance.0 != 0
    }

    /// Validate checks if system variable satisfies specific restriction.
    // go: SysVar.Validate L1
    pub fn validate(&self, vars: &mut SessionVars, value: &str, scope: ScopeFlag) -> ValResult {
        // Check that the scope is correct first.
        if let Err(err) = self.validate_scope(scope) {
            return (value.to_owned(), Err(err));
        }
        // Normalize the value and apply validation based on type.
        // i.e. TypeBool converts 1/on/ON to ON.
        let (normalized_value, err) = self.validate_from_type(vars, value, scope);
        if err.is_err() {
            return (normalized_value, err);
        }
        // If type validation was successful, call the (optional) validation function
        if let Some(validation) = self.validation {
            return validation(vars, &normalized_value, value, scope);
        }
        (normalized_value, Ok(()))
    }

    /// ValidateFromType provides automatic validation based on the SysVar's type
    // go: SysVar.ValidateFromType L1
    pub fn validate_from_type(
        &self,
        vars: &mut SessionVars,
        value: &str,
        scope: ScopeFlag,
    ) -> ValResult {
        // Some sysvars in TiDB have a special behavior where the empty string means
        // "use the config file value". This needs to be cleaned up once the behavior
        // for instance variables is determined.
        if value.is_empty() && ((self.allow_empty && scope == ScopeSession) || self.allow_empty_all)
        {
            return (value.to_owned(), Ok(()));
        }
        // Provide validation using the SysVar struct
        match self.typ {
            TypeUnsigned => self.check_uint64_system_var(value, vars),
            TypeInt => self.check_int64_system_var(value, vars),
            TypeBool => self.check_bool_system_var(value, vars),
            TypeFloat => self.check_float_system_var(value, vars),
            TypeEnum => self.check_enum_system_var(value, vars),
            TypeTime => self.check_time_system_var(value, vars),
            TypeDuration => self.check_duration_system_var(value, vars),
            _ => (value.to_owned(), Ok(())), // typeString
        }
    }

    // go: SysVar.validateScope L1
    fn validate_scope(&self, scope: ScopeFlag) -> Result<(), Error> {
        if self.read_only || self.scope == ScopeNone {
            return Err(ErrIncorrectScope.fast_gen_by_args(vec![self.name.into(), "read only".into()]));
        }
        if scope == ScopeGlobal && !(self.has_global_scope() || self.has_instance_scope()) {
            return Err(errLocalVariable.fast_gen_by_args(vec![self.name.into()]));
        }
        if scope == ScopeSession && !self.has_session_scope() {
            return Err(errGlobalVariable.fast_gen_by_args(vec![self.name.into()]));
        }
        Ok(())
    }

    /// ValidateWithRelaxedValidation normalizes values but can not return errors.
    /// Normalization+validation needs to be applied when reading values because older versions of TiDB
    /// may be less sophisticated in normalizing values. But errors should be caught and handled,
    /// because otherwise there will be upgrade issues.
    // go: SysVar.ValidateWithRelaxedValidation L1
    pub fn validate_with_relaxed_validation(
        &self,
        vars: &mut SessionVars,
        value: &str,
        scope: ScopeFlag,
    ) -> String {
        let warns = vars.stmt_ctx.get_warnings();
        let normalized_value = (|| {
            let (normalized_value, err) = self.validate_from_type(vars, value, scope);
            if err.is_err() {
                return normalized_value;
            }
            if let Some(validation) = self.validation {
                let (normalized_value, _err) = validation(vars, &normalized_value, value, scope);
                return normalized_value;
            }
            normalized_value
        })();
        vars.stmt_ctx.set_warnings(warns); // RelaxedValidation = trim warnings too.
        normalized_value
    }

    // go: SysVar.checkTimeSystemVar L1
    fn check_time_system_var(&self, value: &str, vars: &mut SessionVars) -> ValResult {
        let loc = vars.location();
        let parsed = if value.len() <= localDayTimeFormat.len() {
            timeparse::parse_day_time(localDayTimeFormat, value, &loc)
        } else {
            timeparse::parse_day_time(FullDayTimeFormat, value, &loc)
        };
        match parsed {
            Err(err) => (String::new(), Err(tidb_base::errors::new(err))),
            // Add a modern date to it, as the timezone shift can differ across the history
            // For example, the Asia/Shanghai refers to +08:05 before 1900
            Ok(t) => (timeparse::format_full_day_time_today(&t), Ok(())),
        }
    }

    // go: SysVar.checkDurationSystemVar L1
    fn check_duration_system_var(&self, value: &str, vars: &mut SessionVars) -> ValResult {
        let d = match gostd::time::parse_duration(value) {
            Ok(d) => d,
            Err(_) => {
                return (
                    value.to_owned(),
                    Err(ErrWrongTypeForVar.gen_with_stack_by_args(vec![self.name.into()])),
                );
            }
        };
        // Check for min/max violations
        if d < self.min_value {
            vars.stmt_ctx.append_warning(
                ErrTruncatedWrongValue.fast_gen_by_args(vec![self.name.into(), Arg::from(value.to_owned())]),
            );
            return (gostd::time::duration_string(self.min_value), Ok(()));
        }
        if (d as u64) > self.max_value {
            vars.stmt_ctx.append_warning(
                ErrTruncatedWrongValue.fast_gen_by_args(vec![self.name.into(), Arg::from(value.to_owned())]),
            );
            // [M-INT] time.Duration(sv.MaxValue): uint64 -> int64 wraps.
            return (gostd::time::duration_string(self.max_value as i64), Ok(()));
        }
        // return a string representation of the duration
        (gostd::time::duration_string(d), Ok(()))
    }

    // go: SysVar.checkUInt64SystemVar L1
    fn check_uint64_system_var(&self, value: &str, vars: &mut SessionVars) -> ValResult {
        if self.allow_auto_value && value == "-1" {
            return (value.to_owned(), Ok(()));
        }
        if value.is_empty() {
            return (
                value.to_owned(),
                Err(ErrWrongTypeForVar.gen_with_stack_by_args(vec![self.name.into()])),
            );
        }
        if value.as_bytes()[0] == b'-' {
            let (_, err) = tidb_types::gostd::parse_int(value.as_bytes(), 64);
            if err.is_err() {
                return (
                    value.to_owned(),
                    Err(ErrWrongTypeForVar.gen_with_stack_by_args(vec![self.name.into()])),
                );
            }
            vars.stmt_ctx.append_warning(
                ErrTruncatedWrongValue.fast_gen_by_args(vec![self.name.into(), Arg::from(value.to_owned())]),
            );
            return (self.min_value.to_string(), Ok(()));
        }
        let (val, err) = tidb_types::gostd::parse_uint(value.as_bytes(), 64);
        if err.is_err() {
            return (
                value.to_owned(),
                Err(ErrWrongTypeForVar.gen_with_stack_by_args(vec![self.name.into()])),
            );
        }
        // [M-INT] uint64(sv.MinValue)
        if val < self.min_value as u64 {
            vars.stmt_ctx.append_warning(
                ErrTruncatedWrongValue.fast_gen_by_args(vec![self.name.into(), Arg::from(value.to_owned())]),
            );
            return (self.min_value.to_string(), Ok(()));
        }
        if val > self.max_value {
            vars.stmt_ctx.append_warning(
                ErrTruncatedWrongValue.fast_gen_by_args(vec![self.name.into(), Arg::from(value.to_owned())]),
            );
            return (self.max_value.to_string(), Ok(()));
        }
        (value.to_owned(), Ok(()))
    }

    // go: SysVar.checkInt64SystemVar L1
    fn check_int64_system_var(&self, value: &str, vars: &mut SessionVars) -> ValResult {
        if self.allow_auto_value && value == "-1" {
            return (value.to_owned(), Ok(()));
        }
        let (val, err) = tidb_types::gostd::parse_int(value.as_bytes(), 64);
        if err.is_err() {
            return (
                value.to_owned(),
                Err(ErrWrongTypeForVar.gen_with_stack_by_args(vec![self.name.into()])),
            );
        }
        if val < self.min_value {
            vars.stmt_ctx.append_warning(
                ErrTruncatedWrongValue.fast_gen_by_args(vec![self.name.into(), Arg::from(value.to_owned())]),
            );
            return (self.min_value.to_string(), Ok(()));
        }
        // [M-INT] int64(sv.MaxValue) wraps for values above MaxInt64.
        if val > self.max_value as i64 {
            vars.stmt_ctx.append_warning(
                ErrTruncatedWrongValue.fast_gen_by_args(vec![self.name.into(), Arg::from(value.to_owned())]),
            );
            return (self.max_value.to_string(), Ok(()));
        }
        (value.to_owned(), Ok(()))
    }

    // go: SysVar.checkEnumSystemVar L1
    fn check_enum_system_var(&self, value: &str, _vars: &mut SessionVars) -> ValResult {
        // The value could be either a string or the ordinal position in the PossibleValues.
        // This allows for the behavior 0 = OFF, 1 = ON, 2 = DEMAND etc.
        for (i, v) in self.possible_values.iter().enumerate() {
            let i_str = i.to_string();
            if gostd::equal_fold(value, v) || gostd::equal_fold(value, &i_str) {
                return ((*v).to_owned(), Ok(()));
            }
        }
        (
            value.to_owned(),
            Err(ErrWrongValueForVar.gen_with_stack_by_args(vec![self.name.into(), Arg::from(value.to_owned())])),
        )
    }

    // go: SysVar.checkFloatSystemVar L1
    fn check_float_system_var(&self, value: &str, vars: &mut SessionVars) -> ValResult {
        if value.is_empty() {
            return (
                value.to_owned(),
                Err(ErrWrongTypeForVar.gen_with_stack_by_args(vec![self.name.into()])),
            );
        }
        let val = match parse_float64(value) {
            Ok(v) => v,
            Err(_) => {
                return (
                    value.to_owned(),
                    Err(ErrWrongTypeForVar.gen_with_stack_by_args(vec![self.name.into()])),
                );
            }
        };
        if val < self.min_value as f64 {
            vars.stmt_ctx.append_warning(
                ErrTruncatedWrongValue.fast_gen_by_args(vec![self.name.into(), Arg::from(value.to_owned())]),
            );
            return (self.min_value.to_string(), Ok(()));
        }
        if val > self.max_value as f64 {
            vars.stmt_ctx.append_warning(
                ErrTruncatedWrongValue.fast_gen_by_args(vec![self.name.into(), Arg::from(value.to_owned())]),
            );
            return (self.max_value.to_string(), Ok(()));
        }
        (value.to_owned(), Ok(()))
    }

    // go: SysVar.checkBoolSystemVar L1
    fn check_bool_system_var(&self, value: &str, _vars: &mut SessionVars) -> ValResult {
        if gostd::equal_fold(value, "ON") {
            return (On.to_owned(), Ok(()));
        } else if gostd::equal_fold(value, "OFF") {
            return (Off.to_owned(), Ok(()));
        }
        let (val, err) = tidb_types::gostd::parse_int(value.as_bytes(), 64);
        if err.is_ok() {
            // There are two types of conversion rules for integer values.
            // The default only allows 0 || 1, but a subset of values convert any
            // negative integer to 1.
            if !self.auto_convert_negative_bool {
                if val == 0 {
                    return (Off.to_owned(), Ok(()));
                } else if val == 1 {
                    return (On.to_owned(), Ok(()));
                }
            } else if val == 1 || val < 0 {
                return (On.to_owned(), Ok(()));
            } else if val == 0 {
                return (Off.to_owned(), Ok(()));
            }
        }
        (
            value.to_owned(),
            Err(ErrWrongValueForVar.gen_with_stack_by_args(vec![self.name.into(), Arg::from(value.to_owned())])),
        )
    }

    /// GetNativeValType attempts to convert the val to the approx MySQL non-string type
    // go: SysVar.GetNativeValType L1
    pub fn get_native_val_type(&self, val: &str) -> (Datum, u8, usize) {
        match self.typ {
            TypeUnsigned => {
                let (u, err) = tidb_types::gostd::parse_uint(val.as_bytes(), 64);
                let u = if err.is_err() { 0 } else { u };
                (
                    tidb_types::datum::new_uint_datum(u),
                    mysql::TypeLonglong,
                    mysql::UnsignedFlag | mysql::BinaryFlag,
                )
            }
            TypeBool => {
                let mut opt_val: i64 = 0; // OFF
                if super::varsutil::tidb_opt_on(val) {
                    opt_val = 1;
                }
                (tidb_types::datum::new_int_datum(opt_val), mysql::TypeLonglong, mysql::BinaryFlag)
            }
            _ => (tidb_types::datum::new_string_datum(val), mysql::TypeVarString, 0),
        }
    }

    /// SkipInit returns true if when a new session is created we should "skip" copying
    /// an initial value to it (and call the SetSession func if it exists)
    // go: SysVar.SkipInit L1
    pub fn skip_init(&self) -> bool {
        if self.skip_init || self.is_noop {
            return true;
        }
        !self.has_session_scope()
    }

    /// SkipSysvarCache returns true if the sysvar should not re-execute on peers
    /// This doesn't make sense for the GC variables because they are based in tikv
    /// tables.
    // go: SysVar.SkipSysvarCache L1
    pub fn skip_sysvar_cache(&self) -> bool {
        use super::names::*;
        matches!(
            self.name,
            TiDBGCEnable
                | TiDBGCRunInterval
                | TiDBGCLifetime
                | TiDBGCConcurrency
                | TiDBGCScanLockMode
                | TiDBExternalTS
        )
    }
}

/// `strconv.ParseFloat(s, 64)` (errors only matter as "not nil").
// go-std: strconv.ParseFloat L1 (bitSize 64)
pub(crate) fn parse_float64(s: &str) -> Result<f64, ()> {
    let (v, err) = tidb_types::gostd::parse_float(s);
    err.map(|()| v).map_err(|_| ())
}

// ---------------------------------------------------------------- registry

/// The static registry: the generated table with the hand-written hooks, indexed by
/// lower-case name (Go's `sysVars` after init).
struct Registry {
    vars: Vec<SysVar>,
    index: HashMap<&'static str, usize>,
}

static REGISTRY: LazyLock<Registry> = LazyLock::new(|| {
    let mut vars = Vec::with_capacity(SYSVAR_DATA.len());
    let mut index = HashMap::with_capacity(SYSVAR_DATA.len());
    for d in SYSVAR_DATA {
        let sv = sysvar_of(d);
        // go: variable.go init -> RegisterSysVar (name lower-cased; the names are lower case).
        debug_assert_eq!(sv.name, sv.name.to_lowercase());
        index.insert(sv.name, vars.len());
        vars.push(sv);
    }
    Registry { vars, index }
});

/// The value of an entry whose Go value depends on the build target.
// glue: runtime.GOOS / runtime.GOARCH of the Go build, for this Rust build
fn computed_value(d: &SysVarData) -> String {
    match d.computed {
        Some("runtime.GOOS") => go_goos().to_owned(),
        Some("runtime.GOARCH") => go_goarch().to_owned(),
        // Values from config.GetGlobalConfig() are the default config's (Go evaluates them in
        // the package initializer, before the config file is loaded).
        _ => d.value.to_owned(),
    }
}

/// `runtime.GOOS` for the target of this build.
// glue: Rust target_os -> Go GOOS
pub fn go_goos() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        os => os,
    }
}

/// `runtime.GOARCH` for the target of this build.
// glue: Rust target_arch -> Go GOARCH
pub fn go_goarch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        "x86" => "386",
        arch => arch,
    }
}

// glue: SysVarData (generated static fields) + hooks -> SysVar
fn sysvar_of(d: &'static SysVarData) -> SysVar {
    let validation = hooks::validation(d.name);
    // go: variable.go init -> setHintUpdatable(defaultSysVars): every listed default entry is
    // verified (an entry may also set IsHintUpdatableVerified in its literal).
    if d.go_file == "sysvar.go" && super::setvar_affect::set_hint_updatable(d.name) {
        assert!(
            d.flags & FLAG_HINT_UPDATABLE_VERIFIED != 0,
            "sysvar {}: IsHintUpdatableVerified mismatch with Go",
            d.name
        );
    }
    SysVar {
        scope: d.scope,
        name: d.name,
        value: computed_value(d),
        typ: d.typ,
        min_value: d.min_value,
        max_value: d.max_value,
        auto_convert_negative_bool: d.flags & FLAG_AUTO_CONVERT_NEGATIVE_BOOL != 0,
        read_only: d.flags & FLAG_READ_ONLY != 0,
        possible_values: d.possible_values,
        allow_empty: d.flags & FLAG_ALLOW_EMPTY != 0,
        allow_empty_all: d.flags & FLAG_ALLOW_EMPTY_ALL != 0,
        allow_auto_value: d.flags & FLAG_ALLOW_AUTO_VALUE != 0,
        validation,
        is_hint_updatable_verified: d.flags & FLAG_HINT_UPDATABLE_VERIFIED != 0,
        hidden: d.flags & FLAG_HIDDEN != 0,
        aliases: d.aliases,
        depended: d.flags & FLAG_DEPENDED != 0,
        skip_init: d.flags & FLAG_SKIP_INIT != 0,
        is_noop: d.flags & FLAG_IS_NOOP != 0,
        global_config_name: d.global_config_name,
        go_hooks: d.hooks,
        go_pos: (d.go_file, d.go_line),
    }
}

/// The instance overlay of `SetSysVar` (N8 §3.1 item 4): values that replace a registry
/// entry's default on this instance only.
static OVERLAY: LazyLock<RwLock<HashMap<&'static str, Arc<SysVar>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// A registry entry: the static one, or its overlaid copy (`SetSysVar`).
#[derive(Clone, Debug)]
pub enum SysVarRef {
    /// The static entry.
    Static(&'static SysVar),
    /// A copy registered by `SetSysVar`.
    Overlaid(Arc<SysVar>),
}

impl std::ops::Deref for SysVarRef {
    type Target = SysVar;
    // glue: access to the entry
    fn deref(&self) -> &SysVar {
        match self {
            SysVarRef::Static(s) => s,
            SysVarRef::Overlaid(s) => s,
        }
    }
}

/// GetSysVar returns sys var info for name as key.
// go: GetSysVar L2 (static table + instance overlay instead of the locked map)
pub fn get_sys_var(name: &str) -> Option<SysVarRef> {
    let name = gostd::to_lower(name);
    let reg = &*REGISTRY;
    let &i = reg.index.get(name.as_str())?;
    let sv = &reg.vars[i];
    if let Some(o) = OVERLAY.read().get(sv.name) {
        return Some(SysVarRef::Overlaid(o.clone()));
    }
    Some(SysVarRef::Static(sv))
}

/// SetSysVar sets a sysvar. In fact, SysVar is immutable.
/// SetSysVar is implemented by register a new SysVar with the same name again.
/// This will not propagate to the cluster, so it should only be
/// used for instance scoped AUTO variables such as system_time_zone.
// go: SetSysVar L2 (instance overlay, N8 §3.1 item 4)
pub fn set_sys_var(name: &str, value: &str) {
    let old = get_sys_var(name).expect("SetSysVar of an unknown variable");
    let mut tmp = (*old).clone();
    tmp.value = value.to_owned();
    OVERLAY.write().insert(tmp.name, Arc::new(tmp));
}

/// GetSysVars returns every registered variable (Go deep-copies the map; the registry is
/// immutable here, so this iterates it).
// go: GetSysVars L2 (an iterator over the registry instead of a copied map)
pub fn get_sys_vars() -> impl Iterator<Item = SysVarRef> {
    REGISTRY.vars.iter().map(|sv| get_sys_var(sv.name).expect("registered"))
}

/// OrderByDependency orders the vars by dependency. The depended sys vars are in the front.
/// Unknown sys vars are treated as not depended.
///
/// Go iterates a map, so the order inside each group is random (M-MAPORDER); here it is the
/// order of `names`.
// go: OrderByDependency L1 [M-MAPORDER]
pub fn order_by_dependency<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut depended = Vec::new();
    let mut not_depended = Vec::new();
    for name in names {
        // sysVars[name]: the map key is the lower-case name, looked up as given.
        let sv = REGISTRY.index.get(name).map(|&i| &REGISTRY.vars[i]);
        if sv.is_some_and(|sv| sv.depended) {
            depended.push(name);
        } else {
            not_depended.push(name);
        }
    }
    depended.extend(not_depended);
    depended
}
