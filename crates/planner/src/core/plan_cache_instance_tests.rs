//! Tests of the instance plan cache (the plan-cache exception A3): the port of Go's
//! `plan_cache_instance_test.go` (assertions kept; the memory is set by the test as in Go),
//! the eviction threshold, the flush generation, the path copying of a hit, the sealed cost
//! memo and the key's length prefixes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tidb_parser::types::FieldType;
use tidb_sessionctx::context::InstancePlanCache;

use crate::core::physical_plans::{PhysicalSelection, PhysicalTableReader, PhysicalTableScan};
use crate::core::plan_cache_heap_size::{plan_structure_hash, seal_plan};
use crate::core::plan_cache_instance::{
    InstancePlanCacheImpl, calc_eviction_threshold, new_instance_plan_cache,
    new_instance_plan_cache_with_shards,
};
use crate::core::plan_cache_utils::{PlanCacheKey, PlanCacheValue, check_types_compatibility4_pc};
use crate::physicalop::clone_node_calls;
use crate::{PhysicalPlan, PhysicalPlanRef, map_children};

/// No global variables (the cost tests read none).
struct NoGlobals;

impl tidb_sessionctx::variable::session::GlobalVarAccessor for NoGlobals {
    fn get_global_sys_var(&self, _: &str) -> Result<String, tidb_base::Error> {
        Ok(String::new())
    }
    fn set_global_sys_var_only(
        &mut self,
        _: &str,
        _: &str,
        _: bool,
    ) -> Result<(), tidb_base::Error> {
        Ok(())
    }
    fn get_tidb_table_value(&self, _: &str) -> Result<String, tidb_base::Error> {
        Ok(String::new())
    }
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), tidb_base::Error> {
        Ok(())
    }
}

/// A plan to put (its content does not matter to the cache).
fn plan() -> PhysicalPlanRef {
    Arc::new(PhysicalSelection::default())
}

fn key(test_key: i64, stats_hash: i64) -> PlanCacheKey {
    PlanCacheKey::for_test(&format!("{test_key}-{stats_hash}"))
}

/// Go `_put`; the value remembers its test key in its plan id.
fn put(pc: &InstancePlanCacheImpl, test_key: i64, mem: i64, stats_hash: i64) -> bool {
    let mut p = PhysicalSelection::default();
    p.set_id(test_key as i32);
    let v = PlanCacheValue::for_test(Arc::new(p), mem, Vec::new());
    pc.put(
        &key(test_key, stats_hash),
        Arc::new(v),
        &[],
        pc.flush_generation(),
    )
}

/// Go `_hit`.
fn hit(pc: &InstancePlanCacheImpl, test_key: i64, stats_hash: i64) {
    let v = pc.get(&key(test_key, stats_hash), &[]).expect("a hit");
    assert_eq!(i64::from(v.plan.id()), test_key);
}

/// Go `_miss`.
fn miss(pc: &InstancePlanCacheImpl, test_key: i64, stats_hash: i64) {
    assert!(pc.get(&key(test_key, stats_hash), &[]).is_none());
}

/// The `last_used` of a hit is written at most once a millisecond (A3 decision 7): the
/// tests that refresh entries by hitting them wait that long first.
fn after_last_used_granularity() {
    std::thread::sleep(Duration::from_millis(3));
}

