//! The pinned idioms of N1 (§3.4, §3.5, §3.7, §4), moved over from the D09.7 design pilot
//! (`crates/spike/planner-pilot/tests`). The pilot exercised them through real rule code
//! (column pruning, projection elimination, range rebuild); until those rules are ported
//! (VS1-23, VS1-24, VS1-39) the operators here are test stand-ins with the same shapes, and
//! the interface method is the test-only [`LogicalPlan::push_down`].

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::mysql::TypeLonglong;
use tidb_expression::{Column, Schema, new_schema};
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::field_type::new_field_type;

use crate::logicalop::{BaseLogicalPlan, LogicalSchemaProducer};
use crate::physicalop::BasePhysicalPlan;
use crate::{
    LogicalKind, LogicalPlan, PhysicalKind, PhysicalPlan, PhysicalPlanRef, PlanCtx, PlanId,
    logical_dispatch, logical_plan_boilerplate, make_mut_plan, map_children,
    physical_plan_boilerplate, with_detached,
};

// ---------------------------------------------------------------------------------------------
// Test session and columns.

struct Acc;

impl GlobalVarAccessor for Acc {
    fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

fn session() -> SessionVars {
    SessionVars::new(Box::new(Acc))
}

fn col(id: i64) -> Arc<Column> {
    Arc::new(Column {
        unique_id: id,
        ret_type: Arc::new(new_field_type(TypeLonglong)),
        orig_name: format!("test.t.c{id}"),
        ..Column::default()
    })
}

fn schema(ids: &[i64]) -> Schema {
    new_schema(ids.iter().map(|&i| col(i)).collect())
}

fn ids(s: &Schema) -> Vec<i64> {
    s.columns.iter().map(|c| c.unique_id).collect()
}

// ---------------------------------------------------------------------------------------------
// Logical stand-ins: a leaf with its own schema, a filter without one, a projection with one.

struct Source {
    base: BaseLogicalPlan,
    sp: LogicalSchemaProducer,
    pushed: Vec<String>,
}

struct Filter {
    base: BaseLogicalPlan,
    conditions: Vec<String>,
}

struct Proj {
    base: BaseLogicalPlan,
    sp: LogicalSchemaProducer,
}

fn base(ctx: &PlanCtx<'_>, tp: &'static str, children: Vec<PlanId>) -> BaseLogicalPlan {
    BaseLogicalPlan {
        id: ctx.alloc_plan_id(),
        tp,
        qb_offset: 0,
        children,
    }
}

impl LogicalPlan for Source {
    logical_plan_boilerplate!(DataSource, producer);

    /// A leaf keeps what reaches it.
    fn push_down(
        &mut self,
        _ctx: &mut PlanCtx<'_>,
        this: PlanId,
        predicates: Vec<String>,
    ) -> Result<PlanId, Error> {
        self.pushed.extend(predicates);
        Ok(this)
    }
}

impl LogicalPlan for Filter {
    logical_plan_boilerplate!(Selection);

    /// Go's shape: `retConditions, child = p.Children()[0].PredicatePushDown(...)`, then
    /// `p.Conditions = ...`; a filter left without conditions returns its child as the new
    /// root, which the caller puts in its own children.
    fn push_down(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        predicates: Vec<String>,
    ) -> Result<PlanId, Error> {
        let mut all = std::mem::take(&mut self.conditions);
        all.extend(predicates);
        let (down, keep): (Vec<_>, Vec<_>) = all.into_iter().partition(|c| c.starts_with("a"));
        let child = self.children()[0];
        self.children_mut()[0] = push_down(ctx, child, down)?;
        self.conditions = keep;
        if self.conditions.is_empty() {
            return Ok(self.children()[0]);
        }
        Ok(this)
    }
}

impl LogicalPlan for Proj {
    logical_plan_boilerplate!(Projection, producer);

