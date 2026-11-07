//! go-file: pkg/util/memory/tracker.go
//!
//! The memory tracker tree (A1 3.1: the tree, labels, hard / soft limits, `MaxConsumed`, the
//! action chain and `MemUsageTop1Tracker` are L1).
//!
//! Mapping:
//! - A tracker lives in an `Arc` (Go: `*Tracker`). Children are held strongly by the parent's
//!   `children` map, as in Go; the parent link is a `Weak` (Go's GC makes the cycle harmless,
//!   `Arc` would leak it). Every ancestor of an attached tracker is reachable from the root, so
//!   the walk up in `Consume` only stops early when the root itself has been dropped.
//! - Go's `panic(err)` in `Consume` (kill signal, `PanicOnExceed`) is an `Err` (N3 3.4, L2):
//!   every function that calls `Consume` returns `Result`.
//! - `Release`'s GC finalizer has no counterpart (A1 3.2, open item 3): memory is freed at
//!   `Drop`, so the "released but not yet collected" bytes are released at once.
//! - Go `sync/atomic` operations are sequentially consistent: `SeqCst` throughout.

// Labels and process-wide variables keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::AtomicIsize;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::SeqCst;
use std::time::SystemTime;

use arc_swap::ArcSwap;
use arc_swap::ArcSwapOption;
use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::gofmt::Arg;
use tidb_base::gofmt::sprintf;
use tidb_base::gostd::conv::f64_to_i64;

use crate::action::ActionRef;
use crate::action::LogOnExceed;
use crate::action::same_action;
use crate::sqlkiller::SqlKiller;

/// The threshold when memory usage needs to be tracked (100 MB).
pub const TrackMemWhenExceeds: i64 = 104857600;

/// Default memory quota for query (1 GB).
pub const DefMemQuotaQuery: i64 = 1073741824;

/// Go `ServerMemoryLimitOriginText`.
pub static ServerMemoryLimitOriginText: LazyLock<ArcSwap<String>> =
    LazyLock::new(|| ArcSwap::from_pointee("0".to_owned()));
/// Go `ServerMemoryLimit`.
pub static ServerMemoryLimit: AtomicU64 = AtomicU64::new(0);
/// Go `ServerMemoryLimitSessMinSize`.
pub static ServerMemoryLimitSessMinSize: AtomicU64 = AtomicU64::new(128 << 20);
/// Go `QueryForceDisk`.
pub static QueryForceDisk: AtomicI64 = AtomicI64::new(0);
/// Go `TriggerMemoryLimitGC`.
pub static TriggerMemoryLimitGC: AtomicBool = AtomicBool::new(false);
/// Go `MemoryLimitGCLast` (`None` is Go's zero `time.Time`).
pub static MemoryLimitGCLast: ArcSwapOption<SystemTime> = ArcSwapOption::const_empty();
/// Go `MemoryLimitGCTotal`.
pub static MemoryLimitGCTotal: AtomicI64 = AtomicI64::new(0);

/// Go `EnableGCAwareMemoryTrack`: turns on / off the GC-aware memory track.
pub static EnableGCAwareMemoryTrack: AtomicBool = AtomicBool::new(false);

/// The scale of the soft limit to the hard limit.
const softScale: f64 = 0.8;

/// Go `bytesLimits`: limit config, published atomically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BytesLimits {
    /// `<= 0` means no limit, used for `actionMuForHardLimit`.
    bytes_hard_limit: i64,
    /// `<= 0` means no limit, used for `actionMuForSoftLimit`.
    bytes_soft_limit: i64,
}

static unlimitedBytesLimit: LazyLock<Arc<BytesLimits>> = LazyLock::new(|| {
    Arc::new(BytesLimits {
        bytes_hard_limit: -1,
        bytes_soft_limit: -1,
    })
});

static defaultQueryQuota: LazyLock<Arc<BytesLimits>> = LazyLock::new(|| {
    Arc::new(BytesLimits {
        bytes_hard_limit: DefMemQuotaQuery,
        bytes_soft_limit: DefMemQuotaQuery * 8 / 10,
    })
});

// go-part: InitTracker (`int64(float64(bytesLimit) * softScale)`, M-F2I)
fn new_limits(bytes_limit: i64) -> Arc<BytesLimits> {
    Arc::new(BytesLimits {
        bytes_hard_limit: bytes_limit,
        bytes_soft_limit: f64_to_i64(bytes_limit as f64 * softScale),
    })
}

/// Go `MemUsageTop1Tracker`: the session root tracker using the most memory, for kill.
pub static MemUsageTop1Tracker: ArcSwapOption<Tracker> = ArcSwapOption::const_empty();

type Children = HashMap<isize, Vec<Arc<Tracker>>>;

