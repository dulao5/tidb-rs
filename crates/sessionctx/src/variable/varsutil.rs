//! go-file: pkg/sessionctx/variable/varsutil.go
//!
//! Helpers of the variable package: value conversions (`TiDBOptOn`, `TidbOptInt64`, ...),
//! the checks shared by validation hooks, and the byte-size / percentage parsers.
//! The SetSession / SetGlobal helpers (`setSnapshotTS`, `setTxnReadTS`, `setReadStaleness`,
//! `switchDDL`, `switchStats`) belong with the session hooks of block D05.2;
//! `getTiDBTableValue` / `setTiDBTableValue` are here (D05.2 PR C).

#![allow(non_upper_case_globals)]

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering::SeqCst};

use tidb_base::Error;
use tidb_base::gostd;

use super::error::*;
use super::names::*;
use super::session::SessionVars;
use super::tidb_vars::{self, PasswordValidationLength};
use super::variable::{Off, On, ScopeFlag, ScopeGlobal, ScopeSession, ValResult, Warn};

const secondsPerYear: i64 = 60 * 60 * 24 * 365;
const _: i64 = secondsPerYear;

/// `ddlReorgWorkerCounter` and friends: process-wide DDL settings (tidb_vars.go).
static ddlReorgWorkerCounter: AtomicI32 = AtomicI32::new(tidb_vars::DefTiDBDDLReorgWorkerCount);
static ddlFlashbackConcurrency: AtomicI32 =
    AtomicI32::new(tidb_vars::DefTiDBDDLFlashbackConcurrency);
static ddlReorgBatchSize: AtomicI32 = AtomicI32::new(tidb_vars::DefTiDBDDLReorgBatchSize);
static ddlErrorCountLimit: AtomicI64 = AtomicI64::new(tidb_vars::DefTiDBDDLErrorCountLimit);
static ddlReorgRowFormat: AtomicI64 = AtomicI64::new(tidb_vars::DefTiDBRowFormatV2);
static maxDeltaSchemaCount: AtomicI64 = AtomicI64::new(tidb_vars::DefTiDBMaxDeltaSchemaCount);

/// SetDDLReorgWorkerCounter sets ddlReorgWorkerCounter count.
/// Sysvar validation enforces the max value is not more than MaxConfigurableConcurrency (256).
// go: SetDDLReorgWorkerCounter L1
pub fn set_ddl_reorg_worker_counter(cnt: i32) {
    ddlReorgWorkerCounter.store(cnt, SeqCst);
}

/// GetDDLReorgWorkerCounter gets ddlReorgWorkerCounter.
// go: GetDDLReorgWorkerCounter L1
pub fn get_ddl_reorg_worker_counter() -> i32 {
    ddlReorgWorkerCounter.load(SeqCst)
}

/// SetDDLFlashbackConcurrency sets ddlFlashbackConcurrency count.
// go: SetDDLFlashbackConcurrency L1
pub fn set_ddl_flashback_concurrency(cnt: i32) {
    ddlFlashbackConcurrency.store(cnt, SeqCst);
}

/// GetDDLFlashbackConcurrency gets ddlFlashbackConcurrency count.
// go: GetDDLFlashbackConcurrency L1
pub fn get_ddl_flashback_concurrency() -> i32 {
    ddlFlashbackConcurrency.load(SeqCst)
}

/// SetDDLReorgBatchSize sets ddlReorgBatchSize size.
// go: SetDDLReorgBatchSize L1
pub fn set_ddl_reorg_batch_size(cnt: i32) {
    ddlReorgBatchSize.store(cnt, SeqCst);
}

/// GetDDLReorgBatchSize gets ddlReorgBatchSize.
// go: GetDDLReorgBatchSize L1
pub fn get_ddl_reorg_batch_size() -> i32 {
    ddlReorgBatchSize.load(SeqCst)
}

/// SetDDLErrorCountLimit sets ddlErrorCountlimit size.
// go: SetDDLErrorCountLimit L1
pub fn set_ddl_error_count_limit(cnt: i64) {
    ddlErrorCountLimit.store(cnt, SeqCst);
}

