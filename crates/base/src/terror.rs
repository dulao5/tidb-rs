//! go-file: pkg/parser/terror/terror.go
//!
//! Error classes and the `*Error` constructors of TiDB.
//!
//! Go registers classes (`RegisterErrorClass`) and the codes each class uses
//! (`ErrClass.initError`, called by `New`/`NewStd`/`NewStdErr`) at package initialisation of
//! every package linked into tidb-server, then freezes the registry (`RegisterFinish`, called
//! by `cmd/tidb-server`; registering afterwards panics). The registry is therefore a fixed
//! table for a given binary. The port keeps it as a generated static table
//! ([`ErrClassToMySQLCodes`], from the Go v8.5.8 tidb-server package set, see
//! `terror_registry.rs`); `initError` checks that the code is in it instead of inserting.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::errors::{self, ErrCode, Error, IntoErrCode};
use crate::gomap::GoMap;
use crate::mysql::{self, ErrMessage};

pub use crate::terror_registry::ErrClassToMySQLCodes;

// Executor error codes.

/// CodeUnknown is for errors of unknown reason.
pub const CodeUnknown: ErrCode = -1;
/// CodeExecResultIsEmpty indicates execution result is empty.
pub const CodeExecResultIsEmpty: ErrCode = 3;

// Expression error codes.

/// CodeMissConnectionID indicates connection id is missing.
pub const CodeMissConnectionID: ErrCode = 1;

// Special error codes.

/// CodeResultUndetermined indicates the sql execution result is undetermined.
pub const CodeResultUndetermined: ErrCode = 2;

/// ErrClass represents a class of errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ErrClass(pub isize);

// Error classes.
pub const ClassAutoid: ErrClass = register_error_class(1, "autoid");
pub const ClassDDL: ErrClass = register_error_class(2, "ddl");
pub const ClassDomain: ErrClass = register_error_class(3, "domain");
pub const ClassEvaluator: ErrClass = register_error_class(4, "evaluator");
pub const ClassExecutor: ErrClass = register_error_class(5, "executor");
pub const ClassExpression: ErrClass = register_error_class(6, "expression");
pub const ClassAdmin: ErrClass = register_error_class(7, "admin");
pub const ClassKV: ErrClass = register_error_class(8, "kv");
pub const ClassMeta: ErrClass = register_error_class(9, "meta");
pub const ClassOptimizer: ErrClass = register_error_class(10, "planner");
pub const ClassParser: ErrClass = register_error_class(11, "parser");
pub const ClassPerfSchema: ErrClass = register_error_class(12, "perfschema");
pub const ClassPrivilege: ErrClass = register_error_class(13, "privilege");
pub const ClassSchema: ErrClass = register_error_class(14, "schema");
pub const ClassServer: ErrClass = register_error_class(15, "server");
pub const ClassStructure: ErrClass = register_error_class(16, "structure");
pub const ClassVariable: ErrClass = register_error_class(17, "variable");
pub const ClassXEval: ErrClass = register_error_class(18, "xeval");
pub const ClassTable: ErrClass = register_error_class(19, "table");
pub const ClassTypes: ErrClass = register_error_class(20, "types");
pub const ClassGlobal: ErrClass = register_error_class(21, "global");
pub const ClassMockTikv: ErrClass = register_error_class(22, "mocktikv");
pub const ClassJSON: ErrClass = register_error_class(23, "json");
pub const ClassTiKV: ErrClass = register_error_class(24, "tikv");
pub const ClassSession: ErrClass = register_error_class(25, "session");
pub const ClassPlugin: ErrClass = register_error_class(26, "plugin");
pub const ClassUtil: ErrClass = register_error_class(27, "util");
// Add more as needed.

/// `errClass2Desc`: every class registered above, by code.
static ErrClass2Desc: GoMap<ErrClass, &str> = GoMap::new(ERR_CLASS_2_DESC);

/// `code2ErrClassMap`: class description -> class (Go: a `sync.Map` filled by
/// `initError`). It holds exactly the classes that registered at least one error code,
/// which is the key set of [`ErrClassToMySQLCodes`].
pub struct code2ErrClassMap;

/// `rfcCode2errClass`.
static rfcCode2errClass: code2ErrClassMap = new_code2_err_class_map();