/// Go `Tracker`: tracks the memory usage during query execution. It contains an optional
/// limit and can be arranged into a tree, so that the consumption tracked by a tracker is
/// also tracked by its ancestors (Apache Impala's `MemTracker`).
///
/// Two limits: if the soft limit (0.8 × hard) is exceeded, the action that slows down memory
/// growth is triggered (`AggSpillDiskAction`); if the hard limit (`tidb_mem_query_quota`) is
/// exceeded, the action that immediately reduces memory usage (spill, rate limit, panic, log).
pub struct Tracker {
    bytes_limit: ArcSwap<BytesLimits>,
    action_mu_for_hard_limit: Mutex<Option<ActionRef>>,
    action_mu_for_soft_limit: Mutex<Option<ActionRef>>,
    /// Go `Killer`.
    pub killer: ArcSwapOption<SqlKiller>,
    /// Go `mu.children`. A global tracker (like `GlobalDiskUsageTracker`) does not keep its
    /// children, to avoid mutex contention.
    children: Mutex<Option<Children>>,
    /// Go `parMu.parent`.
    parent: Mutex<Weak<Tracker>>,
    label: AtomicIsize,
    bytes_consumed: AtomicI64,
    bytes_released: AtomicI64,
    max_consumed: AtomicI64,
    /// Go `SessionID`: the session the tracker is bound to.
    pub session_id: AtomicU64,
    /// Go `IsRootTrackerOfSess`.
    pub is_root_tracker_of_sess: AtomicBool,
    is_global: AtomicBool,
}

impl std::fmt::Debug for Tracker {
    // glue: Debug (label and consumption)
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracker")
            .field("label", &self.label())
            .field("bytes_consumed", &self.bytes_consumed())
            .finish_non_exhaustive()
    }
}

impl Default for Tracker {
    /// Go's zero `Tracker`, to be set up with [`init_tracker`]. Go's zero value has no limits
    /// (a nil pointer); here it starts unlimited.
    // glue: zero value
    fn default() -> Self {
        Tracker {
            bytes_limit: ArcSwap::new(unlimitedBytesLimit.clone()),
            action_mu_for_hard_limit: Mutex::new(None),
            action_mu_for_soft_limit: Mutex::new(None),
            killer: ArcSwapOption::empty(),
            children: Mutex::new(None),
            parent: Mutex::new(Weak::new()),
            label: AtomicIsize::new(0),
            bytes_consumed: AtomicI64::new(0),
            bytes_released: AtomicI64::new(0),
            max_consumed: AtomicI64::new(0),
            session_id: AtomicU64::new(0),
            is_root_tracker_of_sess: AtomicBool::new(false),
            is_global: AtomicBool::new(false),
        }
    }
}

/// Go `InitTracker`: (re)initializes a tracker. `bytes_limit <= 0` means no limit.
// go: InitTracker L1
pub fn init_tracker(t: &Tracker, label: isize, bytes_limit: i64, action: Option<ActionRef>) {
    *t.children.lock() = None;
    *t.action_mu_for_hard_limit.lock() = action;
    *t.action_mu_for_soft_limit.lock() = None;
    *t.parent.lock() = Weak::new();

    t.label.store(label, SeqCst);
    if bytes_limit <= 0 {
        t.bytes_limit.store(unlimitedBytesLimit.clone());
    } else if bytes_limit == DefMemQuotaQuery {
        t.bytes_limit.store(defaultQueryQuota.clone());
    } else {
        t.bytes_limit.store(new_limits(bytes_limit));
    }
    t.max_consumed.store(0, SeqCst);
    t.is_global.store(false, SeqCst);
}

/// Go `NewTracker`: a memory tracker. `bytes_limit <= 0` means no limit.
// go: NewTracker L1
pub fn new_tracker(label: isize, bytes_limit: i64) -> Arc<Tracker> {
    let t = Tracker::default();
    t.label.store(label, SeqCst);
    t.bytes_limit.store(new_limits(bytes_limit));
    *t.action_mu_for_hard_limit.lock() = Some(LogOnExceed::new(0));
    Arc::new(t)
}

/// Go `NewGlobalTracker`: a global tracker (`isGlobal`).
// go: NewGlobalTracker L1
pub fn new_global_tracker(label: isize, bytes_limit: i64) -> Arc<Tracker> {
    let t = Tracker::default();
    t.label.store(label, SeqCst);
    t.bytes_limit.store(new_limits(bytes_limit));
    *t.action_mu_for_hard_limit.lock() = Some(LogOnExceed::new(0));
    t.is_global.store(true, SeqCst);
    Arc::new(t)
}

/// Go `reArrangeFallback`: merges two action chains, by priority in descending order.
// go: reArrangeFallback L1
pub(crate) fn re_arrange_fallback(a: Option<ActionRef>, b: Option<ActionRef>) -> Option<ActionRef> {
    let (Some(mut a), Some(mut b)) = (a.clone(), b.clone()) else {
        return a.or(b);
    };
    if a.get_priority() < b.get_priority() {
        std::mem::swap(&mut a, &mut b);
    }
    a.set_fallback(re_arrange_fallback(a.get_fallback(), Some(b)));
    Some(a)
}

// go-part: Tracker.Consume (the `tryAction` closure)
fn try_action(mu: &Mutex<Option<ActionRef>>, tracker: &Arc<Tracker>) -> Result<(), Error> {
    let mut g = mu.lock();
    while let Some(a) = g.as_ref().filter(|a| a.is_finished()) {
        *g = a.get_fallback();
    }
    if let Some(a) = g.as_ref() {
        a.action(tracker)?;
    }
    Ok(())
}