/// GetDDLErrorCountLimit gets ddlErrCountLimit size.
// go: GetDDLErrorCountLimit L1
pub fn get_ddl_error_count_limit() -> i64 {
    ddlErrorCountLimit.load(SeqCst)
}

/// SetDDLReorgRowFormat sets ddlReorgRowFormat version.
// go: SetDDLReorgRowFormat L1
pub fn set_ddl_reorg_row_format(format: i64) {
    ddlReorgRowFormat.store(format, SeqCst);
}

/// GetDDLReorgRowFormat gets ddlReorgRowFormat version.
// go: GetDDLReorgRowFormat L1
pub fn get_ddl_reorg_row_format() -> i64 {
    ddlReorgRowFormat.load(SeqCst)
}

/// SetMaxDeltaSchemaCount sets maxDeltaSchemaCount size.
// go: SetMaxDeltaSchemaCount L1
pub fn set_max_delta_schema_count(cnt: i64) {
    maxDeltaSchemaCount.store(cnt, SeqCst);
}

/// GetMaxDeltaSchemaCount gets maxDeltaSchemaCount size.
// go: GetMaxDeltaSchemaCount L1
pub fn get_max_delta_schema_count() -> i64 {
    maxDeltaSchemaCount.load(SeqCst)
}

/// BoolToOnOff returns the string representation of a bool, i.e. "ON/OFF"
// go: BoolToOnOff L1
pub fn bool_to_on_off(b: bool) -> &'static str {
    if b { On } else { Off }
}

// go: int32ToBoolStr L1
pub fn int32_to_bool_str(i: i32) -> &'static str {
    if i == 1 { On } else { Off }
}

// go: checkCollation L1
pub(crate) fn check_collation(
    _vars: &mut SessionVars,
    normalized_value: &str,
    _original_value: &str,
    _scope: ScopeFlag,
) -> ValResult {
    match tidb_types::collate::get_collation_by_name(normalized_value) {
        Err(err) => (normalized_value.to_owned(), Err(err)),
        Ok(coll) => (coll.name.clone(), Ok(())),
    }
}

// go: checkDefaultCollationForUTF8MB4 L1
pub(crate) fn check_default_collation_for_utf8mb4(
    _vars: &mut SessionVars,
    normalized_value: &str,
    _original_value: &str,
    _scope: ScopeFlag,
) -> ValResult {
    let coll = match tidb_types::collate::get_collation_by_name(normalized_value) {
        Err(err) => return (normalized_value.to_owned(), Err(err)),
        Ok(coll) => coll,
    };
    if !tidb_types::collate::is_default_collation_for_utf8mb4(&coll.name) {
        return (
            String::new(),
            Err(ErrInvalidDefaultUTF8MB4Collation
                .gen_with_stack_by_args(vec![coll.name.clone().into()])),
        );
    }
    (coll.name.clone(), Ok(()))
}

// go: checkCharacterSet L1
pub(crate) fn check_character_set(normalized_value: &str, arg_name: &str) -> ValResult {
    if normalized_value.is_empty() {
        return (
            normalized_value.to_owned(),
            Err(ErrWrongValueForVar
                .fast_gen_by_args(vec![arg_name.to_owned().into(), "NULL".into()])),
        );
    }
    let (cs, err) = tidb_parser::gocharset::get_charset_info(normalized_value);
    if let Err(err) = err {
        return (normalized_value.to_owned(), Err(err));
    }
    (cs.expect("charset when no error").name.clone(), Ok(()))
}

