//! The instance plan cache's walks over a cached plan (the plan-cache exception A3):
//!
//! - [`seal_plan`]: sets the `sealed` bit of every node (spec §3.6), so that a cost computed
//!   on a shared node is not written into its memo;
//! - [`plan_cache_value_heap_size`]: the bytes a cache entry holds (decision 4, spec §3.5):
//!   the nodes' structs and heap buffers (`Vec` capacities, strings), the expressions, the
//!   schemas, ranges and output names. Every `Arc` is counted once (by address), and the
//!   long-lived metadata the InfoSchema owns (`TableInfo`, `IndexInfo`, `ColumnInfo`, N13)
//!   is not counted, as in Go.
//!
//! Spec §3.5 asks for a derive macro (`#[derive(HeapSize)]`); the first stage writes the walk
//! by hand for the operators that can be cached (the others never reach it,
//! `instance_cacheable` is false). The accuracy test is `plan_cache_tests`'s comparison with
//! the bytes freed when the entry is dropped.

use std::collections::HashSet;
use std::mem::size_of;
use std::sync::Arc;

use tidb_expression::expression::ExprRef;
use tidb_expression::{Column, Expr, Schema};
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_sessionctx::ranger::types::Range;
use tidb_types::datum::{Datum, EmptyDatumSize};
use tidb_types::field_name::NameSlice;

use crate::PhysicalKind;
use crate::base::PhysicalPlan;
use crate::core::physical_plans::{
    PhysicalIndexReader, PhysicalIndexScan, PhysicalLimit, PhysicalProjection, PhysicalSelection,
    PhysicalSort, PhysicalTableDual, PhysicalTableReader, PhysicalTableScan, PhysicalTopN,
};
use crate::core::plan_cache_utils::PlanCacheValue;
use crate::core::point_get_plan::PointGetPlan;
use crate::util::ByItems;

/// The reference counts in front of an `Arc`'s value.
const ARC_HEADER: i64 = 2 * size_of::<usize>() as i64;

/// Whether every child of `p` can go into the instance plan cache (Go's
/// `BasePhysicalPlan.CloneForPlanCacheWithSelf`, which clones the children).
// glue: A3 spec §3.3 (the children part of BasePhysicalPlan.CloneForPlanCacheWithSelf succeeds)
pub fn children_instance_cacheable(p: &dyn PhysicalPlan) -> bool {
    p.children().iter().all(|c| c.instance_cacheable())
}

