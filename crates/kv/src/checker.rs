//! go-file: pkg/kv/checker.go

// Go identifiers for the constants (M-GONAME).
#![allow(non_upper_case_globals)]

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
    /// compared too), so the port matches on the integer. The values are tipb's
    /// (`proto/expression.proto` at the revision TiDB v8.5.8 uses); the crate does not
    /// depend on `tipb` (its Rust crate builds gRPC's C++ core).
    // go: RequestTypeSupportedChecker.supportExpr L1
    fn support_expr(self, expr_type: i32) -> bool {
        const NULL: i32 = 0;
        const INT64: i32 = 1;
        const UINT64: i32 = 2;
        const STRING: i32 = 5;
        const BYTES: i32 = 6;
        const MYSQL_DURATION: i32 = 103;
        const MYSQL_TIME: i32 = 107;
        const MYSQL_DECIMAL: i32 = 102;
        const FLOAT32: i32 = 3;
        const FLOAT64: i32 = 4;
        const COLUMN_REF: i32 = 201;
        const MYSQL_ENUM: i32 = 104;
        const MYSQL_BIT: i32 = 101;
        const TIDB_VECTOR_FLOAT32: i32 = 121;
        const COUNT: i32 = 3001;
        const FIRST: i32 = 3006;
        const MAX: i32 = 3005;
        const MIN: i32 = 3004;
        const SUM: i32 = 3002;
        const AVG: i32 = 3003;
        const AGG_BIT_XOR: i32 = 3010;
        const AGG_BIT_AND: i32 = 3008;
        const AGG_BIT_OR: i32 = 3009;
        const APPROX_COUNT_DISTINCT: i32 = 3020;
        const GROUP_CONCAT: i32 = 3007;
        const ROW_NUMBER: i32 = 4001;
        const RANK: i32 = 4002;
        const DENSE_RANK: i32 = 4003;
        const CUME_DIST: i32 = 4004;
        const PERCENT_RANK: i32 = 4005;
        const NTILE: i32 = 4006;
        const LEAD: i32 = 4007;
        const LAG: i32 = 4008;
        const FIRST_VALUE: i32 = 4009;
        const LAST_VALUE: i32 = 4010;
        const NTH_VALUE: i32 = 4011;
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
