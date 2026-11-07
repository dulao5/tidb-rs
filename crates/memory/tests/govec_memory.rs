//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/memory.jsonl.gz`
//! (tracker `govec`, domain "memory", block D15.1) against the Rust port.
//!
//! A `script` record is a sequence of steps on trackers, actions and killers (see the
//! tracker's `govec/domains/memory.go`); every step's result and the actions it fired must
//! be equal. Go's `panic(err)` in `Consume` is an `Err` in Rust (N3 3.4); a Go panic with a
//! string (`Attach to a non-GlobalTracker`) must panic in Rust with the same message.
//! `format_bytes` / `bytes_to_string` are plain calls.
//!
//! The domain has no floating-point arithmetic that gc could fuse (`M-FMA`); the target
//! split only follows the vector layout. The script state is process-wide
//! (`MemUsageTop1Tracker`), so all records run in one test.

use std::collections::BTreeMap;
use std::io::Read;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

use parking_lot::Mutex;
use serde_json::Value;
use serde_json::json;
use tidb_base::Error;
use tidb_memory::action::ActionOnExceed;
use tidb_memory::action::ActionRef;
use tidb_memory::action::BaseOOMAction;
use tidb_memory::action::LogOnExceed;
use tidb_memory::action::PanicOnExceed;
use tidb_memory::action::new_action_with_priority;
use tidb_memory::sqlkiller::SqlKiller;
use tidb_memory::tracker;
use tidb_memory::tracker::Tracker;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

type Fired = Arc<Mutex<Vec<String>>>;

/// govec's `memMock` (tracker_test.go's mockAction with a log).
struct Mock {
    base: BaseOOMAction,
    id: i64,
    called: Mutex<bool>,
    priority: i64,
    fired: Fired,
}

