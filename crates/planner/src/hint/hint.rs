//! go-file: pkg/util/hint/hint.go
//!
//! The hint parsing of the planner (block D13.3, the hint skeleton of the optimizer driver):
//! `ParseStmtHints`, `ParsePlanHints` and the `PlanHints` lookups. `StmtHints` itself lives
//! in `tidb_sessionctx::hint` (the statement context embeds it).
//!
//! The Rust parser keeps `ast::TableOptimizerHint` opaque: it does not parse the hint
//! grammar (`hintparser.y`). So every place where Go reads the contents of a hint (a
//! non-empty hint list) is a boundary stub `blocked-by:D02.2` at the place Go takes the
//! branch; an empty list gives exactly Go's values.

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_parser::ast::IndexHint;
use tidb_parser::gen_types::TableOptimizerHint;
use tidb_parser::model::CIStr;

use crate::hint::StmtHints;
use crate::hint::hint_query_block::{HintWarnHandler, QBHintHandler};

/// `ast.HintTable` (the table argument of a hint): not in the Rust AST, which does not
/// parse the hint grammar (D02.2); used by the query block handler's maps.
// glue: ast.HintTable (the parser's hint grammar is not ported, D02.2)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HintTable {
    pub db_name: CIStr,
    pub table_name: CIStr,
    pub qb_name: CIStr,
    pub partition_list: Vec<CIStr>,
}

/// `ast.HintTimeRange` (the argument of `TIME_RANGE`): not in the Rust AST (D02.2).
// glue: ast.HintTimeRange (the parser's hint grammar is not ported, D02.2)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HintTimeRange {
    pub from: String,
    pub to: String,
}

/// `ast.LeadingList` (the argument of a recursive `LEADING`): not in the Rust AST (D02.2),
/// so `PlanHints.LeadingList` is always nil.
// glue: ast.LeadingList (the parser's hint grammar is not ported, D02.2)
#[derive(Debug)]
pub enum LeadingList {}

/// Go's `hypoIndexChecker func(db, tbl, col pmodel.CIStr) (colOffset int, err error)`.
pub type HypoIndexChecker<'a> = &'a dyn Fn(&CIStr, &CIStr, &CIStr) -> Result<isize, Error>;

/// Go's `setVarHintChecker func(varName, hint string) (ok bool, warning error)`.
pub type SetVarHintChecker<'a> = &'a dyn Fn(&str, &str) -> (bool, Option<Error>);

/// ParseStmtHints parses statement hints.
///
/// `set_var_hint_checker` / `hypo_index_checker` are Go's function arguments (the planner's
/// `setVarHintChecker` / `hypoIndexChecker`). The second return value is Go's `offs` (the
/// offsets of the statement hints in `hints`).
// go: ParseStmtHints L2 (a non-empty hint list is blocked-by:D02.2: TableOptimizerHint is opaque)
pub fn parse_stmt_hints(
    hints: &[&TableOptimizerHint],
    set_var_hint_checker: SetVarHintChecker<'_>,
    hypo_index_checker: HypoIndexChecker<'_>,
    current_db: &str,
    replica_read_follower: u8,
) -> Result<(StmtHints, Vec<usize>, Vec<Error>), Error> {
    let _ = (
        set_var_hint_checker,
        hypo_index_checker,
        current_db,
        replica_read_follower,
    );
    if hints.is_empty() {
        // Go's named results: the zero StmtHints (ForceNthPlan 0), nil offs and warns.
        return Ok((StmtHints::default(), Vec::new(), Vec::new()));
    }
    // for i, hint := range hints { switch hint.HintName.L { ... } }
    // glue: blocked-by:D02.2 (the parser's hint grammar: TableOptimizerHint is opaque)
    Err(blocked(
        "D02.2",
        "statement hints (ParseStmtHints: the parser's hint grammar)",
    ))
}

/// IndexJoinHints stores hint information about index nested loop join.
#[derive(Clone, Debug, Default)]
pub struct IndexJoinHints {
    pub inlj_tables: Vec<HintedTable>,
    pub inlhj_tables: Vec<HintedTable>,
    pub inlmj_tables: Vec<HintedTable>,
}

