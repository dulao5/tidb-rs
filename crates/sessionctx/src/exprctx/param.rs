//! go-file: pkg/expression/exprctx/param.go

// Names keep their Go spelling (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::{Arc, LazyLock};

use tidb_base::Error;
use tidb_types::datum::Datum;

/// ErrParamIndexExceedParamCounts describes the error that the index exceed the count of all
/// params.
// glue: the package-level `errors.New` value ErrParamIndexExceedParamCounts, a fresh copy (M-ERR)
pub fn err_param_index_exceed_param_counts() -> Error {
    tidb_base::errors::new("Param index exceed param counts")
}

/// ParamValues is used to get param values for param expression in prepared statement.
pub trait ParamValues {
    /// GetParamValue returns the value of the parameter by index.
    // glue: interface method of exprctx.ParamValues
    fn get_param_value(&self, idx: i64) -> Result<Datum, Error>;
}

/// EmptyParamValues is the `ParamValues` which contains nothing.
pub static EmptyParamValues: LazyLock<Arc<dyn ParamValues + Send + Sync>> =
    LazyLock::new(|| Arc::new(EmptyParamValuesImpl));

/// emptyParamValues is the `ParamValues` which contains nothing.
struct EmptyParamValuesImpl;

impl ParamValues for EmptyParamValuesImpl {
    /// GetParamValue always returns the `ErrParamIndexExceedParamCounts` for any index.
    // go: emptyParamValues.GetParamValue L1
    fn get_param_value(&self, _idx: i64) -> Result<Datum, Error> {
        Err(err_param_index_exceed_param_counts())
    }
}