    fn push_down(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        predicates: Vec<String>,
    ) -> Result<PlanId, Error> {
        let child = self.children()[0];
        self.children_mut()[0] = push_down(ctx, child, predicates)?;
        Ok(this)
    }
}

// glue: N1 (interface dispatch)
logical_dispatch!(fn push_down(predicates: Vec<String>) -> Result<PlanId, Error>);

/// `Filter(conds) -> Proj -> Source(cols)`; returns (filter, proj, source).
fn tree(ctx: &mut PlanCtx<'_>, conds: &[&str], cols: &[i64]) -> (PlanId, PlanId, PlanId) {
    let src = Source {
        base: base(ctx, "DataSource", vec![]),
        sp: LogicalSchemaProducer::new(Some(schema(cols))),
        pushed: vec![],
    };
    let src = ctx.arena.alloc(Box::new(src));
    let proj = Proj {
        base: base(ctx, "Projection", vec![src]),
        sp: LogicalSchemaProducer::new(None),
    };
    let proj = ctx.arena.alloc(Box::new(proj));
    let filter = Filter {
        base: base(ctx, "Selection", vec![proj]),
        conditions: conds.iter().map(|s| s.to_string()).collect(),
    };
    let filter = ctx.arena.alloc(Box::new(filter));
    (filter, proj, src)
}

/// Detached dispatch: the method runs with `&mut self` next to `&mut PlanCtx`, recurses
/// through the dispatch function, writes its own fields and returns `this`.
#[test]
fn detached_dispatch_keeps_go_shape() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, es::new_expr_context(Vec::new()));
    let (filter, proj, src) = tree(&mut ctx, &["a > 1", "b < 2"], &[1, 2]);
    let root = push_down(&mut ctx, filter, vec!["a < 9".to_owned()]).unwrap();
    assert_eq!(root, filter);
    let f = ctx.arena.downcast_ref::<Filter>(filter).unwrap();
    assert_eq!(f.conditions, ["b < 2"]);
    assert_eq!(f.children(), [proj]);
    let s = ctx.arena.downcast_ref::<Source>(src).unwrap();
    assert_eq!(s.pushed, ["a > 1", "a < 9"]);
    assert!(!(0..3).any(|i| ctx.arena.is_detached(PlanId(i))));
}

/// A rule returns a new subtree root and the caller replaces its child with it.
#[test]
fn returned_root_replaces_the_child() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, es::new_expr_context(Vec::new()));
    let (filter, proj, _) = tree(&mut ctx, &["a > 1"], &[1]);
    // An outer projection over the filter: the filter disappears below it.
    let outer = Proj {
        base: base(&ctx, "Projection", vec![filter]),
        sp: LogicalSchemaProducer::new(None),
    };
    let outer = ctx.arena.alloc(Box::new(outer));
    assert_eq!(push_down(&mut ctx, outer, vec![]).unwrap(), outer);
    assert_eq!(ctx.arena.get(outer).children(), [proj]);
}

/// Go's plan ids come from the session's counter in allocation order (N1 §3.6); arena ids
/// are separate.
#[test]
fn plan_ids_follow_go_allocation() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, es::new_expr_context(Vec::new()));
    let (filter, proj, src) = tree(&mut ctx, &[], &[1]);
    assert_eq!((src, proj, filter), (PlanId(0), PlanId(1), PlanId(2)));
    let got: Vec<i32> = [src, proj, filter]
        .iter()
        .map(|&p| ctx.arena.get(p).id())
        .collect();
    assert_eq!(got, [1, 2, 3]);
    assert_eq!(ctx.arena.get(filter).tp(), "Selection");
    assert_eq!(ctx.arena.get(filter).kind(), LogicalKind::Selection);
    assert!(ctx.arena.downcast_ref::<Proj>(filter).is_none());
}

