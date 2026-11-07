//! go-file: pkg/parser/ast/ddl.go
//! go-file: pkg/parser/ast/functions.go
//!
//! The pieces of the `ast` package that meta/model (D06.1) uses (block D02.3); the rest of
//! the AST is the hand-written `ast` module.

#![allow(non_upper_case_globals)]

/// TableAffinityLevelNone means no affinity.
pub const TableAffinityLevelNone: &str = "none";
/// TableAffinityLevelTable means table-level affinity.
pub const TableAffinityLevelTable: &str = "table";
/// TableAffinityLevelPartition means partition-level affinity.
pub const TableAffinityLevelPartition: &str = "partition";

/// NormalizeTableAffinityLevel normalizes the affinity level to lower case and checks if
/// it's valid.
// go: NormalizeTableAffinityLevel L1
pub fn normalize_table_affinity_level(s: &str) -> (String, bool) {
    let lower = tidb_base::gostd::to_lower(s);
    match lower.as_str() {
        TableAffinityLevelNone | TableAffinityLevelTable | TableAffinityLevelPartition => {
            (lower, true)
        }
        "" => (TableAffinityLevelNone.to_owned(), true),
        _ => (s.to_owned(), false),
    }
}

/// VecL2Distance is the name of the `VEC_L2_DISTANCE` function.
pub const VecL2Distance: &str = "vec_l2_distance";
/// VecCosineDistance is the name of the `VEC_COSINE_DISTANCE` function.
pub const VecCosineDistance: &str = "vec_cosine_distance";