// Go: TestInstancePlanCacheBasic
#[test]
fn instance_plan_cache_basic() {
    let pc = new_instance_plan_cache(1000, 1000);
    put(&pc, 1, 100, 0);
    put(&pc, 2, 100, 0);
    put(&pc, 3, 100, 0);
    assert_eq!(pc.mem_usage(), 300);
    hit(&pc, 1, 0);
    hit(&pc, 2, 0);
    hit(&pc, 3, 0);

    // exceed the hard limit during Put
    let pc = new_instance_plan_cache(250, 250);
    put(&pc, 1, 100, 0);
    put(&pc, 2, 100, 0);
    put(&pc, 3, 100, 0);
    assert_eq!(pc.mem_usage(), 200);
    hit(&pc, 1, 0);
    hit(&pc, 2, 0);
    miss(&pc, 3, 0);

    // can't Put 2 same values
    let pc = new_instance_plan_cache(250, 250);
    put(&pc, 1, 100, 0);
    put(&pc, 1, 101, 0);
    assert_eq!(pc.mem_usage(), 100); // the second one will be ignored

    // eviction
    let pc = new_instance_plan_cache(320, 500);
    put(&pc, 1, 100, 0);
    put(&pc, 2, 100, 0);
    put(&pc, 3, 100, 0);
    put(&pc, 4, 100, 0);
    put(&pc, 5, 100, 0);
    after_last_used_granularity();
    hit(&pc, 1, 0); // access 1-3 to refresh their last_used
    hit(&pc, 2, 0);
    hit(&pc, 3, 0);
    let (_, num_evicted) = pc.evict();
    assert!(num_evicted > 0);
    assert_eq!(pc.mem_usage(), 300);
    hit(&pc, 1, 0);
    hit(&pc, 2, 0);
    hit(&pc, 3, 0);
    miss(&pc, 4, 0); // 4-5 have been evicted
    miss(&pc, 5, 0);

    // no need to eviction if mem < softLimit
    let pc = new_instance_plan_cache(320, 500);
    put(&pc, 1, 100, 0);
    put(&pc, 2, 100, 0);
    put(&pc, 3, 100, 0);
    let (detail, num_evicted) = pc.evict();
    assert_eq!(num_evicted, 0);
    assert_eq!(
        detail,
        "memory usage is below the soft limit, currentTot: 300, softLimit: 320"
    );
    assert_eq!(pc.mem_usage(), 300);
    hit(&pc, 1, 0);
    hit(&pc, 2, 0);
    hit(&pc, 3, 0);

    // empty head should be dropped after eviction
    let pc = new_instance_plan_cache(1, 500);
    put(&pc, 1, 100, 0);
    put(&pc, 2, 100, 0);
    put(&pc, 3, 100, 0);
    assert_eq!(pc.mem_usage(), 300);
    assert_eq!(pc.size(), 3);
    let (_, num_evicted) = pc.evict();
    assert!(num_evicted > 0);
    assert_eq!(pc.mem_usage(), 0);
    assert_eq!(pc.size(), 0);
    assert!(pc.values().is_empty());
}

// Go: TestInstancePlanCacheWithMatchOpts
#[test]
fn instance_plan_cache_with_match_opts() {
    // same key with different statsHash
    let pc = new_instance_plan_cache(1000, 1000);
    put(&pc, 1, 100, 1);
    put(&pc, 1, 100, 2);
    put(&pc, 1, 100, 3);
    hit(&pc, 1, 1);
    hit(&pc, 1, 2);
    hit(&pc, 1, 3);
    miss(&pc, 1, 4);
    miss(&pc, 2, 1);

    // multiple keys with same statsHash
    let pc = new_instance_plan_cache(1000, 1000);
    put(&pc, 1, 100, 1);
    put(&pc, 1, 100, 2);
    put(&pc, 2, 100, 1);
    put(&pc, 2, 100, 2);
    hit(&pc, 1, 1);
    hit(&pc, 1, 2);
    miss(&pc, 1, 3);
    hit(&pc, 2, 1);
    hit(&pc, 2, 2);
    miss(&pc, 2, 3);
    miss(&pc, 3, 1);
    miss(&pc, 3, 2);
    miss(&pc, 3, 3);

    // hard limit can take effect in this case
    let pc = new_instance_plan_cache(200, 200);
    put(&pc, 1, 100, 1);
    put(&pc, 1, 100, 2);
    put(&pc, 1, 100, 3); // the third one will be ignored
    assert_eq!(pc.mem_usage(), 200);
    hit(&pc, 1, 1);
    hit(&pc, 1, 2);
    miss(&pc, 1, 3);

    // eviction this case
    let pc = new_instance_plan_cache(300, 500);
    put(&pc, 1, 100, 1);
    put(&pc, 1, 100, 2);
    put(&pc, 1, 100, 3);
    put(&pc, 1, 100, 4);
    put(&pc, 1, 100, 5);
    after_last_used_granularity();
    hit(&pc, 1, 1); // refresh 1-3's last_used
    hit(&pc, 1, 2);
    hit(&pc, 1, 3);
    let (_, num_evicted) = pc.evict();
    assert!(num_evicted > 0);
    assert_eq!(pc.mem_usage(), 300);
    hit(&pc, 1, 1);
    hit(&pc, 1, 2);
    hit(&pc, 1, 3);
    miss(&pc, 1, 4);
    miss(&pc, 1, 5);
}

