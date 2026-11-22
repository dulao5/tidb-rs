//! go-file: pkg/planner/core/rule/rule_prune_indexes.go
//!
//! The index pruning of the statistics loading point (`CollectPredicateColumnsPoint`):
//! `PruneIndexesByWhereAndOrder` keeps the indexes that cover the interesting columns of a
//! DataSource (block D10.2, specs/D10.2-find-best-task §3.4). A pure computation over the
//! access paths.
//!
//! Mapping: Go's `[]*util.AccessPath` in and out is a mutable slice of paths in and the
//! positions of the kept paths out (Go's pointer identity; the function writes
//! `path.IsSingleScan`). The DataSource's `IndexMergeHints` are empty: the statement has no
//! table hints (the hint handling of D13.3 keeps comment hints opaque).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tidb_expression::Column;
use tidb_model::ColumnInfo;

use crate::logicalop::DataSource;
use crate::planctx::PlanCtx;
use crate::util::AccessPath;
use crate::util::fixcontrol;

/// defaultMaxIndexes is the default maximum number of indexes to keep when pruning.
/// This prevents overly aggressive pruning when the threshold is small.
const DEFAULT_MAX_INDEXES: isize = 10;

/// indexWithScore stores an access path along with its coverage scores for ranking.
#[derive(Clone, Debug, Default)]
struct IndexWithScore {
    /// The position of the path (Go's pointer).
    path: usize,
    /// Total number of interesting columns covered
    interesting_count: isize,
    /// IDs of consecutive columns (for detecting different orderings)
    consecutive_column_ids: Vec<i64>,
}

/// columnRequirements holds the column maps needed for index pruning.
struct ColumnRequirements {
    interesting_col_ids: HashSet<i64>,
}

/// ShouldPreferIndexMerge returns true if index merge should be preferred, either due to
/// hints or fix control.
// go: ShouldPreferIndexMerge L1 [M-CTX]
pub fn should_prefer_index_merge(ds: &DataSource, ctx: &PlanCtx<'_>) -> bool {
    // len(ds.IndexMergeHints) > 0: no table hints (module doc).
    let _ = ds;
    fixcontrol::get_bool_with_default(
        ctx.vars.get_optimizer_fix_control_map(),
        fixcontrol::Fix52869,
        false,
    )
}