/// A node without its own schema writes into its child's (Go's `Schema()` returns the
/// child's), from the arena and from the detached node alike.
#[test]
fn schema_writes_land_in_the_owner() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, es::new_expr_context(Vec::new()));
    let (filter, proj, _) = tree(&mut ctx, &[], &[1, 2, 3]);
    // The projection's schema is nil until read: then it is cloned from the source.
    ctx.arena.schema_mut(filter).columns.remove(2);
    assert_eq!(ids(ctx.arena.get(proj).schema(&ctx.arena)), [1, 2]);
    with_detached(&mut ctx, filter, |n, ctx| {
        n.schema_mut(&mut ctx.arena).columns.remove(0);
    });
    assert_eq!(ids(ctx.arena.get(filter).schema(&ctx.arena)), [2]);
    assert_eq!(ids(ctx.arena.get(proj).schema(&ctx.arena)), [2]);
}

/// `SetSchema(nil)` makes the next read initialise the schema from the child again.
#[test]
fn nil_schema_is_reinitialised_from_the_child() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, es::new_expr_context(Vec::new()));
    let (_, proj, src) = tree(&mut ctx, &[], &[1, 2]);
    assert_eq!(ids(ctx.arena.get(proj).schema(&ctx.arena)), [1, 2]);
    ctx.arena
        .get_mut(src)
        .schema_producer_mut()
        .unwrap()
        .set_schema(Some(schema(&[7])));
    assert_eq!(ids(ctx.arena.get(proj).schema(&ctx.arena)), [1, 2]);
    ctx.arena
        .get_mut(proj)
        .schema_producer_mut()
        .unwrap()
        .set_schema(None);
    assert_eq!(ids(ctx.arena.get(proj).schema(&ctx.arena)), [7]);
}

/// `split()` (the fallback idiom) hands out the arena and the session vars together.
#[test]
fn split_gives_disjoint_borrows() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, es::new_expr_context(Vec::new()));
    let (filter, _, _) = tree(&mut ctx, &["b"], &[1]);
    let (arena, _ectx, vars) = ctx.split();
    vars.last_found_rows = 5;
    arena
        .downcast_mut::<Filter>(filter)
        .unwrap()
        .conditions
        .clear();
    assert!(
        ctx.arena
            .downcast_ref::<Filter>(filter)
            .unwrap()
            .conditions
            .is_empty()
    );
    assert_eq!(ctx.vars.last_found_rows, 5);
}

/// While a node is detached, looking it up by its id panics (N1 §4).
#[test]
#[should_panic(expected = "is detached")]
fn lookup_of_a_detached_node_panics() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, es::new_expr_context(Vec::new()));
    let (filter, _, _) = tree(&mut ctx, &[], &[1]);
    with_detached(&mut ctx, filter, |_, ctx| {
        ctx.arena.get(filter);
    });
}

#[test]
#[should_panic(expected = "detached twice")]
fn detaching_twice_panics() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, es::new_expr_context(Vec::new()));
    let (filter, _, _) = tree(&mut ctx, &[], &[1]);
    let _n = ctx.arena.take(filter);
    ctx.arena.take(filter);
}

#[test]
#[should_panic(expected = "is not detached")]
fn putting_back_an_attached_node_panics() {
    let mut vars = session();
    let mut ctx = PlanCtx::new(&mut vars, es::new_expr_context(Vec::new()));
    let (filter, _, src) = tree(&mut ctx, &[], &[1]);
    let n = ctx.arena.take(src);
    ctx.arena.put(filter, n);
}

// ---------------------------------------------------------------------------------------------
// Physical stand-ins.

/// A key range `[low, high]` on an int handle (ranger is D10.4).
type Range = (i64, i64);

#[derive(Clone)]
struct Scan {
    base: BasePhysicalPlan,
    schema: Schema,
    ranges: Vec<Range>,
}

#[derive(Clone)]
struct Reader {
    base: BasePhysicalPlan,
    schema: Schema,
    table_plans: Vec<PhysicalPlanRef>,
}

#[derive(Clone)]
struct PProj {
    base: BasePhysicalPlan,
    schema: Schema,
}

/// The stand-ins own their schema (Go's `physicalSchemaProducer`, VS1-10).
macro_rules! own_schema {
    () => {
        fn schema(&self) -> &Schema {
            &self.schema
        }
        fn set_schema(&mut self, schema: Schema) {
            self.schema = schema;
        }
    };
}