/// A small deterministic generator for the concurrency tests (Go uses `math/rand`).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: u64) -> i64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) % n) as i64
    }
}

// Go: TestInstancePlanCacheConcurrentRead
#[test]
fn instance_plan_cache_concurrent_read() {
    let pc = Arc::new(new_instance_plan_cache_with_shards(300, 100000, 8));
    let mut flag = vec![[false; 100]; 100];
    let mut r = Lcg(1);
    for (k, row) in flag.iter_mut().enumerate() {
        for (stats_hash, f) in row.iter_mut().enumerate() {
            if r.next(10) < 7 {
                put(&pc, k as i64, 1, stats_hash as i64);
                *f = true;
            }
        }
    }
    let flag = Arc::new(flag);
    let threads: Vec<_> = (0..10)
        .map(|t| {
            let (pc, flag) = (Arc::clone(&pc), Arc::clone(&flag));
            std::thread::spawn(move || {
                let mut r = Lcg(100 + t);
                for _ in 0..10000 {
                    let (k, stats_hash) = (r.next(100), r.next(100));
                    if flag[k as usize][stats_hash as usize] {
                        hit(&pc, k, stats_hash);
                    } else {
                        miss(&pc, k, stats_hash);
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}

// Go: TestInstancePlanCacheConcurrentWriteRead
#[test]
fn instance_plan_cache_concurrent_write_read() {
    let pc = Arc::new(new_instance_plan_cache_with_shards(300, 100000, 8));
    let flag: Arc<Vec<Vec<AtomicBool>>> = Arc::new(
        (0..100)
            .map(|_| (0..100).map(|_| AtomicBool::new(false)).collect())
            .collect(),
    );
    let mut threads = Vec::new();
    for t in 0..5 {
        // writers
        let (pc, flag) = (Arc::clone(&pc), Arc::clone(&flag));
        threads.push(std::thread::spawn(move || {
            let mut r = Lcg(t);
            for _ in 0..1000 {
                let (k, stats_hash) = (r.next(100), r.next(100));
                if put(&pc, k, 1, stats_hash) {
                    flag[k as usize][stats_hash as usize].store(true, Ordering::SeqCst);
                }
            }
        }));
    }
    for t in 0..5 {
        // readers
        let (pc, flag) = (Arc::clone(&pc), Arc::clone(&flag));
        threads.push(std::thread::spawn(move || {
            let mut r = Lcg(50 + t);
            for _ in 0..2000 {
                let (k, stats_hash) = (r.next(100), r.next(100));
                if flag[k as usize][stats_hash as usize].load(Ordering::SeqCst) {
                    hit(&pc, k, stats_hash);
                }
            }
        }));
    }
    for t in threads {
        t.join().unwrap();
    }
    // tot_cost and tot_plan agree with the entries.
    assert_eq!(pc.size() as usize, pc.values().len());
    assert_eq!(pc.mem_usage(), pc.size());
}

/// Evictions and flushes running while other threads hit and put: every hit returns a
/// plan that was put under its key, and the totals agree with the entries at the end.
#[test]
fn instance_plan_cache_evict_and_flush_during_hits() {
    let pc = Arc::new(new_instance_plan_cache_with_shards(50, 100000, 8));
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    for t in 0..4 {
        let (pc, stop) = (Arc::clone(&pc), Arc::clone(&stop));
        threads.push(std::thread::spawn(move || {
            let mut r = Lcg(7 + t);
            while !stop.load(Ordering::SeqCst) {
                let k = r.next(200);
                put(&pc, k, 1, 0);
                if let Some(v) = pc.get(&key(k, 0), &[]) {
                    assert_eq!(i64::from(v.plan.id()), k);
                }
            }
        }));
    }
    for i in 0..50 {
        if i % 10 == 9 {
            pc.delete_all();
        } else {
            pc.evict();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    stop.store(true, Ordering::SeqCst);
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(pc.size() as usize, pc.values().len());
    assert_eq!(pc.mem_usage(), pc.size());
}

/// `calcEvictionThreshold` with Go's inputs and outputs (the times are monotonic ticks;
/// 0 is Go's zero time, which evicts nothing).
#[test]
fn calc_eviction_threshold_matches_go() {
    // no plans, or no memory per plan: zero time
    assert_eq!(calc_eviction_threshold(&mut [], 500, 300), 0);
    assert_eq!(calc_eviction_threshold(&mut [5, 6], 1, 0), 0);
    // below the soft limit: nothing to evict
    assert_eq!(calc_eviction_threshold(&mut [5, 6, 7], 300, 320), 0);
    // 5 plans of 100, soft 320: release 180 -> ceil(180/100) = 2 plans, the 2nd oldest
    assert_eq!(
        calc_eviction_threshold(&mut [50, 10, 40, 20, 30], 500, 320),
        20
    );
    // soft 300: release 200 -> 2 plans
    assert_eq!(
        calc_eviction_threshold(&mut [50, 10, 40, 20, 30], 500, 300),
        20
    );
    // soft 1: release 499 -> 5 plans, the newest
    assert_eq!(
        calc_eviction_threshold(&mut [50, 10, 40, 20, 30], 500, 1),
        50
    );
    // more to evict than there are plans (avg rounds down): Go's safety return
    assert_eq!(calc_eviction_threshold(&mut [3, 1, 2], 10, -100), 0);
}

/// A3 spec §3.4, §5.2: a statement that started optimizing before `ADMIN FLUSH INSTANCE
/// PLAN_CACHE` does not put its plan afterwards; without the flush the same steps put it.
#[test]
fn put_after_flush_is_dropped() {
    let pc = new_instance_plan_cache(1000, 1000);
    let v = || Arc::new(PlanCacheValue::for_test(plan(), 10, Vec::new()));
    // A records the generation, B flushes, A puts.
    let gen_a = pc.flush_generation();
    put(&pc, 9, 10, 0);
    assert_eq!(pc.delete_all(), 1);
    assert!(!pc.put(&key(1, 0), v(), &[], gen_a));
    assert_eq!(pc.size(), 0);
    miss(&pc, 1, 0);
    // No flush in between: the put succeeds.
    let gen_a = pc.flush_generation();
    assert!(pc.put(&key(1, 0), v(), &[], gen_a));
    assert_eq!((pc.size(), pc.mem_usage()), (1, 10));
}

/// The plans of one key are tried newest first, by parameter type compatibility (Go's list
/// order), and a put with a compatible plan already there fails.
#[test]
fn plans_of_one_key_are_matched_by_parameter_types() {
    use tidb_base::mysql::{TypeLonglong, TypeNewDecimal, TypeVarString, TypeVarchar};
    let pc = new_instance_plan_cache(1000, 1000);
    let k = key(1, 0);
    let ft = |tp: u8| FieldType::new(tp);
    let with_id = |id: i32, tps: Vec<FieldType>| {
        let mut p = PhysicalSelection::default();
        p.set_id(id);
        Arc::new(PlanCacheValue::for_test(Arc::new(p), 10, tps))
    };
    let g = pc.flush_generation();
    assert!(pc.put(
        &k,
        with_id(1, vec![ft(TypeLonglong)]),
        &[ft(TypeLonglong)],
        g
    ));
    assert!(pc.put(
        &k,
        with_id(2, vec![ft(TypeVarString)]),
        &[ft(TypeVarString)],
        g
    ));
    // VARCHAR and VAR_STRING are compatible: the existing plan blocks the put.
    assert!(!pc.put(&k, with_id(3, vec![ft(TypeVarchar)]), &[ft(TypeVarchar)], g));
    assert_eq!(pc.get(&k, &[ft(TypeLonglong)]).unwrap().plan.id(), 1);
    assert_eq!(pc.get(&k, &[ft(TypeVarchar)]).unwrap().plan.id(), 2);
    assert!(pc.get(&k, &[ft(TypeNewDecimal)]).is_none());
    assert!(pc.get(&k, &[]).is_none(), "another parameter count");

    // checkTypesCompatibility4PC: DECIMAL flen / decimal no larger than the cached ones.
    let dec = |flen: isize, d: isize| {
        let mut f = ft(TypeNewDecimal);
        f.set_flen(flen);
        f.set_decimal(d);
        f
    };
    assert!(check_types_compatibility4_pc(&[dec(10, 2)], &[dec(8, 2)]));
    assert!(!check_types_compatibility4_pc(&[dec(10, 2)], &[dec(10, 3)]));
    // the unsigned flag of an integer
    let mut u = ft(TypeLonglong);
    u.add_flag(tidb_base::mysql::UnsignedFlag);
    assert!(!check_types_compatibility4_pc(&[ft(TypeLonglong)], &[u]));
}

/// A3 decision 9: the string fields of the key have a length prefix, so a user `a127.0.0.`
/// at `%` and a user `a` at `127.0.0.%` (the same concatenation) get different keys.
#[test]
fn key_strings_have_length_prefixes() {
    let k = |user: &str, host: &str| {
        let mut k = PlanCacheKey::default();
        k.reset("select 1");
        k.push_str(user);
        k.push_str(host);
        k.finish();
        k
    };
    assert_ne!(k("a127.0.0.", "%"), k("a", "127.0.0.%"));
    assert_eq!(k("a", "127.0.0.%"), k("a", "127.0.0.%"));
}

/// A table reader over a selection over a table scan, sealed (as in the cache).
fn cached_reader() -> PhysicalPlanRef {
    let scan: PhysicalPlanRef = Arc::new(PhysicalTableScan::default());
    let mut sel = PhysicalSelection::default();
    sel.set_children(vec![scan]);
    let sel: PhysicalPlanRef = Arc::new(sel);
    let mut reader = PhysicalTableReader::default();
    reader.set_children(vec![sel]);
    let reader: PhysicalPlanRef = Arc::new(reader);
    seal_plan(reader.as_ref());
    reader
}

/// A3 spec §3.6: the cost memo of a sealed node is computed but not written, so another
/// session's cost factors never leak through the cached plan; a copy made by path copying
/// is not sealed and keeps Go's memo.
#[test]
fn sealed_nodes_do_not_write_the_cost_memo() {
    use crate::property::task_type::CopSingleReadTaskType;
    use crate::util::optimizetrace::PlanCostOption;
    let p = cached_reader();
    let before = plan_structure_hash(p.as_ref());
    let scan = {
        let r = p.as_any().downcast_ref::<PhysicalTableReader>().unwrap();
        Arc::clone(&r.table_plans[0])
    };
    let mut v = tidb_sessionctx::variable::session::SessionVars::new(Box::new(NoGlobals));
    let ctx = crate::planctx::PlanCtx::new(
        &mut v,
        tidb_sessionctx::exprstatic::new_expr_context(Vec::new()),
    );
    let opt = PlanCostOption::default();
    let c = scan
        .base_physical()
        .get_plan_cost_ver1(&ctx, CopSingleReadTaskType, &opt)
        .unwrap();
    assert_eq!(c, 0.0);
    assert!(!scan.base_physical().cost.lock().plan_cost_init);
    assert_eq!(plan_structure_hash(p.as_ref()), before);
    // A copy is not sealed: it writes the memo.
    let copy = scan.clone_node();
    assert!(!copy.base_physical().is_sealed());
    copy.base_physical()
        .get_plan_cost_ver1(&ctx, CopSingleReadTaskType, &opt)
        .unwrap();
    assert!(copy.base_physical().cost.lock().plan_cost_init);
}

/// A3 spec §3.2: changing the scan at the bottom of a cached reader copies the path to it
/// (the scan, the selection above it in the chain, and the reader), leaves the cached tree
/// unchanged, and shares the rest.
#[test]
fn path_copying_copies_only_the_rewritten_path() {
    let cached = cached_reader();
    let before = plan_structure_hash(cached.as_ref());
    let mut p = Arc::clone(&cached);
    let calls = clone_node_calls();
    // rebuildRange's TableReader arm, with a range rewrite of the scan.
    {
        let r = p.as_any().downcast_ref::<PhysicalTableReader>().unwrap();
        let mut chain = Arc::clone(r.table_plan.as_ref().unwrap());
        map_children(&mut chain, |mut scan| {
            crate::make_mut_plan(&mut scan)
                .as_any_mut()
                .downcast_mut::<PhysicalTableScan>()
                .unwrap()
                .range_info = "new".to_owned();
            scan
        });
        let r = crate::make_mut_plan(&mut p)
            .as_any_mut()
            .downcast_mut::<PhysicalTableReader>()
            .unwrap();
        r.table_plans = crate::core::initialize::flatten_list_push_down_plan(&chain);
        r.table_plan = Some(chain);
    }
    assert_eq!(clone_node_calls() - calls, 3, "scan, selection, reader");
    assert!(!Arc::ptr_eq(&p, &cached));
    assert_eq!(plan_structure_hash(cached.as_ref()), before);
    assert!(!p.base_physical().is_sealed());
}

#[test]
fn index_reader_seal_hash_heap_size_rebuild_and_cacheable() {
    use crate::core::physical_plans::{PhysicalIndexReader, PhysicalIndexScan};
    use crate::core::plan_cache_heap_size::plan_cache_value_heap_size;
    use crate::core::plan_cache_rebuild::rebuild_range;
    use crate::property::task_type::CopSingleReadTaskType;
    use crate::util::optimizetrace::PlanCostOption;
    use tidb_base::mysql::TypeLonglong;
    use tidb_expression::constant::{Constant, ParamMarker};
    use tidb_expression::scalar_function::{ast_func, new_function};
    use tidb_expression::{Column, Expr, ExprRef, Schema, new_schema};
    use tidb_model::{ColumnInfo, IndexColumn, IndexInfo, TableInfo};
    use tidb_parser::model::CIStr;
    use tidb_parser::types::UnspecifiedLength;
    use tidb_sessionctx::exprstatic as es;
    use tidb_sessionctx::ranger::Range;
    use tidb_types::datum::{Datum, new_int_datum};
    use tidb_types::field_type::new_field_type;

    fn make_ctx<'a>(
        vars: &'a mut tidb_sessionctx::variable::session::SessionVars,
        params: &[Datum],
    ) -> crate::planctx::PlanCtx<'a> {
        let ev = es::new_eval_context(vec![es::with_param_list(params)]);
        let ectx = es::new_expr_context(vec![es::with_eval_ctx(ev)]);
        crate::planctx::PlanCtx::new(vars, ectx)
    }

    let mut vars = tidb_sessionctx::variable::session::SessionVars::new(Box::new(NoGlobals));
    vars.current_db = "test".to_owned();
    let ctx = make_ctx(&mut vars, &[new_int_datum(1), new_int_datum(10)]);

    let k_col = Arc::new(Column {
        unique_id: 2,
        id: 2,
        ret_type: Arc::new(new_field_type(TypeLonglong)),
        ..Column::default()
    });
    let id_col = Arc::new(Column {
        unique_id: 1,
        id: 1,
        ret_type: Arc::new(new_field_type(TypeLonglong)),
        ..Column::default()
    });
    let mut pk_ft = new_field_type(TypeLonglong);
    pk_ft.add_flag(tidb_base::mysql::PriKeyFlag | tidb_base::mysql::NotNullFlag);
    let tbl = Arc::new(TableInfo {
        id: 10,
        name: CIStr::new("sbtest1"),
        pk_is_handle: true,
        columns: Some(vec![
            Arc::new(ColumnInfo {
                id: 1,
                name: CIStr::new("id"),
                offset: 0,
                field_type: pk_ft,
                ..Default::default()
            }),
            Arc::new(ColumnInfo {
                id: 2,
                name: CIStr::new("k"),
                offset: 1,
                field_type: new_field_type(TypeLonglong),
                ..Default::default()
            }),
        ]),
        ..Default::default()
    });
    let idx = Arc::new(IndexInfo {
        id: 1,
        name: CIStr::new("k_1"),
        columns: Some(vec![IndexColumn {
            name: CIStr::new("k"),
            offset: 1,
            length: UnspecifiedLength,
            ..Default::default()
        }]),
        ..Default::default()
    });

    // PhysicalIndexScan::instance_cacheable is false when gen_exprs is non-empty.
    let mut scan_gen = PhysicalIndexScan::default();
    assert!(scan_gen.instance_cacheable());
    scan_gen.gen_exprs.insert(
        tidb_model::TableItemID {
            table_id: 10,
            id: 2,
            is_index: false,
            is_sync_load_failed: false,
        },
        Arc::new(Expr::Column((*k_col).clone())),
    );
    assert!(!scan_gen.instance_cacheable());

    let param_expr = |order: i64, val: i64| -> ExprRef {
        let mut c = Constant::new(new_int_datum(val), new_field_type(TypeLonglong));
        c.param_marker = Some(ParamMarker { order });
        Arc::new(Expr::Constant(c))
    };
    let ge = new_function(
        &*ctx.expr_ctx,
        ast_func::GE,
        new_field_type(TypeLonglong),
        vec![Arc::new(Expr::Column((*k_col).clone())), param_expr(0, 1)],
    )
    .unwrap();
    let le = new_function(
        &*ctx.expr_ctx,
        ast_func::LE,
        new_field_type(TypeLonglong),
        vec![Arc::new(Expr::Column((*k_col).clone())), param_expr(1, 10)],
    )
    .unwrap();

    // Build a sealed IndexReader over IndexScan with ranges [1, 10].
    let r1 = Range {
        low_val: vec![new_int_datum(1)],
        high_val: vec![new_int_datum(10)],
        collators: vec![tidb_types::collate::get_binary_collator()],
        ..Default::default()
    };
    let mut is = PhysicalIndexScan {
        table: Arc::clone(&tbl),
        index: Arc::clone(&idx),
        idx_cols: vec![Arc::clone(&k_col)],
        idx_col_lens: vec![UnspecifiedLength],
        access_condition: vec![ge, le],
        columns: tbl.columns.clone().unwrap(),
        ranges: vec![r1.clone()],
        range_info: "[1,10]".to_owned(),
        ..Default::default()
    }
    .init(&ctx, 0);
    is.set_schema(new_schema(vec![Arc::clone(&k_col)]));
    let mut ir = PhysicalIndexReader {
        index_plan: Some(Arc::new(is)),
        output_columns: vec![Arc::clone(&k_col)],
        ..Default::default()
    }
    .init(&ctx, 0);
    ir.set_schema(new_schema(vec![Arc::clone(&k_col)]));
    let cached: PhysicalPlanRef = Arc::new(ir);
    seal_plan(cached.as_ref());

    // HeapSizer accounts for IndexReader and its inner IndexScan.
    let val = PlanCacheValue::for_test(Arc::clone(&cached), 0, Vec::new());
    assert!(plan_cache_value_heap_size(&val) > 0);
    let before_hash = plan_structure_hash(cached.as_ref());

    // Sealed IndexScan does not mutate cost memo during cost calculation.
    let inner_scan = {
        let r = cached
            .as_any()
            .downcast_ref::<PhysicalIndexReader>()
            .unwrap();
        Arc::clone(&r.index_plans[0])
    };
    inner_scan
        .base_physical()
        .get_plan_cost_ver1(&ctx, CopSingleReadTaskType, &PlanCostOption::default())
        .unwrap();
    assert!(!inner_scan.base_physical().cost.lock().plan_cost_init);
    assert_eq!(plan_structure_hash(cached.as_ref()), before_hash);

    // rebuild_range with identical params [1, 10] takes the zero-copy shortcut (0 node clones).
    let mut vars_same = tidb_sessionctx::variable::session::SessionVars::new(Box::new(NoGlobals));
    let mut ctx_same = make_ctx(&mut vars_same, &[new_int_datum(1), new_int_datum(10)]);
    let mut p_same = Arc::clone(&cached);
    let before_clones = clone_node_calls();
    rebuild_range(&mut ctx_same, &mut p_same).unwrap();
    assert_eq!(
        clone_node_calls() - before_clones,
        0,
        "identical ranges must take the zero-copy shortcut"
    );
    assert!(Arc::ptr_eq(&p_same, &cached));

    // rebuild_range with different params [20, 30] path-copies IndexReader + IndexScan and
    // leaves the cached tree untouched.
    let mut vars_diff = tidb_sessionctx::variable::session::SessionVars::new(Box::new(NoGlobals));
    let mut ctx_diff = make_ctx(&mut vars_diff, &[new_int_datum(20), new_int_datum(30)]);
    let mut p_diff = Arc::clone(&cached);
    rebuild_range(&mut ctx_diff, &mut p_diff).unwrap();
    assert!(!Arc::ptr_eq(&p_diff, &cached));
    assert_eq!(plan_structure_hash(cached.as_ref()), before_hash);
    let rebuilt_ir = p_diff
        .as_any()
        .downcast_ref::<PhysicalIndexReader>()
        .unwrap();
    let rebuilt_is = rebuilt_ir.index_plans[0]
        .as_any()
        .downcast_ref::<PhysicalIndexScan>()
        .unwrap();
    assert_eq!(rebuilt_is.ranges.len(), 1);
    assert_eq!(rebuilt_is.ranges[0].low_val[0].get_int64(), 20);
    assert_eq!(rebuilt_is.ranges[0].high_val[0].get_int64(), 30);

    // GroupRangesByCols rebuild on both IndexScan and TableScan when group_by_col_idxs is set.
    let eq0 = new_function(
        &*ctx.expr_ctx,
        ast_func::EQ,
        new_field_type(TypeLonglong),
        vec![Arc::new(Expr::Column((*k_col).clone())), param_expr(0, 1)],
    )
    .unwrap();
    let eq1 = new_function(
        &*ctx.expr_ctx,
        ast_func::EQ,
        new_field_type(TypeLonglong),
        vec![Arc::new(Expr::Column((*k_col).clone())), param_expr(1, 2)],
    )
    .unwrap();
    let or_cond = new_function(
        &*ctx.expr_ctx,
        ast_func::LogicOr,
        new_field_type(TypeLonglong),
        vec![eq0, eq1],
    )
    .unwrap();

    let mut is_grp = PhysicalIndexScan {
        table: Arc::clone(&tbl),
        index: Arc::clone(&idx),
        idx_cols: vec![Arc::clone(&k_col)],
        idx_col_lens: vec![UnspecifiedLength],
        access_condition: vec![Arc::clone(&or_cond)],
        columns: tbl.columns.clone().unwrap(),
        ranges: vec![r1.clone()],
        group_by_col_idxs: vec![0],
        ..Default::default()
    }
    .init(&ctx, 0);
    is_grp.set_schema(Schema::default());
    let mut ir_grp: PhysicalPlanRef = Arc::new(
        PhysicalIndexReader {
            index_plan: Some(Arc::new(is_grp)),
            ..Default::default()
        }
        .init(&ctx, 0),
    );
    rebuild_range(&mut ctx_diff, &mut ir_grp).unwrap();
    let ir_grp = ir_grp
        .as_any()
        .downcast_ref::<PhysicalIndexReader>()
        .unwrap();
    let is_grp = ir_grp.index_plans[0]
        .as_any()
        .downcast_ref::<PhysicalIndexScan>()
        .unwrap();
    assert_eq!(is_grp.grouped_ranges.len(), 2);
    assert_eq!(is_grp.grouped_ranges[0][0].low_val[0].get_int64(), 20);
    assert_eq!(is_grp.grouped_ranges[1][0].low_val[0].get_int64(), 30);

    // TableScan with pk_is_handle and group_by_col_idxs = [0]: rebuilds grouped_ranges via
    // GroupRangesByCols.
    let pk_eq0 = new_function(
        &*ctx.expr_ctx,
        ast_func::EQ,
        new_field_type(TypeLonglong),
        vec![Arc::new(Expr::Column((*id_col).clone())), param_expr(0, 1)],
    )
    .unwrap();
    let pk_eq1 = new_function(
        &*ctx.expr_ctx,
        ast_func::EQ,
        new_field_type(TypeLonglong),
        vec![Arc::new(Expr::Column((*id_col).clone())), param_expr(1, 2)],
    )
    .unwrap();
    let pk_or = new_function(
        &*ctx.expr_ctx,
        ast_func::LogicOr,
        new_field_type(TypeLonglong),
        vec![pk_eq0, pk_eq1],
    )
    .unwrap();
    let mut ts_grp = PhysicalTableScan {
        table: Arc::clone(&tbl),
        columns: tbl.columns.clone().unwrap(),
        access_condition: vec![pk_or],
        ranges: vec![r1],
        group_by_col_idxs: vec![0],
        ..Default::default()
    }
    .init(&ctx, 0);
    ts_grp.set_schema(new_schema(vec![Arc::clone(&id_col)]));
    let mut tr_grp: PhysicalPlanRef = Arc::new(
        PhysicalTableReader {
            table_plan: Some(Arc::new(ts_grp)),
            ..Default::default()
        }
        .init(&ctx, 0),
    );
    rebuild_range(&mut ctx_diff, &mut tr_grp).unwrap();
    let tr_grp = tr_grp
        .as_any()
        .downcast_ref::<PhysicalTableReader>()
        .unwrap();
    let ts_grp = tr_grp.table_plans[0]
        .as_any()
        .downcast_ref::<PhysicalTableScan>()
        .unwrap();
    assert_eq!(ts_grp.grouped_ranges.len(), 2);
    assert_eq!(ts_grp.grouped_ranges[0][0].low_val[0].get_int64(), 20);
    assert_eq!(ts_grp.grouped_ranges[1][0].low_val[0].get_int64(), 30);
}