/// PlanHints are hints that are used to control the optimizer plan choices like 'use_index', 'hash_join'.
/// TODO: move ignore_plan_cache, straight_join, no_decorrelate here.
#[derive(Debug, Default)]
pub struct PlanHints {
    /// inlj_join, inlhj_join, inlmj_join
    pub index_join: IndexJoinHints,
    /// no_inlj_join, no_inlhj_join, no_inlmj_join
    pub no_index_join: IndexJoinHints,
    /// hash_join
    pub hash_join: Vec<HintedTable>,
    /// no_hash_join
    pub no_hash_join: Vec<HintedTable>,
    /// merge_join
    pub sort_merge_join: Vec<HintedTable>,
    /// no_merge_join
    pub no_merge_join: Vec<HintedTable>,
    /// bcj_join
    pub broadcast_join: Vec<HintedTable>,
    /// shuffle_join
    pub shuffle_join: Vec<HintedTable>,
    /// use_index, ignore_index
    pub index_hint_list: Vec<HintedIndex>,
    /// use_index_merge
    pub index_merge_hint_list: Vec<HintedIndex>,
    /// isolation_read_engines(xx=tiflash)
    pub tiflash_tables: Vec<HintedTable>,
    /// isolation_read_engines(xx=tikv)
    pub tikv_tables: Vec<HintedTable>,
    /// leading
    pub leading_join_order: Vec<HintedTable>,
    /// leading recursive (always nil: see [`LeadingList`])
    pub leading_list: Option<Box<LeadingList>>,
    /// hash_join_build
    pub hj_build: Vec<HintedTable>,
    /// hash_join_probe
    pub hj_probe: Vec<HintedTable>,
    /// no_index_lookup_pushdown
    pub no_index_look_up_push_down: Vec<HintedTable>,

    // Hints belows are not associated with any particular table.
    /// hash_agg, merge_agg, agg_to_cop and so on
    pub prefer_agg_type: u64,
    pub prefer_agg_to_cop: bool,
    /// limit_to_cop
    pub prefer_limit_to_cop: bool,
    /// merge
    pub cte_merge: bool,
    pub time_range_hint: HintTimeRange,
}

/// HintedTable indicates which table this hint should take effect on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HintedTable {
    /// the database name
    pub db_name: CIStr,
    /// the table name
    pub tbl_name: CIStr,
    /// partition information
    pub partitions: Vec<CIStr>,
    /// the select block offset of this hint
    pub select_offset: isize,
    /// whether this hint is applied successfully
    pub matched: bool,
}

impl HintedTable {
    /// Match checks whether the hint is matched with the given dbName and tblName.
    // go: HintedTable.Match L1
    pub fn matches(&self, other: &HintedTable) -> bool {
        self.select_offset == other.select_offset
            && self.tbl_name.l == other.tbl_name.l
            && (self.db_name.l == other.db_name.l
                || self.db_name.l == "*"
                || other.db_name.l == "*") // for cross-db bindings, e.g. *.t
    }
}

/// HintedIndex indicates which index this hint should take effect on.
#[derive(Debug, Default)]
pub struct HintedIndex {
    /// the database name
    pub db_name: CIStr,
    /// the table name
    pub tbl_name: CIStr,
    /// partition information
    pub partitions: Vec<CIStr>,
    /// the original parser index hint structure
    pub index_hint: Option<Box<IndexHint>>,
    /// whether to push down the index lookup
    pub push_down_look_up: bool,
    /// Matched indicates whether this index hint
    /// has been successfully applied to a DataSource.
    /// If an HintedIndex is not Matched after building
    /// a Select statement, we will generate a warning for it.
    pub matched: bool,
}

