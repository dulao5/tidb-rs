//! Unit tests of block D08.2 part 1 that govec cannot reach: `builtinFuncCache` is generic
//! (no linkname into Go), its users (LIKE, ILIKE, REGEXP, JSON_SCHEMA_VALID) come with their
//! function families.

use std::cell::Cell;

use tidb_base::Error;
use tidb_expression::builtin::BuiltinFuncCache;
use tidb_sessionctx::exprstatic as es;

#[test]
fn builtin_func_cache_per_context() {
    let c1 = es::new_eval_context(vec![]);
    let c2 = es::new_eval_context(vec![]);
    assert_ne!(
        tidb_expression::EvalContext::ctx_id(&*c1),
        tidb_expression::EvalContext::ctx_id(&*c2)
    );
    let cache: BuiltinFuncCache<i64> = BuiltinFuncCache::new();
    let built = Cell::new(0);
    let mk = |v: i64| {
        built.set(built.get() + 1);
        Ok::<i64, Error>(v)
    };

    assert_eq!(
        cache.get_cache(tidb_expression::EvalContext::ctx_id(&*c1)),
        None
    );
    assert_eq!(cache.get_or_init_cache(&*c1, || mk(1)).unwrap(), 1);
    // Same context: cached, not built again.
    assert_eq!(cache.get_or_init_cache(&*c1, || mk(9)).unwrap(), 1);
    assert_eq!(built.get(), 1);
    assert_eq!(
        cache.get_cache(tidb_expression::EvalContext::ctx_id(&*c1)),
        Some(1)
    );
    // Another context replaces the item (Go keeps one item, for the last context).
    assert_eq!(cache.get_or_init_cache(&*c2, || mk(2)).unwrap(), 2);
    assert_eq!(
        cache.get_cache(tidb_expression::EvalContext::ctx_id(&*c1)),
        None
    );
    assert_eq!(built.get(), 2);
    // An error is returned and not cached.
    let e = cache.get_or_init_cache(&*c1, || Err::<i64, Error>(Error::plain("boom")));
    assert!(e.is_err());
    assert_eq!(
        cache.get_cache(tidb_expression::EvalContext::ctx_id(&*c2)),
        Some(2)
    );
    // A signature's clone starts with an empty cache (Go's Clone copies only baseBuiltinFunc).
    let cl = cache.clone();
    assert_eq!(
        cl.get_cache(tidb_expression::EvalContext::ctx_id(&*c2)),
        None
    );
}