// go: newCode2ErrClassMap L2 (static registry, see module doc)
const fn new_code2_err_class_map() -> code2ErrClassMap {
    code2ErrClassMap
}

impl code2ErrClassMap {
    // go: code2ErrClassMap.Get L2 (static registry, see module doc)
    pub fn get(&self, key: &str) -> (ErrClass, bool) {
        for (class, _) in ErrClassToMySQLCodes.iter() {
            if ErrClass2Desc.get(class).is_some_and(|d| *d == key) {
                return (*class, true);
            }
        }
        (ErrClass(-1), false)
    }

    /// Registration happens at generation time; this only checks that the class is known.
    // go: code2ErrClassMap.Put L2 (static registry, see module doc)
    pub fn put(&self, key: &str, err: ErrClass) {
        debug_assert_eq!(
            self.get(key),
            (err, true),
            "class {key} has no registered error code"
        );
    }
}

static REGISTER_FINISH: AtomicU32 = AtomicU32::new(0);

/// RegisterFinish makes the register of new error panic.
/// The use pattern should be register all the errors during initialization, and then call
/// RegisterFinish.
// go: RegisterFinish L1
pub fn register_finish() {
    REGISTER_FINISH.store(1, Ordering::SeqCst);
}

// go: frozen L1
fn frozen() -> bool {
    REGISTER_FINISH.load(Ordering::SeqCst) != 0
}

/// RegisterErrorClass registers new error class for terror. The class table is static
/// (`ErrClass2Desc`); a class that is not in it, or with a different description, fails to
/// compile (Go: duplicate registration panics at init).
// go: RegisterErrorClass L2 (static class table checked at compile time)
pub const fn register_error_class(class_code: isize, desc: &str) -> ErrClass {
    let err_class = ErrClass(class_code);
    let entries = ERR_CLASS_2_DESC;
    let mut i = 0;
    while i < entries.len() {
        if entries[i].0.0 == class_code {
            let d = entries[i].1.as_bytes();
            let w = desc.as_bytes();
            assert!(
                d.len() == w.len(),
                "register_error_class: description differs from ErrClass2Desc"
            );
            let mut j = 0;
            while j < d.len() {
                assert!(
                    d[j] == w[j],
                    "register_error_class: description differs from ErrClass2Desc"
                );
                j += 1;
            }
            return err_class;
        }
        i += 1;
    }
    panic!("register_error_class: class not in ErrClass2Desc");
}

/// The class table (`errClass2Desc` after the registrations above), sorted by class.
const ERR_CLASS_2_DESC: &[(ErrClass, &str)] = &[
    (ErrClass(1), "autoid"),
    (ErrClass(2), "ddl"),
    (ErrClass(3), "domain"),
    (ErrClass(4), "evaluator"),
    (ErrClass(5), "executor"),
    (ErrClass(6), "expression"),
    (ErrClass(7), "admin"),
    (ErrClass(8), "kv"),
    (ErrClass(9), "meta"),
    (ErrClass(10), "planner"),
    (ErrClass(11), "parser"),
    (ErrClass(12), "perfschema"),
    (ErrClass(13), "privilege"),
    (ErrClass(14), "schema"),
    (ErrClass(15), "server"),
    (ErrClass(16), "structure"),
    (ErrClass(17), "variable"),
    (ErrClass(18), "xeval"),
    (ErrClass(19), "table"),
    (ErrClass(20), "types"),
    (ErrClass(21), "global"),
    (ErrClass(22), "mocktikv"),
    (ErrClass(23), "json"),
    (ErrClass(24), "tikv"),
    (ErrClass(25), "session"),
    (ErrClass(26), "plugin"),
    (ErrClass(27), "util"),
];