/// Go `metrics.MemoryUsage.WithLabelValues(label[0], label[i]).Set(v)` for the labels of
/// `MetricsTypes`.
// go-part: Tracker.Consume (the `MetricsTypes` lookup, shared with recordRelease / release)
fn set_metrics(label: isize, inuse: bool, v: i64) {
    if let Some(l) = MetricsTypes(label) {
        tidb_metrics::memory_usage_set(l[0], if inuse { l[1] } else { l[2] }, v as f64);
    }
}

impl Tracker {
    /// Whether the hard limit equals `val`. Only used in tests.
    // go: Tracker.CheckBytesLimit L1
    pub fn check_bytes_limit(&self, val: i64) -> bool {
        self.bytes_limit.load().bytes_hard_limit == val
    }

    /// Sets the hard limit (`<= 0` means no limit); the soft limit is 0.8 of it.
    // go: Tracker.SetBytesLimit L1
    pub fn set_bytes_limit(&self, bytes_limit: i64) {
        if bytes_limit <= 0 {
            self.bytes_limit.store(unlimitedBytesLimit.clone());
        } else if bytes_limit == DefMemQuotaQuery {
            self.bytes_limit.store(defaultQueryQuota.clone());
        } else {
            self.bytes_limit.store(new_limits(bytes_limit));
        }
    }

    /// The hard limit (`<= 0` means no limit).
    // go: Tracker.GetBytesLimit L1
    pub fn get_bytes_limit(&self) -> i64 {
        self.bytes_limit.load().bytes_hard_limit
    }

    /// Whether the consumed bytes exceed the hard limit.
    // go: Tracker.CheckExceed L1
    pub fn check_exceed(&self) -> bool {
        let bytes_hard_limit = self.bytes_limit.load().bytes_hard_limit;
        self.bytes_consumed.load(SeqCst) >= bytes_hard_limit && bytes_hard_limit > 0
    }

    /// Sets the action when memory usage exceeds the hard limit.
    // go: Tracker.SetActionOnExceed L1
    pub fn set_action_on_exceed(&self, a: Option<ActionRef>) {
        *self.action_mu_for_hard_limit.lock() = a;
    }

    /// Sets the hard-limit action, with the original action as its fallback (by priority).
    // go: Tracker.FallbackOldAndSetNewAction L1
    pub fn fallback_old_and_set_new_action(&self, a: Option<ActionRef>) {
        let mut g = self.action_mu_for_hard_limit.lock();
        *g = re_arrange_fallback(a, g.take());
    }

    /// Sets the soft-limit action, with the original action as its fallback (by priority).
    // go: Tracker.FallbackOldAndSetNewActionForSoftLimit L1
    pub fn fallback_old_and_set_new_action_for_soft_limit(&self, a: Option<ActionRef>) {
        let mut g = self.action_mu_for_soft_limit.lock();
        *g = re_arrange_fallback(a, g.take());
    }

    /// The hard-limit action (tests).
    // go: Tracker.GetFallbackForTest L1
    pub fn get_fallback_for_test(&self, ignore_finished_action: bool) -> Option<ActionRef> {
        let mut g = self.action_mu_for_hard_limit.lock();
        if let Some(a) = g.as_ref()
            && a.is_finished()
            && ignore_finished_action
        {
            *g = a.get_fallback();
        }
        g.clone()
    }

    /// Unbinds the hard-limit and soft-limit actions.
    // go: Tracker.UnbindActions L1
    pub fn unbind_actions(&self) {
        let mut soft = self.action_mu_for_soft_limit.lock();
        *soft = None;
        // Only called by ResetContextOfStmt, which then always calls SetActionOnExceed.
        let mut hard = self.action_mu_for_hard_limit.lock();
        *hard = None;
    }

    /// Unbinds `action_to_unbind` from the hard-limit chain.
    // go: Tracker.UnbindActionFromHardLimit L1
    pub fn unbind_action_from_hard_limit(&self, action_to_unbind: &ActionRef) {
        let mut g = self.action_mu_for_hard_limit.lock();
        let mut prev: Option<ActionRef> = None;
        let mut current = g.clone();
        while let Some(cur) = current {
            if same_action(&cur, action_to_unbind) {
                match &prev {
                    // action_to_unbind is the first element
                    None => *g = cur.get_fallback(),
                    // action_to_unbind is not the first element
                    Some(p) => p.set_fallback(cur.get_fallback()),
                }
                break;
            }
            current = cur.get_fallback();
            prev = Some(cur);
        }
    }

    /// Sets the label (detaching and re-attaching to the parent).
    // go: Tracker.SetLabel L1
    pub fn set_label(self: &Arc<Self>, label: isize) -> Result<(), Error> {
        let parent = self.get_parent();
        self.detach()?;
        self.label.store(label, SeqCst);
        if let Some(parent) = parent {
            self.attach_to(&parent)?;
        }
        Ok(())
    }

    // go: Tracker.Label L1
    pub fn label(&self) -> isize {
        self.label.load(SeqCst)
    }