/// checkReadOnly requires TiDBEnableNoopFuncs=1 for the same scope otherwise an error will be returned.
// go: checkReadOnly L1
pub(crate) fn check_read_only(
    vars: &mut SessionVars,
    normalized_value: &str,
    original_value: &str,
    scope: ScopeFlag,
    offline_mode: bool,
) -> ValResult {
    let mut err_msg = ErrFunctionsNoopImpl.fast_gen_by_args(vec!["READ ONLY".into()]);
    if offline_mode {
        err_msg = ErrFunctionsNoopImpl.fast_gen_by_args(vec!["OFFLINE MODE".into()]);
    }
    if tidb_opt_on(normalized_value) {
        if scope == ScopeSession && vars.noop_funcs_mode != OnInt {
            if vars.noop_funcs_mode == OffInt {
                return (Off.to_owned(), Err(err_msg));
            }
            vars.stmt_ctx().append_warning(err_msg.clone());
        }
        if scope == ScopeGlobal {
            let val = match vars
                .global_vars_accessor
                .get_global_sys_var(TiDBEnableNoopFuncs)
            {
                Ok(v) => v,
                Err(_) => {
                    return (
                        original_value.to_owned(),
                        Err(errUnknownSystemVariable
                            .gen_with_stack_by_args(vec![TiDBEnableNoopFuncs.into()])),
                    );
                }
            };
            if val == Off {
                return (Off.to_owned(), Err(err_msg));
            }
            if val == Warn {
                vars.stmt_ctx().append_warning(err_msg);
            }
        }
    }
    (normalized_value.to_owned(), Ok(()))
}

// go: checkIsolationLevel L1
pub(crate) fn check_isolation_level(
    vars: &mut SessionVars,
    normalized_value: &str,
    _original_value: &str,
    _scope: ScopeFlag,
) -> ValResult {
    if normalized_value == "SERIALIZABLE" || normalized_value == "READ-UNCOMMITTED" {
        let return_err =
            ErrUnsupportedIsolationLevel.fast_gen_by_args(vec![normalized_value.to_owned().into()]);
        let skip = vars
            .systems
            .get(TiDBSkipIsolationLevelCheck)
            .map(String::as_str)
            .unwrap_or("");
        if !tidb_opt_on(skip) {
            return (
                normalized_value.to_owned(),
                Err(ErrUnsupportedIsolationLevel
                    .gen_with_stack_by_args(vec![normalized_value.to_owned().into()])),
            );
        }
        vars.stmt_ctx().append_warning(return_err);
    }
    (normalized_value.to_owned(), Ok(()))
}

/// Deprecated: Get the value from the mysql.tidb table.
/// This supports the use case that a TiDB server *older* than 5.0 is a member of the cluster.
/// i.e. system variables such as tidb_gc_concurrency, tidb_gc_enable, tidb_gc_life_time
/// do not exist.
// go: getTiDBTableValue L1
pub(crate) fn get_tidb_table_value(
    vars: &SessionVars,
    name: &str,
    default_val: &str,
) -> Result<String, Error> {
    match vars.global_vars_accessor.get_tidb_table_value(name) {
        // handle empty result or other errors
        Err(_) => Ok(default_val.to_owned()),
        Ok(val) => Ok(true_false_to_on_off(&val)),
    }
}

/// Deprecated: Set the value from the mysql.tidb table.
/// This supports the use case that a TiDB server *older* than 5.0 is a member of the cluster.
/// i.e. system variables such as tidb_gc_concurrency, tidb_gc_enable, tidb_gc_life_time
/// do not exist.
// go: setTiDBTableValue L1
pub(crate) fn set_tidb_table_value(
    vars: &mut SessionVars,
    name: &str,
    value: &str,
    comment: &str,
) -> Result<(), Error> {
    let value = on_off_to_true_false(value);
    vars.global_vars_accessor
        .set_tidb_table_value(name, &value, comment)
}

/// In mysql.tidb the convention has been to store the string value "true"/"false",
/// but sysvars use the convention ON/OFF.
// go: trueFalseToOnOff L1
pub fn true_false_to_on_off(s: &str) -> String {
    if gostd::equal_fold("true", s) {
        return On.to_owned();
    } else if gostd::equal_fold("false", s) {
        return Off.to_owned();
    }
    s.to_owned()
}

/// OnOffToTrueFalse convert "ON"/"OFF" to "true"/"false".
/// In mysql.tidb the convention has been to store the string value "true"/"false",
/// but sysvars use the convention ON/OFF.
// go: OnOffToTrueFalse L1
pub fn on_off_to_true_false(s: &str) -> String {
    if gostd::equal_fold("ON", s) {
        return "true".to_owned();
    } else if gostd::equal_fold("OFF", s) {
        return "false".to_owned();
    }
    s.to_owned()
}