/// PruneIndexesByWhereAndOrder prunes indexes based on their coverage of interesting columns.
/// It keeps the most promising indexes up to the threshold, prioritizing those that:
/// 1. Cover more interesting columns
/// 2. Have consecutive column matches from the index start (enabling index prefix usage)
/// 3. Support single-scan (covering index without table lookups)
/// 4. Have different consecutive column orderings (e.g., if interesting columns are A, B, keep both (A,B) and (B,A))
///
/// The threshold controls the behavior:
/// threshold = -1: disable pruning (handled by caller)
/// threshold = 0: only prune indexes with no interesting columns (score == 0)
/// threshold > 0: keep at least threshold indexes (but at least defaultMaxIndexes)
/// but if there are fewer than threshold indexes, we will still prune zero-score indexes.
///
/// Returns the positions of the kept paths in `paths`, in Go's result order.
// go: PruneIndexesByWhereAndOrder L1 [M-CTX]
pub fn prune_indexes_by_where_and_order(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    paths: &mut [AccessPath],
    interesting_columns: &[Arc<Column>],
    threshold: isize,
) -> Vec<usize> {
    let all: Vec<usize> = (0..paths.len()).collect();
    if paths.len() <= 1 {
        return all;
    }

    let total_path_count = paths.len() as isize;

    // If we disabled the prune, return directly.
    if threshold < 0 {
        return all;
    }
    // Now the prune must happen.

    // If threshold is 0 or greater than total paths, we only prune zero-score indexes.
    let only_prune_zero_score = threshold == 0 || (threshold > total_path_count);

    // Build column ID maps and calculate totals
    let req = build_column_requirements(interesting_columns);

    let mut preferred_indexes: Vec<IndexWithScore> = Vec::with_capacity(paths.len());
    let mut table_paths: Vec<usize> = Vec::with_capacity(1);
    let mut mv_index_paths: Vec<usize> = Vec::with_capacity(1);
    let index_merge_index_paths: Vec<usize> = Vec::new();
    let prefer_merge = should_prefer_index_merge(ds, ctx);

    // Check if IndexMerge hints specify specific index names: no table hints (module doc).
    let has_specified_indexes = false;

    // Categorize each index path
    for (i, path) in paths.iter_mut().enumerate() {
        if path.is_table_path() {
            table_paths.push(i);
            continue;
        }

        // Always keep multi-value indexes (like table paths)
        if path.index.as_ref().is_some_and(|idx| idx.mv_index) {
            mv_index_paths.push(i);
            continue;
        }

        // If we have forced paths, we shouldn't prune any paths
        if path.forced {
            return all;
        }

        // Skip paths with nil Index
        if path.index.is_none() {
            continue;
        }

        // Calculate coverage for this index
        // Use TableInfo.Columns (not ds.Columns) because IndexColumn.Offset refers to TableInfo.Columns
        let table_columns: Option<&[Arc<ColumnInfo>]> = ds.table_info.columns.as_deref();
        let idx_score = score_index_path(ds, path, i, &req, table_columns);

        if !path.full_idx_cols.is_empty() {
            path.is_single_scan =
                ds.is_single_scan(ctx, &path.full_idx_cols, &path.full_idx_col_lens);
        }

        // Check if this index is specified in IndexMerge hints: has_specified_indexes is false.

        // If index merge is preferred (via general hints without index names, or fix control),
        // keep indexes that have any coverage. Note: When specific indexes are mentioned in hints,
        // those are handled above. Here we handle general IndexMerge hints (no specific index names)
        // or fix control. We still apply some filtering (len(consecutiveColumnIDs) > 0 OR other coverage)
        // to avoid keeping completely useless indexes, but we're more lenient than normal pruning.
        if prefer_merge
            && !has_specified_indexes
            && (!idx_score.consecutive_column_ids.is_empty()
                || path.is_single_scan
                || idx_score.interesting_count > 0)
        {
            // When IndexMerge is preferred without specific index names, keep any index with coverage
            preferred_indexes.push(idx_score);
            continue;
        }

        // Add to preferred indexes if it has any coverage or is a covering scan
        // We'll handle ordering diversity in buildFinalResult to ensure we keep
        // different orderings even if they have lower scores
        if path.is_single_scan || idx_score.interesting_count > 0 {
            preferred_indexes.push(idx_score);
        }
    }

    // Build final result by sorting and selecting top indexes
    let max_to_keep = threshold.max(DEFAULT_MAX_INDEXES);
    let result = build_final_result(
        paths,
        &table_paths,
        &mv_index_paths,
        &index_merge_index_paths,
        &preferred_indexes,
        max_to_keep,
        only_prune_zero_score,
        &req,
    );

    // failpoint.InjectCall("InjectCheckForIndexPrune", result): test only.

    // Safety check: if we ended up with nothing, return the original paths
    if result.is_empty() {
        return all;
    }

    // Additional safety: if we only have table paths and MVIndex paths and no regular indexes, keep original
    if result.len() == table_paths.len() + mv_index_paths.len() && preferred_indexes.is_empty() {
        return all;
    }

    result
}

/// buildColumnRequirements builds column ID maps for efficient lookup.
// go: buildColumnRequirements L1
fn build_column_requirements(interesting_columns: &[Arc<Column>]) -> ColumnRequirements {
    let mut req = ColumnRequirements {
        interesting_col_ids: HashSet::with_capacity(interesting_columns.len()),
    };

    // Build interesting column IDs
    for col in interesting_columns {
        req.interesting_col_ids.insert(col.id);
    }

    req
}

/// buildOrderingKey creates a string key representing the consecutive column ordering.
/// This is used to detect and keep indexes with different orderings.
// go: buildOrderingKey L1
fn build_ordering_key(column_ids: &[i64]) -> String {
    if column_ids.is_empty() {
        return String::new();
    }
    // Create a simple string representation of the column ID sequence
    // Using a format like "1,2,3" for columns with IDs 1, 2, 3
    let mut builder = String::with_capacity(column_ids.len() * 5);
    for (i, id) in column_ids.iter().enumerate() {
        if i > 0 {
            builder.push(',');
        }
        builder.push_str(&id.to_string());
    }
    builder
}

