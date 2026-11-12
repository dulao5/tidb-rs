//! go-file: pkg/expression/builtin_registry.go
//! go-file: pkg/expression/builtin.go
//!
//! The `funcs` table of `builtin.go`: function name → function class. Each entry carries the
//! class's `baseFunctionClass` (name for errors, argument counts) and the display name of
//! the two classes with `getDisplayName`; the entries are generated from the Go source
//! (`tools/expr-gen/gen_registry.py`, [`crate::builtin_registry_gen`]).
//!
//! A class's `getFunction` (choosing the signature) belongs to its function family's block
//! (D08.4–D08.9) and is added to the entry as that family is ported; `NewFunction*`
//! (`scalar_function.go`, D08.1) comes with the first family (spec §3.2).
//!
//! tidb-rs has no function extensions (`pkg/extension`), so the extension functions Go adds
//! to `GetBuiltinList` are none (spec §4).

use std::collections::HashMap;
use std::sync::LazyLock;

use tidb_base::Error;

use crate::ast_funcs as ast;
use crate::builtin::BaseFunctionClass;
use crate::builtin_registry_gen::FUNCS;

/// An entry of `funcs`.
#[derive(Debug)]
pub struct FuncClassEntry {
    /// The map key (`ast.Xxx`).
    pub name: &'static str,
    /// The Go type of the function class, e.g. `coalesceFunctionClass`.
    pub class: &'static str,
    /// The class's `baseFunctionClass`.
    pub base: BaseFunctionClass,
    /// `getDisplayName()` of the classes implementing `functionClassWithName`.
    pub display_name: Option<&'static str>,
}

/// `funcs` by name.
static FUNCS_BY_NAME: LazyLock<HashMap<&'static str, &'static FuncClassEntry>> =
    LazyLock::new(|| FUNCS.iter().map(|e| (e.name, e)).collect());

/// `funcs[name]`.
// glue: map lookup in funcs
pub fn lookup(name: &str) -> Option<&'static FuncClassEntry> {
    FUNCS_BY_NAME.get(name).copied()
}

/// RegisteredBuiltinFunctionNames returns a sorted snapshot of builtin function names.
// go: RegisteredBuiltinFunctionNames L1
pub fn registered_builtin_function_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = FUNCS.iter().map(|e| e.name).collect();
    names.sort_unstable();
    names
}

/// VerifyArgsWrapper verifies a function by its name and the count of the arguments.
/// Note that this function assumes that the function is supported.
// go: pkg/expression/builtin.go VerifyArgsWrapper L1
pub fn verify_args_wrapper(name: &str, l: usize) -> Result<(), Error> {
    match lookup(name) {
        None => Ok(()),
        Some(f) => f.base.verify_args_by_count(l),
    }
}

/// IsFunctionSupported check if given function name is a builtin sql function.
// go: pkg/expression/builtin.go IsFunctionSupported L1
pub fn is_function_supported(name: &str) -> bool {
    lookup(name).is_some()
}

/// GetDisplayName translate a function name to its display name
// go: pkg/expression/builtin.go GetDisplayName L1
pub fn get_display_name(name: &str) -> &str {
    if let Some(f) = lookup(name)
        && let Some(d) = f.display_name
    {
        return d;
    }
    name
}

/// GetBuiltinList returns a list of builtin functions. Go appends the registered extension
/// functions; tidb-rs has none.
// go: pkg/expression/builtin.go GetBuiltinList L2 (no extension functions)
pub fn get_builtin_list() -> Vec<&'static str> {
    let not_implemented_functions = [ast::RowFunc, ast::IsTruthWithNull];
    let mut res = Vec::with_capacity(FUNCS.len());
    for e in FUNCS {
        let func_name = e.name;
        // Skip not implemented functions
        let mut skip_func = not_implemented_functions.contains(&func_name);
        // Skip literal functions
        // (their names are not readable: 'tidb`.(dateliteral, for example)
        // See: https://github.com/pingcap/parser/pull/591
        if func_name.starts_with("'tidb`.(") {
            skip_func = true;
        }
        if skip_func {
            continue;
        }
        res.push(func_name);
    }
    res.sort_unstable();
    res
}

/// `functionClass.getFunction` of the classes in `funcs`: builds the signature for the
/// arguments.
// glue: Go interface functionClass (the getFunction method; verifyArgs is BaseFunctionClass)
pub trait FunctionClass: Sync {
    /// `getFunction`.
    // glue: method of the functionClass interface (each class's getFunction is annotated where it is ported)
    fn get_function(
        &self,
        ctx: &dyn crate::context::BuildContext,
        args: Vec<crate::expression::ExprRef>,
    ) -> Result<crate::eval::BuiltinFunc, Error>;
}

/// The ported `getFunction` of `funcs[name]`, `None` while the function family is not
/// ported (D08.3–D08.9; the classes are added to this table as their families land).
/// CAST, `from_binary`, `to_binary` and `getvar` are not in `funcs`: `newFunctionImpl`
/// builds them directly.
// glue: dispatch of funcs[name].getFunction over the ported classes
pub fn function_class(name: &str) -> Option<&'static dyn FunctionClass> {
    PORTED_FUNCTION_CLASSES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| *c)
}

/// The ported classes of `funcs`, by function name. Every `impl FunctionClass for` in this
/// crate must have an entry (test `function_class_registry`; Lucky, review of #95).
// glue: the ported part of Go's funcs map
pub static PORTED_FUNCTION_CLASSES: &[(&str, &(dyn FunctionClass + 'static))] = &[
    (ast::LT, &crate::builtin_compare::LT_CLASS),
    (ast::LE, &crate::builtin_compare::LE_CLASS),
    (ast::GT, &crate::builtin_compare::GT_CLASS),
    (ast::GE, &crate::builtin_compare::GE_CLASS),
    (ast::EQ, &crate::builtin_compare::EQ_CLASS),
    (ast::NE, &crate::builtin_compare::NE_CLASS),
    (ast::NullEQ, &crate::builtin_compare::NULL_EQ_CLASS),
    (ast::Plus, &crate::builtin_arithmetic::PLUS_CLASS),
    (ast::Minus, &crate::builtin_arithmetic::MINUS_CLASS),
    (ast::Mul, &crate::builtin_arithmetic::MULTIPLY_CLASS),
    (ast::Div, &crate::builtin_arithmetic::DIVIDE_CLASS),
    (ast::IntDiv, &crate::builtin_arithmetic::INT_DIVIDE_CLASS),
    (ast::Mod, &crate::builtin_arithmetic::MOD_CLASS),
    (ast::Ceil, &crate::builtin_math::CEIL_CLASS),
    (ast::Ceiling, &crate::builtin_math::CEILING_CLASS),
    (ast::Floor, &crate::builtin_math::FLOOR_CLASS),
    (ast::LogicAnd, &crate::builtin_op::LOGIC_AND_CLASS),
    (ast::LogicOr, &crate::builtin_op::LOGIC_OR_CLASS),
    (ast::LogicXor, &crate::builtin_op::LOGIC_XOR_CLASS),
    (ast::UnaryNot, &crate::builtin_op::UNARY_NOT_CLASS),
    (ast::UnaryMinus, &crate::builtin_op::UNARY_MINUS_CLASS),
    (
        ast::IsTruthWithoutNull,
        &crate::builtin_op::IS_TRUTH_WITHOUT_NULL_CLASS,
    ),
    (
        ast::IsTruthWithNull,
        &crate::builtin_op::IS_TRUTH_WITH_NULL_CLASS,
    ),
    (ast::IsFalsity, &crate::builtin_op::IS_FALSITY_CLASS),
];
