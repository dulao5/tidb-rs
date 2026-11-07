//! A1 3.3: `Tracker::checkpoint` runs the action chain only if consumption recorded with
//! `Tracker::record` grew since the last checkpoint (Go runs the actions on `Consume` with
//! `bs > 0`). Without growth an over-limit tracker that only releases memory must not
//! escalate through its fallbacks (D15.1 review, PR #11).

use std::sync::Arc;

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_memory::action::ActionOnExceed;
use tidb_memory::action::ActionRef;
use tidb_memory::action::BaseOOMAction;
use tidb_memory::tracker::Tracker;
use tidb_memory::tracker::new_tracker;

/// Logs its runs; `finish_on_act` marks it finished after acting, like a spill action whose
/// spill is done (the next trigger goes to the fallback).
struct Act {
    base: BaseOOMAction,
    name: &'static str,
    priority: i64,
    finish_on_act: bool,
    log: Arc<Mutex<Vec<&'static str>>>,
}

impl ActionOnExceed for Act {
    fn action(&self, _t: &Arc<Tracker>) -> Result<(), Error> {
        self.log.lock().push(self.name);
        if self.finish_on_act {
            self.base.set_finished();
        }
        Ok(())
    }
    fn set_fallback(&self, a: Option<ActionRef>) {
        self.base.set_fallback(a)
    }
    fn get_fallback(&self) -> Option<ActionRef> {
        self.base.get_fallback()
    }
    fn get_priority(&self) -> i64 {
        self.priority
    }
    fn set_finished(&self) {
        self.base.set_finished()
    }
    fn is_finished(&self) -> bool {
        self.base.is_finished()
    }
}

fn act(
    name: &'static str,
    priority: i64,
    finish: bool,
    log: &Arc<Mutex<Vec<&'static str>>>,
) -> ActionRef {
    Arc::new(Act {
        base: BaseOOMAction::default(),
        name,
        priority,
        finish_on_act: finish,
        log: log.clone(),
    })
}

/// "spill" (priority 2, finished after one run) with "cancel" (priority 0) as fallback.
fn setup() -> (Arc<Tracker>, Arc<Mutex<Vec<&'static str>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let t = new_tracker(1, 1000);
    t.set_action_on_exceed(Some(act("cancel", 0, false, &log)));
    t.fallback_old_and_set_new_action(Some(act("spill", 2, true, &log)));
    (t, log)
}

#[test]
fn growth_runs_the_chain_once() {
    let (t, log) = setup();
    t.record(2000);
    t.checkpoint().unwrap();
    assert_eq!(*log.lock(), ["spill"]);
    // No growth since: nothing, however often the checkpoint runs.
    for _ in 0..10 {
        t.checkpoint().unwrap();
    }
    assert_eq!(*log.lock(), ["spill"]);
}

#[test]
fn releasing_does_not_escalate() {
    let (t, log) = setup();
    t.record(2000);
    t.checkpoint().unwrap();
    // Still over the limit, but only releasing (the spill is freeing memory): Go does not
    // run the chain on Consume(-n), so "cancel" must not run yet.
    for _ in 0..5 {
        t.record(-100);
        t.checkpoint().unwrap();
    }
    assert!(t.check_exceed());
    assert_eq!(*log.lock(), ["spill"]);
    // The next growth escalates to the fallback, as in Go.
    t.record(1);
    t.checkpoint().unwrap();
    assert_eq!(*log.lock(), ["spill", "cancel"]);
}

#[test]
fn growth_below_is_seen_by_an_ancestors_checkpoint() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let root = new_tracker(1, 1000);
    root.set_action_on_exceed(Some(act("root", 1, false, &log)));
    let leaf = new_tracker(2, -1);
    leaf.attach_to(&root).unwrap();
    leaf.record(5000);
    root.checkpoint().unwrap();
    assert_eq!(*log.lock(), ["root"]);
    // The leaf's own flag is separate: its checkpoint runs once too (the topmost exceeding
    // tracker is the root, whose action has already acted).
    leaf.checkpoint().unwrap();
    root.checkpoint().unwrap();
    assert_eq!(*log.lock(), ["root", "root"]);
}

#[test]
fn transient_peak_between_checkpoints_is_not_seen() {
    // A1 deviation: allocated and freed before the next checkpoint, a peak over the limit
    // runs no action (Go's Consume would at the peak). The growth flag is set, but the
    // current consumption is under the limit at the checkpoint.
    let (t, log) = setup();
    t.record(5000);
    t.record(-4900);
    t.checkpoint().unwrap();
    assert!(log.lock().is_empty());
    assert_eq!(t.max_consumed(), 5000);
}

/// One growth runs the chain once even when the leaf and then an ancestor checkpoint (N3
/// calls the checkpoint in the Next of operators at several levels). Go: one Consume(+) calls
/// tryAction once. PR #13 review.
#[test]
fn one_growth_runs_once_across_leaf_then_ancestor() {
    let (root, log) = setup();
    let leaf = new_tracker(2, -1);
    leaf.attach_to(&root).unwrap();
    leaf.record(2000);
    leaf.checkpoint().unwrap();
    root.checkpoint().unwrap();
    assert_eq!(*log.lock(), vec!["spill"]);
}