    /// Attaches this tracker as a child of `parent`, removing it from its old parent first;
    /// its consumption is added to the new ancestors.
    // go: Tracker.AttachTo L1
    pub fn attach_to(self: &Arc<Self>, parent: &Arc<Tracker>) -> Result<(), Error> {
        if parent.is_global.load(SeqCst) {
            return self.attach_to_global_tracker(Some(parent));
        }
        if let Some(old_parent) = self.get_parent() {
            old_parent.remove(self)?;
        }
        parent
            .children
            .lock()
            .get_or_insert_with(HashMap::new)
            .entry(self.label())
            .or_default()
            .push(self.clone());

        self.set_parent(Some(parent));
        parent.consume(self.bytes_consumed())
    }

    /// Detaches the tracker from its parent.
    // go: Tracker.Detach L1
    pub fn detach(self: &Arc<Self>) -> Result<(), Error> {
        let Some(parent) = self.get_parent() else {
            return Ok(());
        };
        if parent.is_global.load(SeqCst) {
            return self.detach_from_global_tracker();
        }
        if parent.is_root_tracker_of_sess.load(SeqCst) && self.label() != LabelForMemDB {
            *parent.action_mu_for_hard_limit.lock() = None;
            *parent.action_mu_for_soft_limit.lock() = None;
            parent
                .killer
                .load()
                .as_ref()
                .expect("runtime error: invalid memory address or nil pointer dereference")
                .reset();
        }
        parent.remove(self)?;
        let _children = self.children.lock();
        self.set_parent(None);
        Ok(())
    }

    // go: Tracker.remove L1
    fn remove(self: &Arc<Self>, old_child: &Arc<Tracker>) -> Result<(), Error> {
        let mut found = false;
        let label = old_child.label();
        {
            let mut g = self.children.lock();
            if let Some(children) = g.as_mut()
                && let Some(list) = children.get_mut(&label)
                && let Some(i) = list.iter().position(|c| Arc::ptr_eq(c, old_child))
            {
                list.remove(i);
                if list.is_empty() {
                    children.remove(&label);
                }
                found = true;
            }
        }
        if found {
            old_child.set_parent(None);
            self.consume(-old_child.bytes_consumed())?;
        }
        Ok(())
    }

    /// Replaces `old_child` with `new_child` (removes it when `new_child` is `None`); the old
    /// child's consumption is removed and the new one's added.
    // go: Tracker.ReplaceChild L1
    pub fn replace_child(
        self: &Arc<Self>,
        old_child: &Arc<Tracker>,
        new_child: Option<&Arc<Tracker>>,
    ) -> Result<(), Error> {
        let Some(new_child) = new_child else {
            return self.remove(old_child);
        };

        if old_child.label() != new_child.label() {
            self.remove(old_child)?;
            return new_child.attach_to(self);
        }

        let mut new_consumed = new_child.bytes_consumed();
        new_child.set_parent(Some(self));

        let label = old_child.label();
        {
            let mut g = self.children.lock();
            if let Some(children) = g.as_mut()
                && let Some(list) = children.get_mut(&label)
                && let Some(slot) = list.iter_mut().find(|c| Arc::ptr_eq(c, old_child))
            {
                new_consumed -= old_child.bytes_consumed();
                old_child.set_parent(None);
                *slot = new_child.clone();
            }
        }

        self.consume(new_consumed)
    }

    /// Consumes `bs` bytes (negative: releases). When the usage of a tracker exceeds its soft
    /// or hard limit, the topmost such tracker runs its action; a kill signal of the session
    /// root tracker is returned as its error (Go panics with it).
    // go: Tracker.Consume L2 (panic(err) → Err, N3 3.4)
    pub fn consume(self: &Arc<Self>, bs: i64) -> Result<(), Error> {
        if bs == 0 {
            return Ok(());
        }
        let walk = self.walk(bs);
        if bs > 0 { walk.act() } else { Ok(()) }
    }

    /// The accounting half of `Consume` (A1 3.3 item 2): adds `bs` along the parent chain and
    /// updates `MaxConsumed`, without running any action. Never fails; called by
    /// [`crate::TrackedAlloc`]. The actions run at the next [`Tracker::checkpoint`].
    // glue: A1 3.3 hot path (the loop of Go Consume, without the actions)
    pub fn record(self: &Arc<Self>, bs: i64) {
        if bs != 0 {
            let _ = self.walk(bs);
        }
    }

    /// The action half of `Consume` (A1 3.3 item 3), at the place where Go calls `Consume`
    /// with a positive delta: updates the Top1 session, returns the kill error, then runs the
    /// hard-limit and the soft-limit action of the topmost exceeding trackers, judged on the
    /// current consumption (`TrackedAlloc` has already recorded it).
    // glue: A1 3.3 checkpoint (the tail of Go Consume for bs > 0)
    pub fn checkpoint(self: &Arc<Self>) -> Result<(), Error> {
        self.walk(0).act()
    }