/// initChunkSizeUpperBound indicates upper bound value of tidb_init_chunk_size.
pub const initChunkSizeUpperBound: i64 = 32;
/// maxChunkSizeLowerBound indicates lower bound value of tidb_max_chunk_size.
pub const maxChunkSizeLowerBound: i64 = 32;

// go: appendDeprecationWarning L1
pub(crate) fn append_deprecation_warning(s: &mut SessionVars, name: &str, replacement: &str) {
    s.stmt_ctx().append_warning(
        errWarnDeprecatedSyntax
            .fast_gen_by_args(vec![name.to_owned().into(), replacement.to_owned().into()]),
    );
}

/// TiDBOptOn could be used for all tidb session variable options, we use "ON"/1 to turn on those options.
// go: TiDBOptOn L1
pub fn tidb_opt_on(opt: &str) -> bool {
    gostd::equal_fold(opt, "ON") || opt == "1"
}

/// OffInt is used by TiDBOptOnOffWarn
pub const OffInt: i64 = 0;
/// OnInt is used TiDBOptOnOffWarn
pub const OnInt: i64 = 1;
/// WarnInt is used by TiDBOptOnOffWarn
pub const WarnInt: i64 = 2;

/// TiDBOptOnOffWarn converts On/Off/Warn to an int.
/// It is used for MultiStmtMode and NoopFunctionsMode
// go: TiDBOptOnOffWarn L1
pub fn tidb_opt_on_off_warn(opt: &str) -> i64 {
    match opt {
        Warn => WarnInt,
        On => OnInt,
        _ => OffInt,
    }
}

/// ClusteredIndexDefMode controls the default clustered property for primary key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClusteredIndexDefMode(pub i64);

/// ClusteredIndexDefModeIntOnly indicates only single int primary key will default be clustered.
pub const ClusteredIndexDefModeIntOnly: ClusteredIndexDefMode = ClusteredIndexDefMode(0);
/// ClusteredIndexDefModeOn indicates primary key will default be clustered.
pub const ClusteredIndexDefModeOn: ClusteredIndexDefMode = ClusteredIndexDefMode(1);
/// ClusteredIndexDefModeOff indicates primary key will default be non-clustered.
pub const ClusteredIndexDefModeOff: ClusteredIndexDefMode = ClusteredIndexDefMode(2);

/// TiDBOptEnableClustered converts enable clustered options to ClusteredIndexDefMode.
// go: TiDBOptEnableClustered L1
pub fn tidb_opt_enable_clustered(opt: &str) -> ClusteredIndexDefMode {
    match opt {
        On => ClusteredIndexDefModeOn,
        Off => ClusteredIndexDefModeOff,
        _ => ClusteredIndexDefModeIntOnly,
    }
}

/// AssertionLevel controls how strict the assertions on data mutations should be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AssertionLevel(pub i64);

/// AssertionLevelOff indicates no assertion should be performed.
pub const AssertionLevelOff: AssertionLevel = AssertionLevel(0);
/// AssertionLevelFast indicates assertions that doesn't affect performance should be performed.
pub const AssertionLevelFast: AssertionLevel = AssertionLevel(1);
/// AssertionLevelStrict indicates full assertions should be performed, even if the performance might be slowed down.
pub const AssertionLevelStrict: AssertionLevel = AssertionLevel(2);

// go: tidbOptAssertionLevel L1
pub fn tidb_opt_assertion_level(opt: &str) -> AssertionLevel {
    match opt {
        super::variable::AssertionStrictStr => AssertionLevelStrict,
        super::variable::AssertionFastStr => AssertionLevelFast,
        _ => AssertionLevelOff,
    }
}

// go: tidbOptPositiveInt32 L1
pub fn tidb_opt_positive_int32(opt: &str, default_val: isize) -> isize {
    let (val, err) = tidb_types::gostd::atoi(opt.as_bytes());
    if err.is_err() || val <= 0 {
        return default_val;
    }
    val
}