/// Go's `PlanHints.If*` methods take the tables as variadic `...*HintedTable`; a nil entry
/// is `None`.
impl PlanHints {
    /// IfPreferMergeJoin checks whether the join hint is merge join.
    // go: PlanHints.IfPreferMergeJoin L1
    pub fn if_prefer_merge_join(&mut self, table_names: &[Option<&HintedTable>]) -> bool {
        Self::match_table_name(table_names, &mut self.sort_merge_join)
    }

    /// IfPreferBroadcastJoin checks whether the join hint is broadcast join.
    // go: PlanHints.IfPreferBroadcastJoin L1
    pub fn if_prefer_broadcast_join(&mut self, table_names: &[Option<&HintedTable>]) -> bool {
        Self::match_table_name(table_names, &mut self.broadcast_join)
    }

    /// IfPreferShuffleJoin checks whether the join hint is shuffle join.
    // go: PlanHints.IfPreferShuffleJoin L1
    pub fn if_prefer_shuffle_join(&mut self, table_names: &[Option<&HintedTable>]) -> bool {
        Self::match_table_name(table_names, &mut self.shuffle_join)
    }

    /// IfPreferHashJoin checks whether the join hint is hash join.
    // go: PlanHints.IfPreferHashJoin L1
    pub fn if_prefer_hash_join(&mut self, table_names: &[Option<&HintedTable>]) -> bool {
        Self::match_table_name(table_names, &mut self.hash_join)
    }

    /// IfPreferNoHashJoin checks whether the join hint is no hash join.
    // go: PlanHints.IfPreferNoHashJoin L1
    pub fn if_prefer_no_hash_join(&mut self, table_names: &[Option<&HintedTable>]) -> bool {
        Self::match_table_name(table_names, &mut self.no_hash_join)
    }

    /// IfPreferNoMergeJoin checks whether the join hint is no merge join.
    // go: PlanHints.IfPreferNoMergeJoin L1
    pub fn if_prefer_no_merge_join(&mut self, table_names: &[Option<&HintedTable>]) -> bool {
        Self::match_table_name(table_names, &mut self.no_merge_join)
    }

    /// IfPreferHJBuild checks whether the join hint is hash join build side.
    // go: PlanHints.IfPreferHJBuild L1
    pub fn if_prefer_hj_build(&mut self, table_names: &[Option<&HintedTable>]) -> bool {
        Self::match_table_name(table_names, &mut self.hj_build)
    }

    /// IfPreferHJProbe checks whether the join hint is hash join probe side.
    // go: PlanHints.IfPreferHJProbe L1
    pub fn if_prefer_hj_probe(&mut self, table_names: &[Option<&HintedTable>]) -> bool {
        Self::match_table_name(table_names, &mut self.hj_probe)
    }

    /// IfPreferINLJ checks whether the join hint is index nested loop join.
    // go: PlanHints.IfPreferINLJ L1
    pub fn if_prefer_inlj(&mut self, table_names: &[Option<&HintedTable>]) -> bool {
        Self::match_table_name(table_names, &mut self.index_join.inlj_tables)
    }

    /// IfPreferINLHJ checks whether the join hint is index nested loop hash join.
    // go: PlanHints.IfPreferINLHJ L1
    pub fn if_prefer_inlhj(&mut self, table_names: &[Option<&HintedTable>]) -> bool {
        Self::match_table_name(table_names, &mut self.index_join.inlhj_tables)
    }

    /// IfPreferINLMJ checks whether the join hint is index nested loop merge join.
    // go: PlanHints.IfPreferINLMJ L1
    pub fn if_prefer_inlmj(&mut self, table_names: &[Option<&HintedTable>]) -> bool {
        Self::match_table_name(table_names, &mut self.index_join.inlmj_tables)
    }

    /// IfPreferNoIndexJoin checks whether the join hint is no index join.
    // go: PlanHints.IfPreferNoIndexJoin L1
    pub fn if_prefer_no_index_join(&mut self, table_names: &[Option<&HintedTable>]) -> bool {
        Self::match_table_name(table_names, &mut self.no_index_join.inlj_tables)
    }