/// scoreIndexPath calculates coverage metrics for a single index path.
/// When FullIdxCols is nil (e.g., in static pruning mode), it uses path.Index.Columns
/// and tableColumns to determine interesting columns. Note that consecutiveColumnIDs
/// cannot be determined when FullIdxCols is nil, so it will remain empty.
// go: scoreIndexPath L1
fn score_index_path(
    ds: &DataSource,
    path: &AccessPath,
    pos: usize,
    req: &ColumnRequirements,
    table_columns: Option<&[Arc<ColumnInfo>]>,
) -> IndexWithScore {
    let mut score = IndexWithScore {
        path: pos,
        ..Default::default()
    };

    if let Some(index) = &path.index
        && !index.condition_expr_string.is_empty()
    {
        let tbl_cols = ds.table_info.columns.as_deref().unwrap_or_default();
        for col in index.affect_column.iter().flatten() {
            if !req
                .interesting_col_ids
                .contains(&tbl_cols[col.offset as usize].id)
            {
                return score;
            }
        }
    }

    if !path.full_idx_cols.is_empty() {
        // Normal path: use FullIdxCols which contains expression.Column with IDs
        for (i, idx_col) in path.full_idx_cols.iter().enumerate() {
            let Some(idx_col) = idx_col else {
                continue;
            };
            let idx_col_id = idx_col.id;

            // Check if this index column matches an interesting column
            if req.interesting_col_ids.contains(&idx_col_id) {
                score.interesting_count += 1;
                // Track consecutive columns from the start of the index
                if i == score.consecutive_column_ids.len() {
                    score.consecutive_column_ids.push(idx_col_id);
                }
            }
            // Note: We continue checking all columns to count all interesting columns,
            // even if they're not consecutive from the start. The consecutive tracking
            // will naturally stop once we hit a non-interesting column, since the condition
            // `i == len(score.consecutiveColumnIDs)` will no longer be true.
        }
    } else if let (Some(index), Some(table_columns)) = (&path.index, table_columns) {
        // Fallback path: use Index.Columns (for static pruning mode when FullIdxCols is nil)
        // Map IndexColumn.Offset to column ID via tableColumns
        for idx_col in index.columns.iter().flatten() {
            if idx_col.offset < 0 || idx_col.offset as usize >= table_columns.len() {
                continue;
            }
            let col_info = &table_columns[idx_col.offset as usize];
            let idx_col_id = col_info.id;

            // Check if this index column matches an interesting column
            if req.interesting_col_ids.contains(&idx_col_id) {
                score.interesting_count += 1;
                // Note: We cannot track consecutiveColumnIDs here because we don't have
                // the full column information needed to determine if columns are consecutive
                // in the index. This is acceptable as the user indicated.
            }
        }
    }

    score
}

/// buildFinalResult sorts and selects the top indexes to keep, combining table paths,
/// multi-value indexes, index merge indexes, and preferred indexes.
#[derive(Clone, Debug)]
struct ScoredIndex {
    info: IndexWithScore,
    score: isize,
    columns: isize,
    is_single_scan: bool,
    total_consecutive: isize,
}

/// `scoreAndSort`. Go's `slices.SortFunc` is not stable; the comparison ends with the index
/// id, so equal elements are the same path and the order is the same.
// go: scoreAndSort L1
fn score_and_sort(
    paths: &[AccessPath],
    indexes: &[IndexWithScore],
    req: &ColumnRequirements,
) -> Vec<ScoredIndex> {
    if indexes.is_empty() {
        return Vec::new();
    }
    let mut scored = Vec::with_capacity(indexes.len());
    for candidate in indexes {
        let path = &paths[candidate.path];
        let score = calculate_score_from_coverage(
            candidate,
            req.interesting_col_ids.len() as isize,
            path.is_single_scan,
        );
        // Skip indexes with score == 0 as they don't provide any value
        if score == 0 {
            continue;
        }
        let cols = path.full_idx_cols.len() as isize;
        scored.push(ScoredIndex {
            info: candidate.clone(),
            score,
            columns: cols,
            is_single_scan: path.is_single_scan,
            total_consecutive: candidate.consecutive_column_ids.len() as isize,
        });
    }
    scored.sort_by(|a, b| {
        use std::cmp::Ordering;
        // Tie-breaker: prefer indexes with higher score
        if a.score != b.score {
            return b.score.cmp(&a.score);
        }
        // Tie-breaker: prefer indexes with more consecutive columns
        if a.total_consecutive != b.total_consecutive {
            return b.total_consecutive.cmp(&a.total_consecutive);
        }
        // Tie-breaker: prefer indexes with single-scan
        if a.is_single_scan != b.is_single_scan {
            if a.is_single_scan {
                return Ordering::Less;
            }
            return Ordering::Greater;
        }
        // Tie-breaker: prefer indexes with fewer columns if
        // they have only 1 consecutive column.
        if a.total_consecutive == 1 && a.columns != b.columns {
            return a.columns.cmp(&b.columns);
        }
        // Tie-breaker: use index ID for deterministic ordering when all other criteria are equal
        // This ensures stable sorting for functionally identical indexes (e.g., k1 and k2 with same expressions)
        if let (Some(ai), Some(bi)) = (&paths[a.info.path].index, &paths[b.info.path].index) {
            // Use proper three-way comparison to avoid integer overflow
            // (a.info.path.Index.ID is int64, casting the difference to int can overflow)
            return ai.id.cmp(&bi.id);
        }
        Ordering::Equal
    });
    scored
}