    // go-part: Tracker.Consume (the loop up the parent chain; bs == 0 only evaluates)
    fn walk(self: &Arc<Self>, bs: i64) -> Walk {
        let mut w = Walk::default();
        let mut next = Some(self.clone());
        while let Some(tracker) = next {
            if tracker.is_root_tracker_of_sess.load(SeqCst) {
                w.session_root_tracker = Some(tracker.clone());
            }
            let bytes_consumed = if bs == 0 {
                tracker.bytes_consumed.load(SeqCst)
            } else {
                tracker
                    .bytes_consumed
                    .fetch_add(bs, SeqCst)
                    .wrapping_add(bs)
            };
            let bytes_released = tracker.bytes_released.load(SeqCst);
            let limits = tracker.bytes_limit.load();
            if bytes_consumed.wrapping_add(bytes_released) >= limits.bytes_hard_limit
                && limits.bytes_hard_limit > 0
            {
                w.root_exceed = Some(tracker.clone());
            }
            if bytes_consumed.wrapping_add(bytes_released) >= limits.bytes_soft_limit
                && limits.bytes_soft_limit > 0
            {
                w.root_exceed_for_soft_limit = Some(tracker.clone());
            }

            if bs != 0 {
                loop {
                    let max_now = tracker.max_consumed.load(SeqCst);
                    let consumed = tracker.bytes_consumed.load(SeqCst);
                    if consumed > max_now
                        && tracker
                            .max_consumed
                            .compare_exchange(max_now, consumed, SeqCst, SeqCst)
                            .is_err()
                    {
                        continue;
                    }
                    set_metrics(tracker.label(), true, consumed);
                    break;
                }
            }
            next = tracker.get_parent();
        }
        w
    }

    /// Returns the kill error of the session root tracker, if any (Go panics with it).
    // go: Tracker.HandleKillSignal L2 (panic(err) → Err, N3 3.4)
    pub fn handle_kill_signal(&self) -> Result<(), Error> {
        // The killer of the topmost session root tracker on the path (Go keeps the last one
        // found walking up).
        let mut session_root_killer: Option<Option<Arc<SqlKiller>>> = None;
        if self.is_root_tracker_of_sess.load(SeqCst) {
            session_root_killer = Some(self.killer.load_full());
        }
        let mut next = self.get_parent();
        while let Some(tracker) = next {
            if tracker.is_root_tracker_of_sess.load(SeqCst) {
                session_root_killer = Some(tracker.killer.load_full());
            }
            next = tracker.get_parent();
        }
        if let Some(killer) = session_root_killer {
            killer
                .expect("runtime error: invalid memory address or nil pointer dereference")
                .handle_signal()?;
        }
        Ok(())
    }

    /// Buffers memory usage and consumes it late (not thread-safe: one goroutine / task).
    // go: Tracker.BufferedConsume L1
    pub fn buffered_consume(
        self: &Arc<Self>,
        buffered_mem_size: &mut i64,
        bytes: i64,
    ) -> Result<(), Error> {
        *buffered_mem_size += bytes;
        if *buffered_mem_size >= TrackMemWhenExceeds {
            self.consume(*buffered_mem_size)?;
            *buffered_mem_size = 0;
        }
        Ok(())
    }

    /// Releases tracked memory. Go keeps GC-aware trackers' released bytes until the GC runs a
    /// finalizer; Rust frees at `Drop`, so they are released at once (A1 3.2).
    // go: Tracker.Release L2 (no GC finalizer: recordRelease and release run together)
    pub fn release(self: &Arc<Self>, bytes: i64) -> Result<(), Error> {
        if bytes == 0 {
            return Ok(());
        }
        let mut next = Some(self.clone());
        while let Some(tracker) = next {
            if tracker.should_record_release() {
                tracker.record_release(bytes);
                tracker.release_recorded(bytes);
                break;
            }
            next = tracker.get_parent();
        }
        self.consume(-bytes)
    }

    /// Buffers memory release and releases it late (not thread-safe).
    // go: Tracker.BufferedRelease L1
    pub fn buffered_release(
        self: &Arc<Self>,
        buffered_mem_size: &mut i64,
        bytes: i64,
    ) -> Result<(), Error> {
        *buffered_mem_size += bytes;
        if *buffered_mem_size >= TrackMemWhenExceeds {
            self.release(*buffered_mem_size)?;
            *buffered_mem_size = 0;
        }
        Ok(())
    }

    // go: Tracker.shouldRecordRelease L1
    fn should_record_release(&self) -> bool {
        EnableGCAwareMemoryTrack.load(SeqCst) && self.label() == LabelForGlobalAnalyzeMemory
    }

    // go: Tracker.recordRelease L1
    fn record_release(self: &Arc<Self>, bytes: i64) {
        let mut next = Some(self.clone());
        while let Some(tracker) = next {
            let bytes_released = tracker.bytes_released.fetch_add(bytes, SeqCst) + bytes;
            set_metrics(tracker.label(), false, bytes_released);
            next = tracker.get_parent();
        }
    }

    // go: Tracker.release L1
    fn release_recorded(self: &Arc<Self>, bytes: i64) {
        let mut next = Some(self.clone());
        while let Some(tracker) = next {
            let bytes_released = tracker.bytes_released.fetch_sub(bytes, SeqCst) - bytes;
            set_metrics(tracker.label(), false, bytes_released);
            next = tracker.get_parent();
        }
    }

    /// The consumed memory in bytes.
    // go: Tracker.BytesConsumed L1
    pub fn bytes_consumed(&self) -> i64 {
        self.bytes_consumed.load(SeqCst)
    }