    /// IfPreferNoIndexHashJoin checks whether the join hint is no index hash join.
    // go: PlanHints.IfPreferNoIndexHashJoin L1
    pub fn if_prefer_no_index_hash_join(&mut self, table_names: &[Option<&HintedTable>]) -> bool {
        Self::match_table_name(table_names, &mut self.no_index_join.inlhj_tables)
    }

    /// IfPreferNoIndexMergeJoin checks whether the join hint is no index merge join.
    // go: PlanHints.IfPreferNoIndexMergeJoin L1
    pub fn if_prefer_no_index_merge_join(&mut self, table_names: &[Option<&HintedTable>]) -> bool {
        Self::match_table_name(table_names, &mut self.no_index_join.inlmj_tables)
    }

    /// IfNoIndexLookUpPushDown checks whether to disable push down index lookup for the table.
    // go: PlanHints.IfNoIndexLookUpPushDown L1
    pub fn if_no_index_look_up_push_down(&mut self, table_names: &[Option<&HintedTable>]) -> bool {
        Self::match_table_name(table_names, &mut self.no_index_look_up_push_down)
    }

    /// IfPreferTiFlash checks whether the hint hit the need of TiFlash.
    // go: PlanHints.IfPreferTiFlash L1
    pub fn if_prefer_tiflash(&mut self, table_name: Option<&HintedTable>) -> Option<HintedTable> {
        Self::match_tikv_or_tiflash(table_name, &mut self.tiflash_tables)
    }

    /// IfPreferTiKV checks whether the hint hit the need of TiKV.
    // go: PlanHints.IfPreferTiKV L1
    pub fn if_prefer_tikv(&mut self, table_name: Option<&HintedTable>) -> Option<HintedTable> {
        Self::match_tikv_or_tiflash(table_name, &mut self.tikv_tables)
    }

    /// Go returns `&tbl`, the loop variable: a copy of the entry taken before its `Matched`
    /// is set.
    // go: PlanHints.matchTiKVOrTiFlash L1
    fn match_tikv_or_tiflash(
        table_name: Option<&HintedTable>,
        hint_tables: &mut [HintedTable],
    ) -> Option<HintedTable> {
        let table_name = table_name?;
        for tbl in hint_tables.iter_mut() {
            if tbl.matches(table_name) {
                let copy = tbl.clone();
                tbl.matched = true;
                return Some(copy);
            }
        }
        None
    }

    /// MatchTableName checks whether the hint hit the need.
    /// Only need either side matches one on the list.
    /// Even though you can put 2 tables on the list,
    /// it doesn't mean optimizer will reorder to make them
    /// join directly.
    /// Which it joins on with depend on sequence of traverse
    /// and without reorder, user might adjust themselves.
    /// This is similar to MySQL hints.
    // go: PlanHints.MatchTableName L1
    pub fn match_table_name(
        tables: &[Option<&HintedTable>],
        hint_tables: &mut [HintedTable],
    ) -> bool {
        let mut hint_matched = false;
        for table in tables {
            for cur_entry in hint_tables.iter_mut() {
                let Some(table) = table else {
                    continue;
                };
                if cur_entry.matches(table) {
                    cur_entry.matched = true;
                    hint_matched = true;
                    break;
                }
            }
        }
        hint_matched
    }
}