// go: buildFinalResult L1
#[allow(clippy::too_many_arguments)]
fn build_final_result(
    paths: &[AccessPath],
    table_paths: &[usize],
    mv_index_paths: &[usize],
    index_merge_index_paths: &[usize],
    preferred_indexes: &[IndexWithScore],
    max_to_keep: isize,
    only_prune_zero_score: bool,
    req: &ColumnRequirements,
) -> Vec<usize> {
    let mut result: Vec<usize> = Vec::with_capacity(
        table_paths.len() + index_merge_index_paths.len() + preferred_indexes.len(),
    );

    // CRITICAL: Always include table paths - this is mandatory for correctness
    result.extend_from_slice(table_paths);
    // CRITICAL: Always include multi-value index paths - we do not have sufficient
    // information to determine if they should be pruned in this function.
    result.extend_from_slice(mv_index_paths);
    // CRITICAL: Always include indexes specified in IndexMerge hints - index merge needs them to build partial paths
    result.extend_from_slice(index_merge_index_paths);

    let mut added: HashSet<usize> = HashSet::with_capacity(
        table_paths.len() + mv_index_paths.len() + index_merge_index_paths.len(),
    );
    added.extend(table_paths);
    added.extend(mv_index_paths);
    added.extend(index_merge_index_paths);

    let preferred_scored = score_and_sort(paths, preferred_indexes, req);

    // Prune all the path with 0 score.
    if only_prune_zero_score {
        for entry in &preferred_scored {
            if !added.contains(&entry.info.path) {
                result.push(entry.info.path);
            }
        }
        return result;
    }

    // Apply two-phase selection to limit the number of indexes
    let phase1_limit = max_to_keep / 2;
    let mut selection_state = new_index_selection_state(phase1_limit, max_to_keep);

    select_indexes(
        paths,
        &preferred_scored,
        &mut added,
        req,
        &mut selection_state,
        result,
    )
}

/// indexSelectionState tracks state during two-phase index selection
struct IndexSelectionState {
    phase1_limit: isize,
    remaining: isize,
    has_non_zero_score: bool,
    phase1_count: isize,
    seen_consecutive_column_ids: HashSet<i64>,
    seen_ordering_keys: HashMap<String, ()>,
}

// go: newIndexSelectionState L1
fn new_index_selection_state(phase1_limit: isize, max_to_keep: isize) -> IndexSelectionState {
    IndexSelectionState {
        phase1_limit,
        remaining: max_to_keep,
        has_non_zero_score: false,
        phase1_count: 0,
        seen_consecutive_column_ids: HashSet::new(),
        seen_ordering_keys: HashMap::new(),
    }
}

/// selectIndexes performs two-phase selection of indexes
// go: selectIndexes L1
fn select_indexes(
    paths: &[AccessPath],
    preferred_scored: &[ScoredIndex],
    added: &mut HashSet<usize>,
    req: &ColumnRequirements,
    state: &mut IndexSelectionState,
    mut result: Vec<usize>,
) -> Vec<usize> {
    for entry in preferred_scored {
        let path = entry.info.path;
        if added.contains(&path) {
            continue;
        }
        if state.remaining == 0 {
            break;
        }
        if state.has_non_zero_score && entry.score == 0 {
            continue;
        }

        let should_add = should_add_index(paths, entry, path, req, state);
        if should_add {
            result.push(path);
            added.insert(path);
            if entry.score > 0 {
                state.has_non_zero_score = true;
            }
            state.remaining -= 1;
        }
    }
    result
}