impl ErrClass {
    /// String implements fmt.Stringer interface.
    // go: ErrClass.String L1 [M-NAME]
    pub fn string(self) -> Cow<'static, str> {
        if let Some(s) = ErrClass2Desc.get(&self) {
            return Cow::Borrowed(s);
        }
        Cow::Owned(self.0.to_string())
    }

    /// EqualClass returns true if err is *Error with the same class.
    // go: ErrClass.EqualClass L1 [M-ERR]
    pub fn equal_class(self, err: Option<&Error>) -> bool {
        let Some(e) = err.map(errors::cause) else {
            return false;
        };
        if e.is_normalized() {
            let rfc_code = e.rfc_code();
            if let Some(index) = rfc_code.find(':')
                && index > 0
            {
                let (class, has) = rfcCode2errClass.get(&rfc_code[..index]);
                if has {
                    return class == self;
                }
            }
        }
        false
    }

    /// NotEqualClass returns true if err is not *Error with the same class.
    // go: ErrClass.NotEqualClass L1
    pub fn not_equal_class(self, err: Option<&Error>) -> bool {
        !self.equal_class(err)
    }

    // go: ErrClass.initError L2 (static registry: checks instead of inserting, see module doc)
    fn init_error(self, code: ErrCode) -> String {
        if frozen() {
            panic!("register error after initialized is prohibited");
        }
        debug_assert!(
            ErrClassToMySQLCodes
                .get(&self)
                .is_some_and(|codes| codes.binary_search(&code).is_ok()),
            "error code {code} of class {} is not in ErrClassToMySQLCodes (regenerate terror_registry.rs)",
            self.0
        );
        let class = ErrClass2Desc.get(&self).copied().unwrap_or("");
        let rfc_code = format!("{class}:{code}");
        rfcCode2errClass.put(class, self);
        rfc_code
    }

    /// New defines an *Error with an error code and an error message.
    /// Usually used to create base *Error.
    ///
    /// Deprecated: use NewStd or NewStdErr instead.
    // go: ErrClass.New L1
    #[allow(clippy::new_ret_no_self)] // Go's method name (it returns an *Error)
    pub fn new(self, code: impl IntoErrCode, message: &'static str) -> Error {
        let code = code.into_err_code();
        let rfc_code = self.init_error(code);
        errors::normalize(
            message,
            [
                errors::mysql_error_code(code),
                errors::rfc_code_text(rfc_code),
            ],
        )
    }

    /// NewStdErr defines an *Error with an error code, an error
    /// message and workaround to create standard error.
    // go: ErrClass.NewStdErr L1
    pub fn new_std_err(self, code: impl IntoErrCode, message: &ErrMessage) -> Error {
        let code = code.into_err_code();
        let rfc_code = self.init_error(code);
        errors::normalize(
            message.raw,
            [
                errors::redact_args(message.redact_arg_pos),
                errors::mysql_error_code(code),
                errors::rfc_code_text(rfc_code),
            ],
        )
    }

    /// NewStd calls New using the standard message for the error code. A code without a
    /// message in `mysql.MySQLErrName` panics (Go dereferences a nil `*ErrMessage`).
    // go: ErrClass.NewStd L1
    pub fn new_std(self, code: impl IntoErrCode) -> Error {
        let code = code.into_err_code();
        let msg = mysql::MySQLErrName
            .get(&(code as u16))
            .unwrap_or_else(|| panic!("invalid memory address or nil pointer dereference"));
        self.new_std_err(code, msg)
    }

    /// Synthesize synthesizes an *Error in the air
    /// it didn't register error into ErrClassToMySQLCodes
    /// so it's goroutine-safe
    /// and often be used to create Error came from other systems like TiKV.
    // go: ErrClass.Synthesize L1
    pub fn synthesize(
        self,
        code: impl IntoErrCode,
        message: impl Into<Cow<'static, str>>,
    ) -> Error {
        let code = code.into_err_code();
        let desc = ErrClass2Desc.get(&self).copied().unwrap_or("");
        errors::normalize(
            message,
            [
                errors::mysql_error_code(code),
                errors::rfc_code_text(format!("{desc}:{code}")),
            ],
        )
    }
}

/// ToSQLError convert Error to mysql.SQLError.
// go: ToSQLError L1
pub fn to_sql_error(e: &Error) -> mysql::SQLError {
    let code = get_mysql_error_code(e);
    mysql::new_errf(code, "%s", &[], crate::go_args![e.get_msg()])
}

/// `defaultMySQLErrorCode` (set to `mysql.ErrUnknown` by `init`).
const defaultMySQLErrorCode: u16 = init();

// go: init L2 (constant initialiser of defaultMySQLErrorCode)
const fn init() -> u16 {
    mysql::ErrUnknown
}

