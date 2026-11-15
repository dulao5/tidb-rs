//! Building ranges from conditions (Go `pkg/util/ranger`, block D10.4): the points of one
//! condition on one column, detaching the access conditions of an index or a column from
//! the filters, and turning points into `ranger.Range`s. The `Range` type, its context and
//! the full ranges are D00.1 (`tidb_sessionctx::ranger`). Spec: `specs/D10.4-ranger`.

pub mod checker;
pub mod detacher;
pub mod points;
#[allow(clippy::module_inception)]
pub mod ranger;

pub use self::detacher::*;
pub use self::points::{RangeType, null_range};
pub use self::ranger::{
    append_ranges2_point_ranges, build_column_range, build_table_range, cut_datum_by_prefix_len,
    range_single_col_to_string, ranges_to_string, reach_prefix_len, union_ranges,
};