/// ParsePlanHints parses *ast.TableOptimizerHint to PlanHints.
///
/// Returns the plan hints and the sub-query hint flags (`HintFlagSemiJoinRewrite` /
/// `HintFlagNoDecorrelate`). The warning handler is Go's `hintWarnHandler` argument.
// go: ParsePlanHints L2 (a non-empty hint list is blocked-by:D02.2: TableOptimizerHint is opaque)
#[allow(clippy::too_many_arguments)]
pub fn parse_plan_hints(
    hints: &[&TableOptimizerHint],
    current_level: isize,
    current_db: &str,
    hint_processor: Option<&QBHintHandler>,
    straight_join_order: bool,
    handling_in_subquery: bool,
    handling_exists_subquery: bool,
    not_handling_subquery: bool,
    warn_handler: &dyn HintWarnHandler,
) -> Result<(PlanHints, u64), Error> {
    let _ = (
        current_level,
        current_db,
        hint_processor,
        handling_in_subquery,
        handling_exists_subquery,
        not_handling_subquery,
        warn_handler,
    );
    if !hints.is_empty() {
        // for _, hint := range hints { switch hint.HintName.L { ... } }
        // glue: blocked-by:D02.2 (the parser's hint grammar: TableOptimizerHint is opaque)
        return Err(blocked(
            "D02.2",
            "table hints (ParsePlanHints: the parser's hint grammar)",
        ));
    }
    // With no hints leadingHintCnt is 0: the LEADING / STRAIGHT_JOIN warning is not raised
    // whatever straightJoinOrder is.
    let _ = straight_join_order;
    Ok((PlanHints::default(), 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidb_base::blocked::blocked_by;

    fn t(db: &str, tbl: &str, off: isize) -> HintedTable {
        HintedTable {
            db_name: CIStr::new(db),
            tbl_name: CIStr::new(tbl),
            select_offset: off,
            ..Default::default()
        }
    }

    struct NoWarn;
    impl HintWarnHandler for NoWarn {
        fn set_hint_warning(&self, _: &str) {
            panic!("no warning expected");
        }
        fn set_hint_warning_from_error(&self, _: Error) {
            panic!("no warning expected");
        }
    }

    #[test]
    fn empty_hint_lists_give_go_zero_values() {
        let (sh, offs, warns) =
            parse_stmt_hints(&[], &|_, _| (true, None), &|_, _, _| Ok(0), "test", 1).unwrap();
        // Go returns the zero StmtHints: ForceNthPlan stays 0 (not -1).
        assert_eq!(sh.force_nth_plan, 0);
        assert!(!sh.has_resource_group && sh.set_vars.is_empty());
        assert!(offs.is_empty() && warns.is_empty());

        let (ph, flags) =
            parse_plan_hints(&[], 1, "test", None, true, false, false, true, &NoWarn).unwrap();
        assert_eq!(flags, 0);
        assert!(ph.hash_join.is_empty() && ph.index_hint_list.is_empty());
        assert!(ph.leading_list.is_none() && !ph.cte_merge && !ph.prefer_limit_to_cop);
    }

    #[test]
    fn non_empty_hint_lists_are_blocked() {
        let h = TableOptimizerHint::default();
        let e =
            parse_stmt_hints(&[&h], &|_, _| (true, None), &|_, _, _| Ok(0), "test", 1).unwrap_err();
        assert_eq!(blocked_by(&e), Some("D02.2"));
        let e = parse_plan_hints(&[&h], 1, "test", None, false, false, false, true, &NoWarn)
            .unwrap_err();
        assert_eq!(blocked_by(&e), Some("D02.2"));
    }

    #[test]
    fn match_table_name_marks_the_first_match() {
        let mut ph = PlanHints {
            hash_join: vec![t("test", "t1", 1), t("*", "t2", 1), t("test", "t2", 1)],
            ..Default::default()
        };
        assert!(ph.if_prefer_hash_join(&[None, Some(&t("x", "t2", 1))]));
        assert!(!ph.hash_join[0].matched && ph.hash_join[1].matched && !ph.hash_join[2].matched);
        assert!(!ph.if_prefer_hash_join(&[Some(&t("test", "t1", 2))]));
        assert!(!ph.if_prefer_merge_join(&[Some(&t("test", "t1", 1))]));
    }

    #[test]
    fn tikv_or_tiflash_returns_the_entry_before_matching() {
        let mut ph = PlanHints {
            tiflash_tables: vec![t("test", "t1", 1)],
            ..Default::default()
        };
        let got = ph.if_prefer_tiflash(Some(&t("test", "t1", 1))).unwrap();
        assert!(!got.matched);
        assert!(ph.tiflash_tables[0].matched);
        assert!(ph.if_prefer_tiflash(None).is_none());
        assert!(ph.if_prefer_tikv(Some(&t("test", "t1", 1))).is_none());
    }
}