impl ActionOnExceed for Mock {
    fn action(&self, t: &Arc<Tracker>) -> Result<(), Error> {
        let mut called = self.called.lock();
        if *called && let Some(fb) = self.base.get_fallback() {
            drop(called);
            return fb.action(t);
        }
        *called = true;
        self.fired
            .lock()
            .push(format!("mock:{}:{}", self.id, t.label()));
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

#[derive(Default)]
struct State {
    trackers: BTreeMap<i64, Arc<Tracker>>,
    actions: BTreeMap<i64, ActionRef>,
    mocks: BTreeMap<i64, Arc<Mock>>,
    killers: BTreeMap<i64, Arc<SqlKiller>>,
    bufs: BTreeMap<i64, i64>,
    fired: Fired,
}

fn err_json(e: &Error) -> Value {
    json!({"code": e.code(), "msg": String::from_utf8(e.error()).unwrap()})
}

/// A step's outcome: a value, or a Go panic (error or message).
enum Out {
    V(Value),
    Err(Error),
}

impl State {
    fn t(&self, a: &[i64], i: usize) -> Option<Arc<Tracker>> {
        (a[i] >= 0).then(|| self.trackers[&a[i]].clone())
    }
    fn tt(&self, a: &[i64], i: usize) -> Arc<Tracker> {
        self.t(a, i).unwrap()
    }
    fn tid(&self, t: Option<&Arc<Tracker>>) -> Value {
        match t {
            None => Value::Null,
            Some(t) => self
                .trackers
                .iter()
                .find(|(_, x)| Arc::ptr_eq(x, t))
                .map(|(id, _)| json!(id))
                .unwrap_or(json!("unknown")),
        }
    }
    fn aid(&self, a: Option<&ActionRef>) -> Value {
        match a {
            None => Value::Null,
            Some(a) => self
                .actions
                .iter()
                .find(|(_, x)| std::ptr::addr_eq(Arc::as_ptr(x), Arc::as_ptr(a)))
                .map(|(id, _)| json!(id))
                .unwrap_or(json!("unknown")),
        }
    }
    fn sorted_ids(&self, ts: &[Arc<Tracker>]) -> Value {
        let mut ids: Vec<i64> = ts
            .iter()
            .filter_map(|t| self.tid(Some(t)).as_i64())
            .collect();
        ids.sort();
        json!(ids)
    }
    fn act(&self, a: &[i64], i: usize) -> Option<ActionRef> {
        (a[i] >= 0).then(|| self.actions[&a[i]].clone())
    }

    fn step(&mut self, op: &str, a: &[i64]) -> Out {
        macro_rules! r {
            ($e:expr) => {
                match $e {
                    Ok(()) => Value::Null,
                    Err(e) => return Out::Err(e),
                }
            };
        }
        let v = match op {
            "new" => {
                self.trackers
                    .insert(a[0], tracker::new_tracker(a[1] as isize, a[2]));
                Value::Null
            }
            "new_global" => {
                self.trackers
                    .insert(a[0], tracker::new_global_tracker(a[1] as isize, a[2]));
                Value::Null
            }
            "init" => {
                let t = Arc::new(Tracker::default());
                tracker::init_tracker(&t, a[1] as isize, a[2], self.act(a, 3));
                self.trackers.insert(a[0], t);
                Value::Null
            }
            "attach" => r!(self.tt(a, 0).attach_to(&self.tt(a, 1))),
            "attach_global" => r!(self
                .tt(a, 0)
                .attach_to_global_tracker(self.t(a, 1).as_ref())),
            "detach" => r!(self.tt(a, 0).detach()),
            "detach_global" => r!(self.tt(a, 0).detach_from_global_tracker()),
            "replace" => r!(self
                .tt(a, 0)
                .replace_child(&self.tt(a, 1), self.t(a, 2).as_ref())),
            "consume" => r!(self.tt(a, 0).consume(a[1])),
            "release" => r!(self.tt(a, 0).release(a[1])),
            "buf_consume" | "buf_release" => {
                let mut b = self.bufs.get(&a[1]).copied().unwrap_or(0);
                let res = if op == "buf_consume" {
                    self.tt(a, 0).buffered_consume(&mut b, a[2])
                } else {
                    self.tt(a, 0).buffered_release(&mut b, a[2])
                };
                self.bufs.insert(a[1], b);
                if let Err(e) = res {
                    return Out::Err(e);
                }
                json!(b)
            }
            "set_limit" => {
                self.tt(a, 0).set_bytes_limit(a[1]);
                Value::Null
            }
            "set_label" => r!(self.tt(a, 0).set_label(a[1] as isize)),
            "reset" => r!(self.tt(a, 0).reset()),
            "replace_bytes" => r!(self.tt(a, 0).replace_bytes_used(a[1])),
            "reset_max" => {
                self.tt(a, 0).reset_max_consumed();
                Value::Null
            }
            "check_exceed" => json!(self.tt(a, 0).check_exceed()),
            "check_limit" => json!(self.tt(a, 0).check_bytes_limit(a[1])),
            "get_limit" => json!(self.tt(a, 0).get_bytes_limit()),
            "string" => json!(self.tt(a, 0).string()),
            "count_children" => {
                let m: BTreeMap<String, i64> = self
                    .tt(a, 0)
                    .count_all_children_mem_use()
                    .into_iter()
                    .collect();
                json!(
                    m.into_iter()
                        .map(|(k, v)| json!([k, v]))
                        .collect::<Vec<_>>()
                )
            }
            "search_label" => {
                let t = self.tt(a, 0).search_tracker_without_lock(a[1] as isize);
                self.tid(t.as_ref())
            }
            "search_more" => {
                let ts = self
                    .tt(a, 0)
                    .search_tracker_consumed_more_than_n_bytes(a[1]);
                self.sorted_ids(&ts)
            }
            "children" => self.sorted_ids(&self.tt(a, 0).get_children_for_test()),
            "less" => json!(Tracker::less_than(
                self.t(a, 0).as_deref(),
                self.t(a, 1).as_deref()
            )),
            "sess_root" => {
                let t = self.tt(a, 0);
                t.is_root_tracker_of_sess.store(true, SeqCst);
                t.killer.store(self.killers.get(&a[1]).cloned());
                Value::Null
            }
            "session_id" => {
                self.tt(a, 0).session_id.store(a[1] as u64, SeqCst);
                Value::Null
            }
            "killer" => {
                let k = SqlKiller::new();
                k.conn_id.store(a[1] as u64, SeqCst);
                self.killers.insert(a[0], k);
                Value::Null
            }
            "kill" => {
                self.killers[&a[0]].send_kill_signal(a[1] as u32);
                Value::Null
            }
            "killer_reset" => {
                self.killers[&a[0]].reset();
                Value::Null
            }
            "handle_kill" => r!(self.tt(a, 0).handle_kill_signal()),
            "killer_handle" => match self.killers[&a[0]].handle_signal() {
                Ok(()) => Value::Null,
                Err(e) => err_json(&e),
            },
            "act" => {
                let id = a[0];
                let fired = self.fired.clone();
                let action: ActionRef = match a[1] {
                    0 => {
                        let m = Arc::new(Mock {
                            base: BaseOOMAction::default(),
                            id,
                            called: Mutex::new(false),
                            priority: a[2],
                            fired,
                        });
                        self.mocks.insert(id, m.clone());
                        m
                    }
                    1 => {
                        let l = LogOnExceed::new(a[2] as u64);
                        l.set_log_hook(Some(Arc::new(move |c| {
                            fired.lock().push(format!("log:{id}:{c}"))
                        })));
                        l
                    }
                    _ => {
                        let p = PanicOnExceed::new(
                            self.killers.get(&a[2]).cloned(),
                            (100 + a[2]) as u64,
                        );
                        p.set_log_hook(Some(Arc::new(move |c| {
                            fired.lock().push(format!("panic:{id}:{c}"))
                        })));
                        p
                    }
                };
                self.actions.insert(id, action);
                Value::Null
            }
            "act_prio" => {
                let inner = self.actions[&a[1]].clone();
                self.actions
                    .insert(a[0], new_action_with_priority(inner, a[2]));
                Value::Null
            }
            "set_action" => {
                self.tt(a, 0).set_action_on_exceed(self.act(a, 1));
                Value::Null
            }
            "fallback" => {
                self.tt(a, 0)
                    .fallback_old_and_set_new_action(self.act(a, 1));
                Value::Null
            }
            "fallback_soft" => {
                self.tt(a, 0)
                    .fallback_old_and_set_new_action_for_soft_limit(self.act(a, 1));
                Value::Null
            }
            "unbind" => {
                self.tt(a, 0).unbind_actions();
                Value::Null
            }
            "unbind_hard" => {
                if let Some(x) = self.actions.get(&a[1]).cloned() {
                    self.tt(a, 0).unbind_action_from_hard_limit(&x);
                } else {
                    // Go passes a nil interface (missing map key): nothing matches.
                }
                Value::Null
            }
            "finish" => {
                self.actions[&a[0]].set_finished();
                Value::Null
            }
            "is_finished" => json!(self.actions[&a[0]].is_finished()),
            "get_fallback" => {
                let fb = self.actions[&a[0]].get_fallback();
                self.aid(fb.as_ref())
            }
            "chain" => {
                let mut ids = Vec::new();
                let mut cur = self.tt(a, 0).get_fallback_for_test(a[1] != 0);
                while let Some(c) = cur {
                    ids.push(self.aid(Some(&c)));
                    if ids.len() > 200 {
                        break;
                    }
                    cur = c.get_fallback();
                }
                if ids.is_empty() {
                    Value::Null
                } else {
                    json!(ids)
                }
            }
            "set_fallback" => {
                self.actions[&a[0]].set_fallback(self.act(a, 1));
                Value::Null
            }
            "trigger_fallback" => r!(self.mocks[&a[0]]
                .base
                .trigger_fall_back_action(&self.tt(a, 1))),
            "sess_min" => {
                tracker::ServerMemoryLimitSessMinSize.store(a[0] as u64, SeqCst);
                Value::Null
            }
            "top1" => {
                let t = tracker::MemUsageTop1Tracker.load_full();
                self.tid(t.as_ref())
            }
            "dump" => json!(
                self.trackers
                    .iter()
                    .map(|(id, t)| json!([
                        id,
                        t.label(),
                        t.bytes_consumed(),
                        t.bytes_released(),
                        t.max_consumed(),
                        t.get_bytes_limit(),
                        t.check_exceed(),
                        self.sorted_ids(&t.get_children_for_test())
                    ]))
                    .collect::<Vec<_>>()
            ),
            _ => panic!("unknown step {op}"),
        };
        Out::V(v)
    }
}

fn run_script(steps: &[Value]) -> Vec<Value> {
    tracker::MemUsageTop1Tracker.store(None);
    tracker::ServerMemoryLimitSessMinSize.store(128 << 20, SeqCst);
    tracker::EnableGCAwareMemoryTrack.store(false, SeqCst);
    let mut st = State::default();
    let mut out = Vec::new();
    for s in steps {
        let s = s.as_array().unwrap();
        let op = s[0].as_str().unwrap().to_owned();
        let args: Vec<i64> = s[1..].iter().map(|v| v.as_i64().unwrap()).collect();
        st.fired.lock().clear();
        let res = catch_unwind(AssertUnwindSafe(|| st.step(&op, &args)));
        let fired = json!(std::mem::take(&mut *st.fired.lock()));
        match res {
            Ok(Out::V(v)) => out.push(json!({"r": v, "fired": fired})),
            Ok(Out::Err(e)) => {
                out.push(json!({"r": {"panic": err_json(&e)}, "fired": fired}));
                break;
            }
            Err(p) => {
                let msg = p
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                out.push(json!({"r": {"panic": msg}, "fired": fired}));
                break;
            }
        }
    }
    tracker::MemUsageTop1Tracker.store(None);
    out
}

#[test]
fn govec_memory() {
    let path = format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/memory.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut s = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(&path).unwrap())
        .read_to_string(&mut s)
        .unwrap();
    let mut lines = s.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "memory");
    let mut n = 0;
    let mut bad = 0;
    // Panicking steps print the default hook's message; keep the output readable.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = rec["op"].as_str().unwrap();
        let got = match op {
            "script" => json!(run_script(rec["args"][0].as_array().unwrap())),
            "format_bytes" => json!(tracker::format_bytes(rec["args"][0].as_i64().unwrap())),
            "bytes_to_string" => json!(tracker::bytes_to_string(rec["args"][0].as_i64().unwrap())),
            _ => panic!("unknown op {op}"),
        };
        n += 1;
        if got != rec["out"] {
            bad += 1;
            if bad <= 5 {
                eprintln!(
                    "MISMATCH {} {}\n got  {got}\n want {}",
                    rec["src"], rec["args"], rec["out"]
                );
            }
        }
    }
    std::panic::set_hook(hook);
    assert_eq!(n, header["records"].as_i64().unwrap());
    assert_eq!(bad, 0, "{bad} of {n} records differ");
}