/// shouldAddIndex determines if an index should be added based on phase and diversity rules
// go: shouldAddIndex L1
fn should_add_index(
    paths: &[AccessPath],
    entry: &ScoredIndex,
    path: usize,
    req: &ColumnRequirements,
    state: &mut IndexSelectionState,
) -> bool {
    if state.phase1_count < state.phase1_limit {
        // Phase 1: Keep top threshold/2 based solely on score
        state.phase1_count += 1;
        record_consecutive_columns(&entry.info.consecutive_column_ids, state);
        return true;
    }

    // Phase 2: Apply diversity rules
    let has_consecutive = !entry.info.consecutive_column_ids.is_empty();
    if has_consecutive {
        return should_add_index_with_consecutive(&entry.info.consecutive_column_ids, state);
    }

    should_add_index_without_consecutive(paths, entry, path, req, state)
}

/// recordConsecutiveColumns tracks consecutive column IDs and ordering keys
// go: recordConsecutiveColumns L1
fn record_consecutive_columns(consecutive_column_ids: &[i64], state: &mut IndexSelectionState) {
    for &col_id in consecutive_column_ids {
        state.seen_consecutive_column_ids.insert(col_id);
    }
    if !consecutive_column_ids.is_empty() {
        let ordering_key = build_ordering_key(consecutive_column_ids);
        state.seen_ordering_keys.insert(ordering_key, ());
    }
}

/// shouldAddIndexWithConsecutive checks if an index with consecutive columns should be added in phase 2
// go: shouldAddIndexWithConsecutive L1
fn should_add_index_with_consecutive(
    consecutive_column_ids: &[i64],
    state: &mut IndexSelectionState,
) -> bool {
    let ordering_key = build_ordering_key(consecutive_column_ids);
    if state.seen_ordering_keys.contains_key(&ordering_key) {
        return false;
    }
    state.seen_ordering_keys.insert(ordering_key, ());
    record_consecutive_columns(consecutive_column_ids, state);
    true
}

/// shouldAddIndexWithoutConsecutive checks if an index without consecutive columns should be added in phase 2
// go: shouldAddIndexWithoutConsecutive L1
fn should_add_index_without_consecutive(
    paths: &[AccessPath],
    entry: &ScoredIndex,
    path: usize,
    req: &ColumnRequirements,
    state: &IndexSelectionState,
) -> bool {
    if entry.info.interesting_count != 1 {
        return true;
    }

    // For single-column indexes, check if the column is already covered by a consecutive column
    let single_col_id = find_single_interesting_column(&paths[path], req);
    if single_col_id < 0 {
        return true;
    }

    // Don't add if the column is already in a consecutive column, unless it's a single scan
    let covered = state.seen_consecutive_column_ids.contains(&single_col_id);
    !covered || paths[path].is_single_scan
}

/// findSingleInterestingColumn finds the single interesting column ID in an index.
/// Returns -1 if FullIdxCols is nil (e.g., in static pruning mode) since we cannot
/// determine the specific column without FullIdxCols. This causes the caller to
/// keep the index, which is the safe default.
// go: findSingleInterestingColumn L1
fn find_single_interesting_column(path: &AccessPath, req: &ColumnRequirements) -> i64 {
    if path.full_idx_cols.is_empty() {
        return -1;
    }
    for idx_col in path.full_idx_cols.iter().flatten() {
        if req.interesting_col_ids.contains(&idx_col.id) {
            return idx_col.id;
        }
    }
    -1
}

/// calculateScoreFromCoverage calculates a ranking score using already-computed coverage information.
/// This avoids re-iterating through index columns.
// go: calculateScoreFromCoverage L1
fn calculate_score_from_coverage(
    info: &IndexWithScore,
    total_columns: isize,
    is_single_scan: bool,
) -> isize {
    let mut score = 0;

    // Score for interesting column coverage
    score += info.interesting_count * 10;

    // Bonus for consecutive interesting columns from start (critical for index usage)
    // Consecutive columns are much more valuable than scattered matches
    // Index on (a,b,c,d) with interesting columns a, b, c can use first 3 columns
    // But with interesting columns a, d, can only use first 1 column
    score += info.consecutive_column_ids.len() as isize * 10;

    // Bonus if the index is covering all interesting columns
    if info.interesting_count == total_columns {
        score += 10;
    }

    // Bonus for single-scan (covering index without table lookups)
    if is_single_scan {
        score += 20;
    }

    score
}