    /// The released (not yet collected) memory in bytes.
    // go: Tracker.BytesReleased L1
    pub fn bytes_released(&self) -> i64 {
        self.bytes_released.load(SeqCst)
    }

    /// The maximum number of bytes consumed during execution (never -1: the binary plan uses
    /// -1 for "N/A").
    // go: Tracker.MaxConsumed L1
    pub fn max_consumed(&self) -> i64 {
        self.max_consumed.load(SeqCst)
    }

    /// Resets `MaxConsumed` to the current consumption (before each statement).
    // go: Tracker.ResetMaxConsumed L1
    pub fn reset_max_consumed(&self) {
        self.max_consumed.store(self.bytes_consumed(), SeqCst);
    }

    /// Go `SessionID.Load()`.
    // glue: accessor of the atomic SessionID field
    pub fn session_id(&self) -> u64 {
        self.session_id.load(SeqCst)
    }

    /// The tracker with `label` among this tracker and its children. Go reads the children
    /// map without the lock; Rust takes it (no other difference).
    // go: Tracker.SearchTrackerWithoutLock L2 (takes the children lock: no unsynchronized read)
    pub fn search_tracker_without_lock(self: &Arc<Self>, label: isize) -> Option<Arc<Tracker>> {
        if self.label() == label {
            return Some(self.clone());
        }
        let g = self.children.lock();
        g.as_ref()?.get(&label)?.first().cloned()
    }

    /// The children consuming more than `limit` bytes.
    // go: Tracker.SearchTrackerConsumedMoreThanNBytes L1
    pub fn search_tracker_consumed_more_than_n_bytes(&self, limit: i64) -> Vec<Arc<Tracker>> {
        let g = self.children.lock();
        let mut res = Vec::new();
        for child_slice in g.iter().flat_map(|m| m.values()) {
            for tracker in child_slice {
                if tracker.bytes_consumed() > limit {
                    res.push(tracker.clone());
                }
            }
        }
        res
    }

    /// The string representation of this tracker tree.
    // go: Tracker.String L1
    pub fn string(&self) -> String {
        let mut buffer = String::from("\n");
        self.to_string_indent("", &mut buffer);
        buffer
    }

    // go: Tracker.toString L1
    fn to_string_indent(&self, indent: &str, buffer: &mut String) {
        buffer.push_str(&format!("{indent}\"{}\"{{\n", self.label()));
        let bytes_limit = self.get_bytes_limit();
        if bytes_limit > 0 {
            buffer.push_str(&format!(
                "{indent}  \"quota\": {}\n",
                format_bytes(bytes_limit)
            ));
        }
        buffer.push_str(&format!(
            "{indent}  \"consumed\": {}\n",
            format_bytes(self.bytes_consumed())
        ));

        let g = self.children.lock();
        if let Some(children) = g.as_ref() {
            let mut labels: Vec<isize> = children.keys().copied().collect();
            labels.sort_unstable();
            let indent2 = format!("{indent}  ");
            for label in labels {
                for child in &children[&label] {
                    child.to_string_indent(&indent2, buffer);
                }
            }
        }
        drop(g);
        buffer.push_str(indent);
        buffer.push_str("}\n");
    }

    /// Go method `FormatBytes` (the receiver is unused).
    // go: Tracker.FormatBytes L1
    pub fn format_bytes(&self, num_bytes: i64) -> String {
        format_bytes(num_bytes)
    }

    /// Go method `LessThan` on possibly nil trackers: whether `t` consumes less than `t2`.
    // go: Tracker.LessThan L2 (nil receiver → Option)
    pub fn less_than(t: Option<&Tracker>, t2: Option<&Tracker>) -> bool {
        match (t, t2) {
            (None, _) => true,
            (_, None) => false,
            (Some(a), Some(b)) => a.bytes_consumed() < b.bytes_consumed(),
        }
    }

    /// Attaches the tracker to a global tracker (at the initialization of a session
    /// executor's tracker). Panics if `global_tracker` is not global, as in Go.
    // go: Tracker.AttachToGlobalTracker L1
    pub fn attach_to_global_tracker(
        self: &Arc<Self>,
        global_tracker: Option<&Arc<Tracker>>,
    ) -> Result<(), Error> {
        let Some(global_tracker) = global_tracker else {
            return Ok(());
        };
        if !global_tracker.is_global.load(SeqCst) {
            panic!("Attach to a non-GlobalTracker");
        }
        if let Some(parent) = self.get_parent() {
            if parent.is_global.load(SeqCst) {
                parent.consume(-self.bytes_consumed())?;
            } else {
                parent.remove(self)?;
            }
        }
        self.set_parent(Some(global_tracker));
        global_tracker.consume(self.bytes_consumed())
    }

    /// Detaches the tracker from its global parent (panics if the parent is not global).
    // go: Tracker.DetachFromGlobalTracker L1
    pub fn detach_from_global_tracker(&self) -> Result<(), Error> {
        let Some(parent) = self.get_parent() else {
            return Ok(());
        };
        if !parent.is_global.load(SeqCst) {
            panic!("Detach from a non-GlobalTracker");
        }
        parent.consume(-self.bytes_consumed())?;
        self.set_parent(None);
        Ok(())
    }