/// TidbOptInt converts a string to an int
// go: TidbOptInt L1
pub fn tidb_opt_int(opt: &str, default_val: isize) -> isize {
    let (val, err) = tidb_types::gostd::atoi(opt.as_bytes());
    if err.is_err() {
        return default_val;
    }
    val
}

/// TidbOptInt64 converts a string to an int64
// go: TidbOptInt64 L1
pub fn tidb_opt_int64(opt: &str, default_val: i64) -> i64 {
    let (val, err) = tidb_types::gostd::parse_int(opt.as_bytes(), 64);
    if err.is_err() {
        return default_val;
    }
    val
}

/// TidbOptUint64 converts a string to an uint64.
// go: TidbOptUint64 L1
pub fn tidb_opt_uint64(opt: &str, default_val: u64) -> u64 {
    let (val, err) = tidb_types::gostd::parse_uint(opt.as_bytes(), 64);
    if err.is_err() {
        return default_val;
    }
    val
}

// go: tidbOptFloat64 L1
pub fn tidb_opt_float64(opt: &str, default_val: f64) -> f64 {
    super::variable::parse_float64(opt).unwrap_or(default_val)
}

/// parseMemoryLimit: the byte size of a tidb_server_memory_limit value.
// go: parseMemoryLimit L1
pub(crate) fn parse_memory_limit(
    s: &mut SessionVars,
    normalized_value: &str,
    original_value: &str,
) -> (u64, String, Result<(), Error>) {
    let (mut byte_size, mut normalized_str, err) = (|| {
        let total = tidb_vars::mem_total_ignore_err();
        if total != 0 {
            let (perc, s) = parse_percentage(normalized_value);
            if perc != 0 {
                let int_val = total / 100 * perc;
                return (int_val, s, Ok(()));
            }
        }
        let (bt, s) = parse_byte_size(normalized_value);
        if !s.is_empty() {
            return (bt, s, Ok(()));
        }
        (
            0,
            String::new(),
            Err(ErrTruncatedWrongValue.gen_with_stack_by_args(vec![
                TiDBServerMemoryLimit.into(),
                original_value.to_owned().into(),
            ])),
        )
    })();
    // defer
    if err.is_ok() && byte_size > 0 && byte_size < (512 << 20) {
        s.stmt_ctx()
            .append_warning(ErrTruncatedWrongValue.fast_gen_by_args(vec![
                TiDBServerMemoryLimit.into(),
                original_value.to_owned().into(),
            ]));
        byte_size = 512 << 20;
        normalized_str = "512MB".to_owned();
    }
    (byte_size, normalized_str, err)
}