/// The plans below `p` that are not its children: a reader's pushed-down plan(s).
// glue: N1 §3 item 9 (the reader's chain is not a child)
fn inner_plans(p: &dyn PhysicalPlan) -> Vec<&crate::PhysicalPlanRef> {
    match p.kind() {
        PhysicalKind::TableReader => p
            .as_any()
            .downcast_ref::<PhysicalTableReader>()
            .and_then(|r| r.table_plan.as_ref())
            .into_iter()
            .collect(),
        PhysicalKind::IndexReader => p
            .as_any()
            .downcast_ref::<PhysicalIndexReader>()
            .and_then(|r| r.index_plan.as_ref())
            .into_iter()
            .collect(),
        PhysicalKind::IndexLookUpReader => {
            let mut out = Vec::with_capacity(2);
            if let Some(r) = p
                .as_any()
                .downcast_ref::<crate::core::physical_plans::PhysicalIndexLookUpReader>()
            {
                if let Some(ip) = r.index_plan.as_ref() {
                    out.push(ip);
                }
                if let Some(tp) = r.table_plan.as_ref() {
                    out.push(tp);
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Seals every node of the plan (A3 spec §3.6): the children, the probe parents and the
/// reader's pushed-down plan(s).
// glue: A3 spec §3.6
pub fn seal_plan(p: &dyn PhysicalPlan) {
    p.base_physical().seal();
    for c in p.children() {
        seal_plan(c.as_ref());
    }
    for inner in inner_plans(p) {
        seal_plan(inner.as_ref());
    }
    if let Some(pg) = p.as_any().downcast_ref::<PointGetPlan>() {
        for pp in &pg.probe_parents {
            seal_plan(pp.as_ref());
        }
    }
}

/// The bytes a [`PlanCacheValue`] holds (A3 decision 4).
// glue: A3 spec §3.5
pub fn plan_cache_value_heap_size(v: &PlanCacheValue) -> i64 {
    let mut h = HeapSizer::default();
    h.bytes += size_of::<PlanCacheValue>() as i64 + ARC_HEADER;
    h.plan(&v.plan);
    h.names(&v.output_columns);
    h.bytes += (v.param_types.capacity() * size_of::<FieldType>()) as i64;
    for ft in &v.param_types {
        h.field_type_heap(ft);
    }
    h.bytes += v.stmt_hints.set_vars.capacity() as i64 * (2 * size_of::<String>() as i64 + 8)
        + v.stmt_hints
            .set_vars
            .iter()
            .map(|(k, val)| (k.capacity() + val.capacity()) as i64)
            .sum::<i64>();
    h.bytes
}

/// The walk of [`plan_cache_value_heap_size`]: the bytes so far and the `Arc`s already
/// counted.
#[derive(Default)]
struct HeapSizer {
    bytes: i64,
    seen: HashSet<usize>,
}

impl HeapSizer {
    /// Whether the `Arc` at `addr` is counted for the first time.
    // glue: A3 spec §3.5 (an Arc is counted once)
    fn first<T: ?Sized>(&mut self, a: &Arc<T>) -> bool {
        self.seen.insert(Arc::as_ptr(a) as *const () as usize)
    }

    // glue: A3 spec §3.5
    fn plan(&mut self, p: &crate::PhysicalPlanRef) {
        if !self.first(p) {
            return;
        }
        self.bytes += std::mem::size_of_val(p.as_ref()) as i64 + ARC_HEADER;
        self.node(p.as_ref());
    }

    // glue: A3 spec §3.5 (the heap of one node)
    fn node(&mut self, p: &dyn PhysicalPlan) {
        let base = p.base_physical();
        self.bytes += (base.children.capacity() * size_of::<crate::PhysicalPlanRef>()) as i64;
        for c in &base.children {
            self.plan(c);
        }
        if let Some(stats) = p.stats_info()
            && self.first(stats)
        {
            self.bytes += std::mem::size_of_val(stats.as_ref()) as i64 + ARC_HEADER;
        }
        let any = p.as_any();
        match p.kind() {
            PhysicalKind::PointGet => {
                let Some(x) = any.downcast_ref::<PointGetPlan>() else {
                    return; // a test double of the kind
                };
                self.point_get(x);
            }
            PhysicalKind::TableReader => {
                let Some(x) = any.downcast_ref::<PhysicalTableReader>() else {
                    return; // a test double of the kind
                };
                self.schema(p.schema());
                if let Some(tp) = &x.table_plan {
                    self.plan(tp);
                }
                self.bytes +=
                    (x.table_plans.capacity() * size_of::<crate::PhysicalPlanRef>()) as i64;
                for tp in &x.table_plans {
                    self.plan(tp);
                }
            }
            PhysicalKind::IndexReader => {
                let Some(x) = any.downcast_ref::<PhysicalIndexReader>() else {
                    return; // a test double of the kind
                };
                self.schema(p.schema());
                if let Some(ip) = &x.index_plan {
                    self.plan(ip);
                }
                self.bytes +=
                    (x.index_plans.capacity() * size_of::<crate::PhysicalPlanRef>()) as i64;
                for ip in &x.index_plans {
                    self.plan(ip);
                }
                self.bytes += (x.output_columns.capacity() * size_of::<usize>()) as i64;
                for c in &x.output_columns {
                    self.column(c);
                }
            }
            PhysicalKind::IndexLookUpReader => {
                let Some(x) =
                    any.downcast_ref::<crate::core::physical_plans::PhysicalIndexLookUpReader>()
                else {
                    return; // a test double of the kind
                };
                self.schema(p.schema());
                if let Some(ip) = &x.index_plan {
                    self.plan(ip);
                }
                if let Some(tp) = &x.table_plan {
                    self.plan(tp);
                }
                self.bytes +=
                    (x.index_plans.capacity() * size_of::<crate::PhysicalPlanRef>()) as i64;
                for ip in &x.index_plans {
                    self.plan(ip);
                }
                self.bytes +=
                    (x.table_plans.capacity() * size_of::<crate::PhysicalPlanRef>()) as i64;
                for tp in &x.table_plans {
                    self.plan(tp);
                }
                self.bytes +=
                    (x.index_plans_un_nature_orders.capacity() * (2 * size_of::<isize>())) as i64;
                if let Some(c) = &x.extra_handle_col {
                    self.column(c);
                }
                self.bytes += (x.common_handle_cols.capacity() * size_of::<usize>()) as i64;
                for c in &x.common_handle_cols {
                    self.column(c);
                }
            }
            PhysicalKind::LocalIndexLookUp => {
                let Some(x) =
                    any.downcast_ref::<crate::core::physical_plans::PhysicalLocalIndexLookUp>()
                else {
                    return; // a test double of the kind
                };
                self.schema(p.schema());
                self.bytes += (x.index_handle_offsets.capacity() * size_of::<u32>()) as i64;
            }
            PhysicalKind::IndexScan => {
                let Some(x) = any.downcast_ref::<PhysicalIndexScan>() else {
                    return; // a test double of the kind
                };
                self.schema(p.schema());
                self.exprs(&x.access_condition);
                self.bytes += (x.idx_cols.capacity() * size_of::<usize>()) as i64;
                for c in &x.idx_cols {
                    self.column(c);
                }
                self.bytes += (x.idx_col_lens.capacity() * size_of::<isize>()) as i64;
                self.ranges(&x.ranges);
                self.bytes += (x.columns.capacity() * size_of::<usize>()) as i64;
                self.cistr(&x.db_name);
                if let Some(n) = &x.table_as_name {
                    self.cistr(n);
                }
                if let Some(s) = &x.data_source_schema {
                    self.schema(s);
                }
                self.bytes += x.range_info.capacity() as i64;
                self.by_items(&x.by_items);
                if let Some(c) = &x.pk_is_handle_col {
                    self.column(c);
                }
                self.bytes += x.const_cols_by_cond.capacity() as i64;
                for g in &x.grouped_ranges {
                    self.ranges(g);
                }
                self.bytes += (x.group_by_col_idxs.capacity() * size_of::<isize>()) as i64;
            }
            PhysicalKind::TableScan => {
                let Some(x) = any.downcast_ref::<PhysicalTableScan>() else {
                    return; // a test double of the kind
                };
                self.schema(p.schema());
                self.exprs(&x.access_condition);
                self.exprs(&x.filter_condition);
                self.exprs(&x.late_materialization_filter_condition);
                self.bytes += (x.columns.capacity() * size_of::<usize>()) as i64;
                self.cistr(&x.db_name);
                self.ranges(&x.ranges);
                if let Some(n) = &x.table_as_name {
                    self.cistr(n);
                }
                self.bytes += x.range_info.capacity() as i64;
                self.bytes += (x.handle_idx.capacity() * size_of::<isize>()) as i64;
                self.by_items(&x.by_items);
                self.bytes += (x.tbl_cols.capacity() * size_of::<usize>()) as i64;
                for c in &x.tbl_cols {
                    self.column(c);
                }
                self.bytes += x.const_cols_by_cond.capacity() as i64;
                for g in &x.grouped_ranges {
                    self.ranges(g);
                }
            }
            PhysicalKind::Selection => {
                let Some(x) = any.downcast_ref::<PhysicalSelection>() else {
                    return; // a test double of the kind
                };
                self.exprs(&x.conditions);
            }
            PhysicalKind::Projection => {
                let Some(x) = any.downcast_ref::<PhysicalProjection>() else {
                    return; // a test double of the kind
                };
                self.schema(p.schema());
                self.exprs(&x.exprs);
            }
            PhysicalKind::Sort => {
                let Some(x) = any.downcast_ref::<PhysicalSort>() else {
                    return; // a test double of the kind
                };
                self.by_items(&x.by_items);
            }
            PhysicalKind::TopN => {
                let Some(x) = any.downcast_ref::<PhysicalTopN>() else {
                    return; // a test double of the kind
                };
                self.by_items(&x.by_items);
                self.bytes += (x.partition_by.capacity() * size_of::<usize>() * 2) as i64;
            }
            PhysicalKind::Limit => {
                let Some(x) = any.downcast_ref::<PhysicalLimit>() else {
                    return; // a test double of the kind
                };
                self.schema(p.schema());
                self.bytes += (x.partition_by.capacity() * size_of::<usize>() * 2) as i64;
            }
            PhysicalKind::TableDual => {
                let Some(x) = any.downcast_ref::<PhysicalTableDual>() else {
                    return; // a test double of the kind
                };
                self.schema(p.schema());
                self.names(&x.names);
            }
            // Not cacheable (instance_cacheable is false): never walked.
            PhysicalKind::NominalSort => {}
        }
    }

    // glue: A3 spec §3.5
    fn point_get(&mut self, x: &PointGetPlan) {
        self.bytes += (x.probe_parents.capacity() * size_of::<crate::PhysicalPlanRef>()) as i64;
        for pp in &x.probe_parents {
            self.plan(pp);
        }
        self.bytes += (x.partition_names.capacity() * size_of::<CIStr>()) as i64;
        for n in &x.partition_names {
            self.cistr(n);
        }
        self.bytes += x.db_name.capacity() as i64;
        self.schema(&x.schema);
        if let Some(c) = &x.handle_constant
            && self.first(c)
        {
            self.bytes += size_of::<tidb_expression::constant::Constant>() as i64 + ARC_HEADER;
            self.datum_heap(&c.value);
            self.field_type_heap(&c.ret_type);
        }
        if let Some(ft) = &x.handle_field_type
            && self.first(ft)
        {
            self.bytes += size_of::<FieldType>() as i64 + ARC_HEADER;
            self.field_type_heap(ft);
        }
        self.datums(&x.index_values);
        self.bytes += (x.index_constants.capacity() * size_of::<usize>()) as i64;
        for c in x.index_constants.iter().flatten() {
            if self.first(c) {
                self.bytes += size_of::<tidb_expression::constant::Constant>() as i64 + ARC_HEADER;
                self.datum_heap(&c.value);
                self.field_type_heap(&c.ret_type);
            }
        }
        self.bytes += (x.cols_field_type.capacity() * size_of::<usize>()) as i64;
        for ft in &x.cols_field_type {
            if self.first(ft) {
                self.bytes += size_of::<FieldType>() as i64 + ARC_HEADER;
                self.field_type_heap(ft);
            }
        }
        self.bytes += (x.idx_cols.capacity() * size_of::<usize>()) as i64;
        for c in &x.idx_cols {
            self.column(c);
        }
        self.bytes += (x.idx_col_lens.capacity() * size_of::<isize>()) as i64;
        self.exprs(&x.access_conditions);
        self.names(&x.output_names);
        // `columns` are the table's ColumnInfos (InfoSchema metadata): only the vector.
        self.bytes += (x.columns.capacity() * size_of::<usize>()) as i64;
        self.bytes += x.unfold_from_wild_card.capacity() as i64;
        self.bytes += (x.cols_in_where_clause.capacity() * size_of::<String>()) as i64;
        for s in &x.cols_in_where_clause {
            self.bytes += s.capacity() as i64;
        }
        self.bytes += (x.access_cols.capacity() * size_of::<usize>()) as i64;
        for c in &x.access_cols {
            self.column(c);
        }
    }

    // glue: A3 spec §3.5
    fn schema(&mut self, s: &Schema) {
        self.bytes += (s.columns.capacity() * size_of::<usize>()) as i64;
        for c in &s.columns {
            self.column(c);
        }
        for keys in [&s.keys, &s.unique_keys] {
            self.bytes += (keys.capacity() * size_of::<Vec<usize>>()) as i64;
            for k in keys.iter() {
                self.bytes += (k.capacity() * size_of::<usize>()) as i64;
                for c in k.iter() {
                    self.column(c);
                }
            }
        }
    }

    // glue: A3 spec §3.5
    fn column(&mut self, c: &Arc<Column>) {
        if !self.first(c) {
            return;
        }
        self.bytes += size_of::<Column>() as i64 + ARC_HEADER;
        self.column_heap(c);
    }

    // glue: A3 spec §3.5
    fn column_heap(&mut self, c: &Column) {
        if self.first(&c.ret_type) {
            self.bytes += size_of::<FieldType>() as i64 + ARC_HEADER;
            self.field_type_heap(&c.ret_type);
        }
        self.bytes += c.orig_name.capacity() as i64;
        if let Some(h) = c.hashcode.get() {
            self.bytes += h.capacity() as i64;
        }
        if let Some(e) = &c.virtual_expr {
            self.expr(e);
        }
    }

    // glue: A3 spec §3.5
    fn exprs(&mut self, es: &[ExprRef]) {
        self.bytes += std::mem::size_of_val(es) as i64;
        for e in es {
            self.expr(e);
        }
    }

    // glue: A3 spec §3.5
    fn by_items(&mut self, items: &[ByItems]) {
        self.bytes += std::mem::size_of_val(items) as i64;
        for i in items {
            self.expr(&i.expr);
        }
    }

    // glue: A3 spec §3.5 (an expression tree, shared nodes once)
    fn expr(&mut self, e: &ExprRef) {
        if !self.first(e) {
            return;
        }
        self.bytes += size_of::<Expr>() as i64 + ARC_HEADER;
        match &**e {
            Expr::Column(c) => self.column_heap(c),
            Expr::CorrelatedColumn(c) => self.column_heap(&c.column),
            Expr::Constant(c) => {
                self.datum_heap(&c.value);
                self.field_type_heap(&c.ret_type);
                if let Some(d) = &c.deferred_expr {
                    self.expr(d);
                }
            }
            Expr::ScalarFunction(f) => {
                self.cistr(&f.func_name);
                if let Some(h) = f.hashcode.get() {
                    self.bytes += h.capacity() as i64;
                }
                if let Some(h) = f.canonicalhashcode.get() {
                    self.bytes += h.capacity() as i64;
                }
                let func = &f.function;
                if self.first(&func.tp) {
                    self.bytes += size_of::<FieldType>() as i64 + ARC_HEADER;
                    self.field_type_heap(&func.tp);
                }
                self.exprs(&func.args);
            }
        }
    }

    // glue: A3 spec §3.5
    fn ranges(&mut self, rs: &[Range]) {
        self.bytes += std::mem::size_of_val(rs) as i64;
        for r in rs {
            self.datums(&r.low_val);
            self.datums(&r.high_val);
            self.bytes += (r.collators.capacity() * 2 * size_of::<usize>()) as i64;
        }
    }

    // glue: A3 spec §3.5
    fn datums(&mut self, ds: &Vec<Datum>) {
        self.bytes += (ds.capacity() * size_of::<Datum>()) as i64;
        for d in ds {
            self.datum_heap(d);
        }
    }

    /// The heap of a datum: its byte buffer and an owned collation name (`Datum.MemUsage`
    /// less the struct).
    // glue: A3 spec §3.5
    fn datum_heap(&mut self, d: &Datum) {
        self.bytes += (d.mem_usage() - EmptyDatumSize).max(0);
    }

    // glue: A3 spec §3.5
    fn field_type_heap(&mut self, ft: &FieldType) {
        for e in ft.get_elems().into_iter().flatten() {
            self.bytes += (size_of::<Vec<u8>>() + e.len()) as i64;
        }
    }

    // glue: A3 spec §3.5
    fn cistr(&mut self, s: &CIStr) {
        self.bytes += (s.o.capacity() + s.l.capacity()) as i64;
    }

    // glue: A3 spec §3.5
    fn names(&mut self, names: &NameSlice) {
        self.bytes += (names.capacity() * size_of::<usize>()) as i64;
        for n in names {
            if !self.first(n) {
                continue;
            }
            self.bytes += std::mem::size_of_val(n.as_ref()) as i64 + ARC_HEADER;
            for s in [
                &n.orig_tbl_name,
                &n.orig_col_name,
                &n.db_name,
                &n.tbl_name,
                &n.col_name,
            ] {
                self.cistr(s);
            }
        }
    }
}

/// A hash of what a cached plan holds that execution or a later statement could change:
/// for every node (children, the reader's chain, probe parents) its kind and id, its
/// `sealed` bit and cost memo, the point get's handle and index values, and the table
/// scan's ranges. The regression tests of the plan-cache exception A3 (spec §2.3 rows 9a
/// and 9b, §5.2) compare it before and after executing a cached plan.
// glue: A3 spec §5.2 (the structural hash of a cached plan)
pub fn plan_structure_hash(p: &dyn PhysicalPlan) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    structure_into(p, &mut h);
    // glue: A3 spec §5.2 (the walk of plan_structure_hash)
    fn structure_into(p: &dyn PhysicalPlan, h: &mut std::collections::hash_map::DefaultHasher) {
        format!("{:?}", p.kind()).hash(h);
        p.id().hash(h);
        let base = p.base_physical();
        base.is_sealed().hash(h);
        {
            let c = base.cost.lock();
            c.plan_cost_init.hash(h);
            c.plan_cost.to_bits().hash(h);
            c.plan_cost_ver2.get_cost().to_bits().hash(h);
        }
        format!("{:?}", p.schema().columns.len()).hash(h);
        if let Some(x) = p.as_any().downcast_ref::<PointGetPlan>() {
            format!("{:?}", x.handle).hash(h);
            format!("{:?}", x.index_values).hash(h);
            for pp in &x.probe_parents {
                structure_into(pp.as_ref(), h);
            }
        }
        if let Some(x) = p.as_any().downcast_ref::<PhysicalTableScan>() {
            format!("{:?}", x.ranges).hash(h);
            format!("{:?}", x.grouped_ranges).hash(h);
        }
        if let Some(x) = p.as_any().downcast_ref::<PhysicalTableReader>() {
            x.table_plans.len().hash(h);
        }
        if let Some(x) = p.as_any().downcast_ref::<PhysicalIndexScan>() {
            format!("{:?}", x.ranges).hash(h);
            format!("{:?}", x.grouped_ranges).hash(h);
        }
        if let Some(x) = p.as_any().downcast_ref::<PhysicalIndexReader>() {
            x.index_plans.len().hash(h);
        }
        if let Some(x) = p
            .as_any()
            .downcast_ref::<crate::core::physical_plans::PhysicalIndexLookUpReader>()
        {
            x.index_look_up_push_down.hash(h);
            x.paging.load(std::sync::atomic::Ordering::Relaxed).hash(h);
            x.pushed_limit.map(|l| (l.offset, l.count)).hash(h);
            x.index_plans.len().hash(h);
            x.table_plans.len().hash(h);
        }
        if let Some(x) = p
            .as_any()
            .downcast_ref::<crate::core::physical_plans::PhysicalLocalIndexLookUp>()
        {
            x.index_handle_offsets.hash(h);
        }
        for inner in inner_plans(p) {
            structure_into(inner.as_ref(), h);
        }
        for c in p.children() {
            structure_into(c.as_ref(), h);
        }
    }
    h.finish()
}