    /// Sets the consumption to `bytes`.
    // go: Tracker.ReplaceBytesUsed L1
    pub fn replace_bytes_used(self: &Arc<Self>, bytes: i64) -> Result<(), Error> {
        self.consume(bytes - self.bytes_consumed())
    }

    /// Detaches the tracker and clears its children; label and limit are kept.
    // go: Tracker.Reset L1
    pub fn reset(self: &Arc<Self>) -> Result<(), Error> {
        self.detach()?;
        self.replace_bytes_used(0)?;
        *self.children.lock() = None;
        Ok(())
    }

    // go: Tracker.getParent L1
    fn get_parent(&self) -> Option<Arc<Tracker>> {
        self.parent.lock().upgrade()
    }

    // go: Tracker.setParent L1
    fn set_parent(&self, parent: Option<&Arc<Tracker>>) {
        *self.parent.lock() = parent.map(Arc::downgrade).unwrap_or_default();
    }

    /// The memory used by the tree, keyed by the label path (`[a] <- [b]`).
    // go: Tracker.CountAllChildrenMemUse L1
    pub fn count_all_children_mem_use(&self) -> HashMap<String, i64> {
        let mut tracker_mem_use_map = HashMap::with_capacity(1024);
        count_child_mem(self, String::new(), &mut tracker_mem_use_map);
        tracker_mem_use_map
    }

    /// The children (tests).
    // go: Tracker.GetChildrenForTest L1
    pub fn get_children_for_test(&self) -> Vec<Arc<Tracker>> {
        let g = self.children.lock();
        g.iter()
            .flat_map(|m| m.values())
            .flatten()
            .cloned()
            .collect()
    }

    /// Whether this is a global tracker.
    // glue: accessor (Go reads the unexported isGlobal field inside the package)
    pub fn is_global(&self) -> bool {
        self.is_global.load(SeqCst)
    }
}

/// What the loop of Go `Consume` found: the topmost trackers over their hard / soft limit and
/// the topmost session root tracker.
#[derive(Default)]
struct Walk {
    root_exceed: Option<Arc<Tracker>>,
    root_exceed_for_soft_limit: Option<Arc<Tracker>>,
    session_root_tracker: Option<Arc<Tracker>>,
}

impl Walk {
    // go-part: Tracker.Consume (after the loop, for bs > 0: Top1, kill signal, actions)
    fn act(&self) -> Result<(), Error> {
        if let Some(root) = &self.session_root_tracker {
            // Update the Top1 session
            let mem_usage = root.bytes_consumed();
            let limit_sess_min_size = ServerMemoryLimitSessMinSize.load(SeqCst);
            if mem_usage as u64 >= limit_sess_min_size {
                let mut old_tracker = MemUsageTop1Tracker.load_full();
                while Tracker::less_than(old_tracker.as_deref(), Some(root)) {
                    let prev =
                        MemUsageTop1Tracker.compare_and_swap(&old_tracker, Some(root.clone()));
                    if ptr_eq_opt(prev.as_ref(), old_tracker.as_ref()) {
                        break;
                    }
                    old_tracker = MemUsageTop1Tracker.load_full();
                }
            }
        }

        if let Some(root) = &self.session_root_tracker {
            root.killer
                .load()
                .as_ref()
                .expect("runtime error: invalid memory address or nil pointer dereference")
                .handle_signal()?;
        }

        if let Some(t) = &self.root_exceed {
            try_action(&t.action_mu_for_hard_limit, t)?;
        }

        if let Some(t) = &self.root_exceed_for_soft_limit {
            try_action(&t.action_mu_for_soft_limit, t)?;
        }
        Ok(())
    }
}