impl PhysicalPlan for Scan {
    physical_plan_boilerplate!(TableScan);
    own_schema!();
}
impl PhysicalPlan for Reader {
    physical_plan_boilerplate!(TableReader);
    own_schema!();
}
impl PhysicalPlan for PProj {
    physical_plan_boilerplate!(Projection);
    own_schema!();
}

/// A `BasePhysicalPlan` with plan id `id` and `children`.
fn pbase(tp: &'static str, id: i32, children: Vec<PhysicalPlanRef>) -> BasePhysicalPlan {
    let mut b = BasePhysicalPlan::default();
    b.plan.set_tp(tp);
    b.plan.set_id(id);
    b.children = children;
    b
}

const _: () = {
    const fn send_sync<T: Send + Sync + ?Sized>() {}
    const fn send<T: Send + ?Sized>() {}
    send_sync::<dyn PhysicalPlan>();
    send::<dyn LogicalPlan>();
};

fn reader(ranges: Vec<Range>) -> PhysicalPlanRef {
    let scan = Scan {
        base: pbase("TableFullScan", 1, vec![]),
        schema: schema(&[1, 2]),
        ranges,
    };
    Arc::new(Reader {
        base: pbase("TableReader", 2, vec![]),
        schema: schema(&[1, 2]),
        table_plans: vec![Arc::new(scan)],
    })
}

fn proj(id: i32, child: PhysicalPlanRef) -> PhysicalPlanRef {
    Arc::new(PProj {
        schema: child.schema().clone_schema(),
        base: pbase("Projection", id, vec![child]),
    })
}

fn ptr(p: &PhysicalPlanRef) -> *const () {
    Arc::as_ptr(p) as *const ()
}

/// The shape of `eliminatePhysicalProjection` (postOptimize): projections that only copy
/// their child's columns are removed, bottom-up, through `map_children`.
fn eliminate_projection(p: PhysicalPlanRef) -> PhysicalPlanRef {
    let mut p = p;
    map_children(&mut p, eliminate_projection);
    if p.kind() == PhysicalKind::Projection {
        return Arc::clone(&p.children()[0]);
    }
    p
}

/// `make_mut_plan` on a tree the statement owns alone changes the node in place.
#[test]
fn make_mut_plan_in_place_when_owned() {
    let mut p = proj(3, reader(vec![]));
    let before = ptr(&p);
    make_mut_plan(&mut p).set_schema(schema(&[1]));
    assert_eq!(ptr(&p), before);
    assert_eq!(ids(p.schema()), [1]);
}

/// Shared (the plan cache holds it): the node is copied, the original is unchanged and the
/// children stay shared.
#[test]
fn make_mut_plan_copies_when_shared() {
    let cached = proj(3, reader(vec![]));
    let mut p = Arc::clone(&cached);
    make_mut_plan(&mut p).set_schema(schema(&[1]));
    assert_ne!(ptr(&p), ptr(&cached));
    assert_eq!(ids(cached.schema()), [1, 2]);
    assert!(Arc::ptr_eq(&p.children()[0], &cached.children()[0]));
}

/// `map_children` on an owned node: children are moved out and back, so `f` sees them with
/// their own reference count (1) and nothing is copied (pilot finding B5).
#[test]
fn map_children_on_an_owned_node_copies_nothing() {
    let mut p = proj(4, proj(3, reader(vec![])));
    let before = (ptr(&p), ptr(&p.children()[0]));
    map_children(&mut p, |c| {
        assert_eq!(Arc::strong_count(&c), 1);
        c
    });
    assert_eq!((ptr(&p), ptr(&p.children()[0])), before);
}

/// `map_children` on a shared node copies it only if a child changes.
#[test]
fn map_children_on_a_shared_node_copies_on_change() {
    let cached = proj(4, proj(3, reader(vec![])));
    let mut p = Arc::clone(&cached);
    map_children(&mut p, |c| c);
    assert!(Arc::ptr_eq(&p, &cached));
    map_children(&mut p, |c| Arc::clone(&c.children()[0]));
    assert!(!Arc::ptr_eq(&p, &cached));
    assert_eq!(p.children()[0].id(), 2);
    assert_eq!(cached.children()[0].id(), 3);
}

