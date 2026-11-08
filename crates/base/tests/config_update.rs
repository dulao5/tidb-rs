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
    // Each closure checks that the clone still holds this thread's own previous write: a lost
    // update anywhere during the run is caught, not only one that hits the last writes
    // (Lucky, review of #63: 6 of 6 runs fail without the lock).
    let a = std::thread::spawn(|| {
        let mut lost = 0;
        for i in 1..=N {
            config::update_global_config(|c| {
                if c.instance.max_connections != i - 1 {
                    lost += 1; // own previous write reverted
                }
                // Widen the clone → store window so that unserialized writers interleave.
                std::thread::yield_now();
                c.instance.max_connections = i;
            });
        }
        lost
    });
    let b = std::thread::spawn(|| {
        let mut lost = 0;
        for i in 1..=N {
            config::update_global(|c| {
                if i > 1 && c.split_table != ((i - 1) % 2 == 1) {
                    lost += 1; // own previous write reverted
                }
                std::thread::yield_now();
                c.split_table = i % 2 == 1;
            });
        }
        lost
    });
    assert_eq!(a.join().unwrap(), 0, "thread a lost updates");
    assert_eq!(b.join().unwrap(), 0, "thread b lost updates");
    let c = config::get_global_config();
    assert_eq!(c.instance.max_connections, N);
    assert!(
        !c.split_table,
        "the last write of thread b (N even) is false"
    );
}