// go: getMySQLErrorCode L1 [M-INT]
fn get_mysql_error_code(e: &Error) -> u16 {
    let rfc_code = e.rfc_code();
    let mut class = ErrClass(0);
    if let Some(index) = rfc_code.find(':')
        && index > 0
    {
        let (ec, has) = rfcCode2errClass.get(&rfc_code[..index]);
        if !has {
            log::warn!("Unknown error class; class={}", &rfc_code[..index]);
            return defaultMySQLErrorCode;
        }
        class = ec;
    }
    let Some(code_map) = ErrClassToMySQLCodes.get(&class) else {
        log::warn!("Unknown error class; class={}", class.0);
        return defaultMySQLErrorCode;
    };
    if code_map.binary_search(&e.code()).is_err() {
        log::debug!("Unknown error code; class={} code={}", class.0, e.code());
        return defaultMySQLErrorCode;
    }
    e.code() as u16
}

/// ErrCritical is the critical error class.
pub static ErrCritical: std::sync::LazyLock<Error> = std::sync::LazyLock::new(|| {
    ClassGlobal.new_std_err(
        CodeExecResultIsEmpty,
        &mysql::message("critical error %v", &[]),
    )
});

/// ErrResultUndetermined is the error when execution result is unknown.
pub static ErrResultUndetermined: std::sync::LazyLock<Error> = std::sync::LazyLock::new(|| {
    ClassGlobal.new_std_err(
        CodeResultUndetermined,
        &mysql::message("execution result undetermined", &[]),
    )
});

/// ErrorEqual returns a boolean indicating whether err1 is equal to err2.
// go: ErrorEqual L1 [M-ERR]
pub fn error_equal(err1: Option<&Error>, err2: Option<&Error>) -> bool {
    let (e1, e2) = match (err1.map(errors::cause), err2.map(errors::cause)) {
        (None, None) => return true,
        (Some(a), Some(b)) => (a, b),
        _ => return false,
    };
    if e1.is_normalized() && e2.is_normalized() {
        return e1.rfc_code() == e2.rfc_code();
    }
    // e1 == e2 (identity) is covered by comparing the texts.
    e1.error() == e2.error()
}

/// ErrorNotEqual returns a boolean indicating whether err1 isn't equal to err2.
// go: ErrorNotEqual L1
pub fn error_not_equal(err1: Option<&Error>, err2: Option<&Error>) -> bool {
    !error_equal(err1, err2)
}

/// MustNil cleans up and fatals if err is not nil.
// go: MustNil L1 [M-DEFER]
pub fn must_nil(err: Option<&Error>, close_funs: &mut [&mut dyn FnMut()]) {
    if let Some(err) = err {
        for f in close_funs.iter_mut() {
            f();
        }
        log::error!(
            "unexpected error; error={err}; stack={}",
            std::backtrace::Backtrace::force_capture()
        );
        // log.Fatal exits the process.
        std::process::exit(1);
    }
}

/// Call executes a function and checks the returned err.
// go: Call L1
pub fn call(f: impl FnOnce() -> Result<(), Error>) {
    if let Err(err) = f() {
        log::error!(
            "function call errored; error={err}; stack={}",
            std::backtrace::Backtrace::capture()
        );
    }
}

/// Log logs the error if it is not nil.
// go: Log L1
pub fn log(err: Option<&Error>) {
    if let Some(err) = err {
        log::error!(
            "encountered error; error={err}; stack={}",
            std::backtrace::Backtrace::capture()
        );
    }
}

/// GetErrClass returns the error class of the error.
// go: GetErrClass L1
pub fn get_err_class(e: &Error) -> ErrClass {
    let rfc_code = e.rfc_code();
    if let Some(index) = rfc_code.find(':')
        && index > 0
    {
        let (class, has) = rfcCode2errClass.get(&rfc_code[..index]);
        if has {
            return class;
        }
    }
    ErrClass(-1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_tables() {
        assert!(ErrClass2Desc.is_sorted());
        assert!(ErrClassToMySQLCodes.is_sorted());
        for (_, codes) in ErrClassToMySQLCodes.iter() {
            assert!(codes.windows(2).all(|w| w[0] < w[1]));
        }
        assert_eq!(ErrClass2Desc.len(), 27);
    }

    #[test]
    fn critical() {
        assert_eq!(ErrCritical.error(), "[global:3]critical error %v");
        assert_eq!(
            ErrCritical
                .gen_with_stack_by_args(crate::go_args!["x"])
                .error(),
            "[global:3]critical error x"
        );
    }
}