/// `fmt.Sscanf(s, "%d" + unit + "%s", &n, &rest)` returning `n == 1 && err == io.EOF`: a
/// decimal number (after optional white space, no sign), the literal unit, then only white
/// space to the end.
// go-std: fmt.Sscanf L2 (the "%d<lit>%s" formats of parseByteSize / parsePercentage)
fn sscanf_num_unit(s: &str, unit: &str) -> Option<u64> {
    // fmt's isSpace table (fmt/scan.go `space`).
    // go-std: fmt.isSpace L1
    fn is_space(r: char) -> bool {
        matches!(r as u32, 0x09..=0x0d | 0x20 | 0x85 | 0xa0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000)
    }
    // ss.SkipSpace with nlIsSpace = false (Sscanf): a newline is an error.
    // go-std: fmt.ss.SkipSpace L1 (nlIsSpace false)
    fn skip_space(s: &str) -> Option<&str> {
        let mut rest = s;
        loop {
            let mut it = rest.chars();
            let Some(r) = it.next() else {
                return Some(rest);
            };
            if r == '\r' && it.as_str().starts_with('\n') {
                rest = it.as_str();
                continue;
            }
            if r == '\n' {
                return None; // "unexpected newline"
            }
            if !is_space(r) {
                return Some(rest);
            }
            rest = it.as_str();
        }
    }
    // %d into a uint64: SkipSpace, notEOF, digits (no sign for unsigned), ParseUint.
    let rest = skip_space(s)?;
    if rest.is_empty() {
        return None; // io.EOF with n == 0
    }
    let ndig = rest.bytes().take_while(u8::is_ascii_digit).count();
    if ndig == 0 {
        return None; // "expected integer"
    }
    let (val, err) = tidb_types::gostd::parse_uint(&rest.as_bytes()[..ndig], 64);
    if err.is_err() {
        return None;
    }
    let mut rest = &rest[ndig..];
    if unit == "%" {
        // "%%" in the format: ss.advance matches it as a literal '%'; when the input has
        // something else, doScanf falls through to the verb path, which skips space and
        // then needs '%' (scanPercent) or ends with io.EOF (accepted: n == 1).
        if let Some(r) = rest.strip_prefix('%') {
            rest = r;
        } else if rest.is_empty() {
            return None; // advance: mustReadRune -> io.ErrUnexpectedEOF
        } else {
            let r = skip_space(rest)?;
            if r.is_empty() {
                return Some(val); // scanPercent: notEOF -> io.EOF
            }
            rest = r.strip_prefix('%')?; // "missing literal %"
        }
    } else {
        // The literal text of the format (rune by rune; EOF -> io.ErrUnexpectedEOF).
        for fc in unit.chars() {
            let mut it = rest.chars();
            match it.next() {
                Some(c) if c == fc => rest = it.as_str(),
                _ => return None,
            }
        }
    }
    // %s: SkipSpace, then EOF gives io.EOF with n == 1.
    let rest = skip_space(rest)?;
    if rest.is_empty() { Some(val) } else { None }
}

// go: parsePercentage L1
pub fn parse_percentage(s: &str) -> (u64, String) {
    let (mut percentage, mut normalized_str) = match sscanf_num_unit(s, "%") {
        Some(p) => (p, format!("{p}%")),
        None => (0, String::new()),
    };
    // defer
    if percentage == 0 || percentage >= 100 {
        percentage = 0;
        normalized_str = String::new();
    }
    (percentage, normalized_str)
}

// go: parseByteSize L1
pub fn parse_byte_size(s: &str) -> (u64, String) {
    if let Some(b) = sscanf_num_unit(s, "") {
        return (b, format!("{b}"));
    }
    for (unit, shift) in [
        ("KB", 10),
        ("KiB", 10),
        ("MB", 20),
        ("MiB", 20),
        ("GB", 30),
        ("GiB", 30),
        ("TB", 40),
        ("TiB", 40),
    ] {
        if let Some(b) = sscanf_num_unit(s, unit) {
            // [M-INT] byteSize << n wraps like Go's uint64 shift.
            return (b.wrapping_shl(shift), format!("{b}{unit}"));
        }
    }
    (0, String::new())
}

/// GAFunction4ExpressionIndex stores functions GA for expression index.
pub static GAFunction4ExpressionIndex: &[&str] = &[
    "lower",
    "upper",
    "md5",
    "reverse",
    "vitess_hash",
    "tidb_shard",
    // JSON functions.
    "json_type",
    "json_extract",
    "json_unquote",
    "json_array",
    "json_object",
    "json_set",
    "json_insert",
    "json_replace",
    "json_remove",
    "json_contains",
    "json_contains_path",
    "json_valid",
    "json_array_append",
    "json_array_insert",
    "json_merge_patch",
    "json_merge_preserve",
    "json_pretty",
    "json_quote",
    "json_schema_valid",
    "json_search",
    "json_storage_size",
    "json_depth",
    "json_keys",
    "json_length",
];

// go: collectAllowFuncName4ExpressionIndex L1
pub fn collect_allow_func_name4_expression_index() -> String {
    let mut s: Vec<&str> = GAFunction4ExpressionIndex.to_vec();
    s.sort_unstable();
    s.join(", ")
}