/// postOptimize on a tree the statement owns alone: the root projection's child is replaced
/// in place, no node is copied.
#[test]
fn post_optimize_in_place_on_owned_tree() {
    let r = reader(vec![]);
    let r_ptr = ptr(&r);
    let root = Arc::new(Reader {
        base: pbase("TableReader", 9, vec![proj(4, r)]),
        schema: schema(&[1, 2]),
        table_plans: vec![],
    }) as PhysicalPlanRef;
    let root_ptr = ptr(&root);
    let out = eliminate_projection(root);
    assert_eq!(ptr(&out), root_ptr);
    assert_eq!(ptr(&out.children()[0]), r_ptr);
}

/// A child shared by two candidate parents (a memoized best task in FindBestTask): while
/// the losing candidate is alive, rewriting the winner leaves the loser's subtree as it was.
#[test]
fn memo_shared_child_is_copied_only_while_shared() {
    let shared = proj(5, reader(vec![]));
    let winner = proj(6, Arc::clone(&shared));
    let loser = proj(7, Arc::clone(&shared));
    drop(shared);
    let loser_child = ptr(&loser.children()[0]);
    let out = eliminate_projection(winner);
    assert_eq!(
        out.id(),
        2,
        "both projections removed: the reader is the root"
    );
    assert_eq!(ptr(&loser.children()[0]), loser_child);
    assert_eq!(loser.children()[0].id(), 5);
}

/// The shape of `rebuildRange` (N1 §3.5): rewrite the ranges of every scan of a cached plan,
/// copying only the nodes on the path from the root.
fn rebuild_range(p: &mut PhysicalPlanRef, low: i64) {
    match p.kind() {
        PhysicalKind::TableScan => {
            let s = make_mut_plan(p)
                .as_any_mut()
                .downcast_mut::<Scan>()
                .unwrap();
            s.ranges = vec![(low, low + 10)];
        }
        PhysicalKind::TableReader => {
            let r = p.as_any().downcast_ref::<Reader>().unwrap();
            let mut tp = Arc::clone(&r.table_plans[0]);
            rebuild_range(&mut tp, low);
            let r = make_mut_plan(p)
                .as_any_mut()
                .downcast_mut::<Reader>()
                .unwrap();
            r.table_plans[0] = tp;
        }
        _ => map_children(p, |mut c| {
            rebuild_range(&mut c, low);
            c
        }),
    }
}

fn scan_ranges(p: &PhysicalPlanRef) -> Vec<Range> {
    let mut p = Arc::clone(p);
    while p.kind() != PhysicalKind::TableReader {
        p = Arc::clone(&p.children()[0]);
    }
    let r = p.as_any().downcast_ref::<Reader>().unwrap();
    r.table_plans[0]
        .as_any()
        .downcast_ref::<Scan>()
        .unwrap()
        .ranges
        .clone()
}

/// An instance plan cache entry shared by sessions: concurrent rebuilds copy only the path
/// to the scan, share everything else, and leave the cached plan unchanged.
#[test]
fn plan_cache_rebuild_by_path_copy() {
    let cached = proj(10, proj(11, reader(vec![(1, 1)])));
    std::thread::scope(|s| {
        for k in 0..4i64 {
            let cached = Arc::clone(&cached);
            s.spawn(move || {
                let mut plan = Arc::clone(&cached);
                rebuild_range(&mut plan, k);
                assert_eq!(scan_ranges(&plan), [(k, k + 10)]);
                assert!(!Arc::ptr_eq(&plan, &cached));
                assert!(!Arc::ptr_eq(&plan.children()[0], &cached.children()[0]));
                // Schemas (and expressions) inside the copied nodes are shared.
                assert!(Arc::ptr_eq(
                    &plan.schema().columns[0],
                    &cached.schema().columns[0]
                ));
            });
        }
    });
    assert_eq!(scan_ranges(&cached), [(1, 1)]);
}
