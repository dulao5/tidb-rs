//! go-file: pkg/kv/checker.go

// Go identifiers for the constants (M-GONAME).
#![allow(non_upper_case_globals)]

use tipb::ExprType;

use crate::kv::ReqSubTypeBasic;
use crate::kv::ReqSubTypeDesc;
use crate::kv::ReqSubTypeGroupBy;
use crate::kv::ReqSubTypeSignature;
use crate::kv::ReqSubTypeTopN;
use crate::kv::ReqTypeAnalyze;
use crate::kv::ReqTypeDAG;
use crate::kv::ReqTypeIndex;
use crate::kv::ReqTypeSelect;

/// RequestTypeSupportedChecker is used to check expression can be pushed down.
#[derive(Clone, Copy, Debug, Default)]
pub struct RequestTypeSupportedChecker;

impl RequestTypeSupportedChecker {
    /// IsRequestTypeSupported checks whether reqType is supported.
    // go: RequestTypeSupportedChecker.IsRequestTypeSupported L1 [M-INT]
    pub fn is_request_type_supported(self, req_type: i64, sub_type: i64) -> bool {
        match req_type {
            ReqTypeSelect | ReqTypeIndex => match sub_type {
                ReqSubTypeGroupBy | ReqSubTypeBasic | ReqSubTypeTopN => true,
                // Go: tipb.ExprType(subType), an int32 conversion that truncates.
                _ => self.support_expr(sub_type as i32),
            },
            ReqTypeDAG => self.support_expr(sub_type as i32),
            ReqTypeAnalyze => true,
            _ => false,
        }
    }

    /// TODO: deprecate this function, because:
    /// 1. we have more than one storage engine available for push-down.
    /// 2. we'd better do an accurate push-down check at the planner stage, instead of here.
    ///
    /// currently, we do aggregation push-down check in `CheckAggCanPushCop`.
    ///
    /// Go's `tipb.ExprType` is an int32 that may hold any value (the request sub-types are
    /// compared too), so the port matches on the integer.
    // go: RequestTypeSupportedChecker.supportExpr L1
    fn support_expr(self, expr_type: i32) -> bool {
        const NULL: i32 = ExprType::Null as i32;
        const INT64: i32 = ExprType::Int64 as i32;
        const UINT64: i32 = ExprType::Uint64 as i32;
        const STRING: i32 = ExprType::String as i32;
        const BYTES: i32 = ExprType::Bytes as i32;
        const MYSQL_DURATION: i32 = ExprType::MysqlDuration as i32;
        const MYSQL_TIME: i32 = ExprType::MysqlTime as i32;
        const MYSQL_DECIMAL: i32 = ExprType::MysqlDecimal as i32;
        const FLOAT32: i32 = ExprType::Float32 as i32;
        const FLOAT64: i32 = ExprType::Float64 as i32;
        const COLUMN_REF: i32 = ExprType::ColumnRef as i32;
        const MYSQL_ENUM: i32 = ExprType::MysqlEnum as i32;
        const MYSQL_BIT: i32 = ExprType::MysqlBit as i32;
        const TIDB_VECTOR_FLOAT32: i32 = ExprType::TiDbVectorFloat32 as i32;
        const COUNT: i32 = ExprType::Count as i32;
        const FIRST: i32 = ExprType::First as i32;
        const MAX: i32 = ExprType::Max as i32;
        const MIN: i32 = ExprType::Min as i32;
        const SUM: i32 = ExprType::Sum as i32;
        const AVG: i32 = ExprType::Avg as i32;
        const AGG_BIT_XOR: i32 = ExprType::AggBitXor as i32;
        const AGG_BIT_AND: i32 = ExprType::AggBitAnd as i32;
        const AGG_BIT_OR: i32 = ExprType::AggBitOr as i32;
        const APPROX_COUNT_DISTINCT: i32 = ExprType::ApproxCountDistinct as i32;
        const GROUP_CONCAT: i32 = ExprType::GroupConcat as i32;
        const ROW_NUMBER: i32 = ExprType::RowNumber as i32;
        const RANK: i32 = ExprType::Rank as i32;
        const DENSE_RANK: i32 = ExprType::DenseRank as i32;
        const CUME_DIST: i32 = ExprType::CumeDist as i32;
        const PERCENT_RANK: i32 = ExprType::PercentRank as i32;
        const NTILE: i32 = ExprType::Ntile as i32;
        const LEAD: i32 = ExprType::Lead as i32;
        const LAG: i32 = ExprType::Lag as i32;
        const FIRST_VALUE: i32 = ExprType::FirstValue as i32;
        const LAST_VALUE: i32 = ExprType::LastValue as i32;
        const NTH_VALUE: i32 = ExprType::NthValue as i32;
        const DESC: i32 = ReqSubTypeDesc as i32;
        const SIGNATURE: i32 = ReqSubTypeSignature as i32;
        match expr_type {
            NULL | INT64 | UINT64 | STRING | BYTES | MYSQL_DURATION | MYSQL_TIME
            | MYSQL_DECIMAL | FLOAT32 | FLOAT64 | COLUMN_REF | MYSQL_ENUM | MYSQL_BIT
            | TIDB_VECTOR_FLOAT32 => true,
            // aggregate functions.
            // NOTE: tipb.ExprType_GroupConcat is only supported by TiFlash, So checking it for
            // TiKV case outside.
            COUNT
            | FIRST
            | MAX
            | MIN
            | SUM
            | AVG
            | AGG_BIT_XOR
            | AGG_BIT_AND
            | AGG_BIT_OR
            | APPROX_COUNT_DISTINCT
            | GROUP_CONCAT => true,
            // window functions.
            ROW_NUMBER | RANK | DENSE_RANK | CUME_DIST | PERCENT_RANK | NTILE | LEAD | LAG
            | FIRST_VALUE | LAST_VALUE | NTH_VALUE => true,
            DESC => true,
            SIGNATURE => true,
            _ => false,
        }
    }
}