// go: updatePasswordValidationLength L1
pub(crate) fn update_password_validation_length(
    s: &mut SessionVars,
    length: i32,
) -> Result<(), Error> {
    s.global_vars_accessor.set_global_sys_var_only(
        ValidatePasswordLength,
        &(length as i64).to_string(),
        false,
    )?;
    PasswordValidationLength.store(length, SeqCst);
    Ok(())
}

static analyzeSkipAllowedTypes: &[&str] = &[
    "json",
    "text",
    "mediumtext",
    "longtext",
    "blob",
    "mediumblob",
    "longblob",
];

/// ValidAnalyzeSkipColumnTypes makes validation for tidb_analyze_skip_column_types.
// go: ValidAnalyzeSkipColumnTypes L1
pub fn valid_analyze_skip_column_types(val: &str) -> ValResult {
    if val.is_empty() {
        return (String::new(), Ok(()));
    }
    let lower = gostd::to_lower(val);
    let mut column_types: Vec<&str> = Vec::new();
    for item in lower.split(',') {
        let column_type = item.trim_matches(|c: char| gostd_is_space(c));
        if !analyzeSkipAllowedTypes.contains(&column_type) {
            return (
                val.to_owned(),
                Err(ErrWrongValueForVar.gen_with_stack_by_args(vec![
                    TiDBAnalyzeSkipColumnTypes.into(),
                    val.to_owned().into(),
                ])),
            );
        }
        column_types.push(column_type);
    }
    (column_types.join(","), Ok(()))
}

/// `unicode.IsSpace` (strings.TrimSpace).
// go-std: unicode.IsSpace L1
pub(crate) fn gostd_is_space(r: char) -> bool {
    matches!(r as u32, 0x09..=0x0d | 0x20 | 0x85 | 0xa0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000)
}

/// ParseAnalyzeSkipColumnTypes converts tidb_analyze_skip_column_types to the map form.
// go: ParseAnalyzeSkipColumnTypes L1
pub fn parse_analyze_skip_column_types(val: &str) -> BTreeSet<String> {
    let mut skip_types = BTreeSet::new();
    for column_type in gostd::to_lower(val).split(',') {
        if analyzeSkipAllowedTypes.contains(&column_type) {
            skip_types.insert(column_type.to_owned());
        }
    }
    skip_types
}

/// SchemaCacheSizeLowerBound will adjust the schema cache size to this value if
/// it is lower than this value.
pub const SchemaCacheSizeLowerBound: u64 = 64 << 20;
/// SchemaCacheSizeLowerBoundStr is the string version of SchemaCacheSizeLowerBound.
pub const SchemaCacheSizeLowerBoundStr: &str = "64MB";

// go: parseSchemaCacheSize L1
pub(crate) fn parse_schema_cache_size(
    s: &mut SessionVars,
    normalized_value: &str,
    original_value: &str,
) -> (u64, String, Result<(), Error>) {
    let (mut byte_size, mut normalized_str, err) = {
        let (bt, st) = parse_byte_size(normalized_value);
        if !st.is_empty() {
            (bt, st, Ok(()))
        } else {
            (
                0,
                String::new(),
                Err(ErrTruncatedWrongValue.gen_with_stack_by_args(vec![
                    TiDBSchemaCacheSize.into(),
                    original_value.to_owned().into(),
                ])),
            )
        }
    };
    // defer
    if err.is_ok() && byte_size > 0 && byte_size < SchemaCacheSizeLowerBound {
        s.stmt_ctx()
            .append_warning(ErrTruncatedWrongValue.fast_gen_by_args(vec![
                TiDBSchemaCacheSize.into(),
                original_value.to_owned().into(),
            ]));
        byte_size = SchemaCacheSizeLowerBound;
        normalized_str = SchemaCacheSizeLowerBoundStr.to_owned();
    }
    if err.is_ok() && byte_size > i64::MAX as u64 {
        s.stmt_ctx()
            .append_warning(ErrTruncatedWrongValue.fast_gen_by_args(vec![
                TiDBSchemaCacheSize.into(),
                original_value.to_owned().into(),
            ]));
        byte_size = i64::MAX as u64;
        normalized_str = i64::MAX.to_string();
    }
    (byte_size, normalized_str, err)
}
