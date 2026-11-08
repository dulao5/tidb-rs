//! `config::update_global_config` serializes its clone → modify → store: two threads that
//! each change a different field of the global config N times never lose an update (Go
//! writes the fields of the shared `*Config` in place; D05.2 B1 review).

use tidb_base::config;

#[test]
fn concurrent_updates_of_different_fields_are_not_lost() {
    const N: u32 = 2000;
    config::update_global_config(|c| {
        c.instance.max_connections = 0;
        c.split_table = false;
    });
    let a = std::thread::spawn(|| {
        for i in 1..=N {
            config::update_global_config(|c| {
                // Widen the clone → store window so that unserialized writers interleave.
                std::thread::yield_now();
                c.instance.max_connections = i;
            });
        }
    });
    let b = std::thread::spawn(|| {
        for i in 1..=N {
            config::update_global(|c| {
                std::thread::yield_now();
                c.split_table = i % 2 == 1;
            });
        }
    });
    a.join().unwrap();
    b.join().unwrap();
    let c = config::get_global_config();
    assert_eq!(c.instance.max_connections, N);
    assert!(
        !c.split_table,
        "the last write of thread b (N even) is false"
    );
}