// glue: pointer identity of two optional trackers (Go `==` on *Tracker)
fn ptr_eq_opt(a: Option<&Arc<Tracker>>, b: Option<&Arc<Tracker>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

// go: countChildMem L1
fn count_child_mem(t: &Tracker, mut family_tree_name: String, map: &mut HashMap<String, i64>) {
    if !family_tree_name.is_empty() {
        family_tree_name.push_str(" <- ");
    }
    family_tree_name.push_str(&format!("[{}]", t.label()));
    *map.entry(family_tree_name.clone()).or_insert(0) += t.bytes_consumed();
    let g = t.children.lock();
    for sli in g.iter().flat_map(|m| m.values()) {
        for tracker in sli {
            count_child_mem(tracker, family_tree_name.clone(), map);
        }
    }
}

const byteSizeGB: i64 = 1 << 30;
const byteSizeMB: i64 = 1 << 20;
const byteSizeKB: i64 = 1 << 10;
const byteSizeBB: i64 = 1;

/// Converts a memory consumption to a readable string (`%v` of the float, as Go).
// go: BytesToString L1
pub fn bytes_to_string(num_bytes: i64) -> String {
    let gb = num_bytes as f64 / byteSizeGB as f64;
    if gb > 1.0 {
        return go_sprintf("%v GB", &[Arg::from(gb)]);
    }

    let mb = num_bytes as f64 / byteSizeMB as f64;
    if mb > 1.0 {
        return go_sprintf("%v MB", &[Arg::from(mb)]);
    }

    let kb = num_bytes as f64 / byteSizeKB as f64;
    if kb > 1.0 {
        return go_sprintf("%v KB", &[Arg::from(kb)]);
    }

    go_sprintf("%v Bytes", &[Arg::from(num_bytes)])
}

// glue: Go fmt.Sprintf with an ASCII result (numbers and units)
fn go_sprintf(format: &str, args: &[Arg]) -> String {
    String::from_utf8(sprintf(format, args)).expect("ASCII")
}

/// Formats bytes, pruning precision first (`1.25 GB`, `512 KB`, `10.5 MB`).
// go: FormatBytes L1
pub fn format_bytes(num_bytes: i64) -> String {
    if num_bytes <= byteSizeKB {
        return bytes_to_string(num_bytes);
    }
    let (unit, unit_str) = get_byte_unit(num_bytes);
    if unit == byteSizeBB {
        return bytes_to_string(num_bytes);
    }
    let v = num_bytes as f64 / unit as f64;
    let mut decimal = 1;
    if num_bytes % unit == 0 {
        decimal = 0;
    } else if v < 10.0 {
        decimal = 2;
    }
    // strconv.FormatFloat(v, 'f', decimal, 64) is fmt's %.<decimal>f.
    let num = go_sprintf(&format!("%.{decimal}f"), &[Arg::from(v)]);
    format!("{num} {unit_str}")
}

// go: getByteUnit L1
fn get_byte_unit(b: i64) -> (i64, &'static str) {
    if b > byteSizeGB {
        (byteSizeGB, "GB")
    } else if b > byteSizeMB {
        (byteSizeMB, "MB")
    } else if b > byteSizeKB {
        (byteSizeKB, "KB")
    } else {
        (byteSizeBB, "Bytes")
    }
}

/// The label of the SQL Text.
pub const LabelForSQLText: isize = -1;
/// The label of the index worker.
pub const LabelForIndexWorker: isize = -2;
/// The label of the inner list.
pub const LabelForInnerList: isize = -3;
/// The label of the inner table.
pub const LabelForInnerTable: isize = -4;
/// The label of the outer table.
pub const LabelForOuterTable: isize = -5;
/// The label of the coprocessor.
pub const LabelForCoprocessor: isize = -6;
/// The label of the chunk list.
pub const LabelForChunkList: isize = -7;
/// The label of the Global SimpleLRUCache.
pub const LabelForGlobalSimpleLRUCache: isize = -8;
/// The label of the chunk list in disk.
pub const LabelForChunkDataInDiskByRows: isize = -9;
/// The label of the row container.
pub const LabelForRowContainer: isize = -10;
/// The label of the Global Storage.
pub const LabelForGlobalStorage: isize = -11;
/// The label of the Global Memory.
pub const LabelForGlobalMemory: isize = -12;
/// The label of the BuildSideResult.
pub const LabelForBuildSideResult: isize = -13;
/// The label of the row chunks.
pub const LabelForRowChunks: isize = -14;
/// The label of the stats cache.
pub const LabelForStatsCache: isize = -15;
/// The label of the outer list.
pub const LabelForOuterList: isize = -16;
/// The label of the apply cache.
pub const LabelForApplyCache: isize = -17;
/// The label of the simple task.
pub const LabelForSimpleTask: isize = -18;
/// The label of CTE storage.
pub const LabelForCTEStorage: isize = -19;
/// The label of IndexJoin InnerWorker.
pub const LabelForIndexJoinInnerWorker: isize = -20;
/// The label of IndexJoin OuterWorker.
pub const LabelForIndexJoinOuterWorker: isize = -21;
/// The label of the bind cache.
pub const LabelForBindCache: isize = -22;
/// The label of the non-transactional DML.
pub const LabelForNonTransactionalDML: isize = -23;
/// The label of the memory of each analyze job.
pub const LabelForAnalyzeMemory: isize = -24;
/// The label of the global memory of all analyze jobs.
pub const LabelForGlobalAnalyzeMemory: isize = -25;
/// The label of the prepared plan cache memory usage.
pub const LabelForPreparedPlanCache: isize = -26;
/// The label of a session.
pub const LabelForSession: isize = -27;
/// The label of the MemDB.
pub const LabelForMemDB: isize = -28;
/// The label of the execution of cursor fetch.
pub const LabelForCursorFetch: isize = -29;
/// The label of the chunk list in disk.
pub const LabelForChunkDataInDiskByChunks: isize = -30;
/// The label of the sort partition.
pub const LabelForSortPartition: isize = -31;
/// The label of the hash join v2's hash table.
pub const LabelForHashTableInHashJoinV2: isize = -32;

/// Go `MetricsTypes`: `[LblModule, heap-in-use type, released type]` of a label.
// glue: Go map literal `MetricsTypes` (one entry) as a match
#[allow(non_snake_case)]
pub fn MetricsTypes(label: isize) -> Option<[&'static str; 3]> {
    match label {
        LabelForGlobalAnalyzeMemory => Some(["analyze", "inuse", "released"]),
        _ => None,
    }
}
