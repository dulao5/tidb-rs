//! Per-statement phase timing, off by default (`tidb-server --stmt-timing SECS`).
//!
//! Each SELECT is classified by plan shape ([`Kind`]) and its wall-clock time is split into
//! [`Phase`]s. Sessions accumulate locally and fold into process-wide counters every
//! [`FLUSH_EVERY`] statements, so enabling it adds a few `Instant::now()` calls per statement
//! and no shared-cache-line traffic per statement. [`report`] formats and resets the counters.

use std::fmt::Write as _;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;
use std::time::Instant;

use tidb_sql::Plan;
use tidb_sql::plan::Source;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turn collection on or off (process-wide).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// Statement shape, matching the oltp_read_only statements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `WHERE id = ?`
    PointGet,
    /// `WHERE id BETWEEN ? AND ?`
    Range,
    /// `SELECT SUM(k) ... BETWEEN`
    Sum,
    /// `... BETWEEN ... ORDER BY c`
    OrderBy,
    /// `SELECT DISTINCT c ... BETWEEN ... ORDER BY c`
    Distinct,
    /// Anything else that reads a table.
    Other,
}

const KINDS: usize = 6;
const KIND_NAMES: [&str; KINDS] = ["point_get", "range", "sum", "order_by", "distinct", "other"];

impl Kind {
    pub fn of(plan: &Plan) -> Kind {
        match (&plan.source, &plan.agg, plan.sort.is_empty()) {
            (Source::PointGet { .. }, _, _) => Kind::PointGet,
            (Source::TableReader { .. }, None, true) => Kind::Range,
            (Source::TableReader { .. }, None, false) => Kind::OrderBy,
            (Source::TableReader { .. }, Some(a), true) if a.group_by.is_empty() => Kind::Sum,
            (Source::TableReader { .. }, Some(a), false) if !a.group_by.is_empty() => {
                Kind::Distinct
            }
            _ => Kind::Other,
        }
    }
}

/// Phases of one statement. `Total` is the server-side time from the command packet being
/// read to the response being flushed; the other phases are parts of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Parsing (COM_QUERY) or binding parameters (COM_STMT_EXECUTE).
    Parse,
    /// Waiting for the PD timestamp.
    Tso,
    /// Waiting for the schema check (`SchemaCache`).
    Schema,
    /// Planning.
    Plan,
    /// Reading from TiKV (point get or coprocessor), including decoding.
    KvRead,
    /// Root operators and conversion to protocol values.
    Finish,
    /// Encoding and writing the response (incl. TLS) and flushing.
    Write,
    Total,
}

const PHASES: usize = 8;
const PHASE_NAMES: [&str; PHASES] = [
    "parse", "tso", "schema", "plan", "kv_read", "finish", "write", "total",
];

struct Cell {
    ns: AtomicU64,
    count: AtomicU64,
}

#[allow(clippy::declare_interior_mutable_const)]
const CELL: Cell = Cell {
    ns: AtomicU64::new(0),
    count: AtomicU64::new(0),
};
#[allow(clippy::declare_interior_mutable_const)]
const ROW: [Cell; PHASES] = [CELL; PHASES];
static GLOBAL: [[Cell; PHASES]; KINDS] = [ROW; KINDS];

/// Statements a session accumulates before folding into the global counters.
const FLUSH_EVERY: u32 = 256;

/// A session's timing state: the statement in flight and the not yet flushed sums.
#[derive(Default)]
pub struct Timer {
    on: bool,
    last: Option<Instant>,
    kind: Option<Kind>,
    current: [u64; PHASES],
    local: [[(u64, u64); PHASES]; KINDS],
    pending: u32,
}

impl Timer {
    /// Start a statement (a no-op unless collection is on).
    pub fn begin(&mut self) {
        self.start(enabled());
    }

    fn start(&mut self, on: bool) {
        self.on = on;
        self.kind = None;
        self.current = [0; PHASES];
        self.last = self.on.then(Instant::now);
    }

    /// Charge the time since the previous mark (or `begin`) to `phase`.
    pub fn mark(&mut self, phase: Phase) {
        if let Some(last) = self.last {
            let now = Instant::now();
            self.add(phase, now - last);
            self.last = Some(now);
        }
    }

    /// Restart the clock without charging anything (time spent elsewhere is "other").
    pub fn skip(&mut self) {
        if self.on {
            self.last = Some(Instant::now());
        }
    }

    pub fn add(&mut self, phase: Phase, d: Duration) {
        if self.on {
            self.current[phase as usize] += u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        }
    }

    pub fn set_kind(&mut self, kind: Kind) {
        if self.on {
            self.kind = Some(kind);
        }
    }

    /// End the statement: `write` and `total` come from the protocol layer. Statements that
    /// did not read a table (no kind) are not recorded.
    pub fn end(&mut self, write: Duration, total: Duration) {
        if !self.on {
            return;
        }
        self.on = false;
        self.last = None;
        let Some(kind) = self.kind.take() else { return };
        self.add_raw(Phase::Write, write);
        self.add_raw(Phase::Total, total);
        let row = &mut self.local[kind as usize];
        for (p, ns) in self.current.iter().enumerate() {
            row[p].0 += ns;
            row[p].1 += 1;
        }
        self.pending += 1;
        if self.pending >= FLUSH_EVERY {
            self.flush();
        }
    }

    fn add_raw(&mut self, phase: Phase, d: Duration) {
        self.current[phase as usize] += u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
    }

    /// Fold the local sums into the global counters.
    pub fn flush(&mut self) {
        if self.pending == 0 {
            return;
        }
        self.pending = 0;
        for (k, row) in self.local.iter_mut().enumerate() {
            for (p, (ns, count)) in row.iter_mut().enumerate() {
                if *count > 0 {
                    GLOBAL[k][p].ns.fetch_add(*ns, Relaxed);
                    GLOBAL[k][p].count.fetch_add(*count, Relaxed);
                    *ns = 0;
                    *count = 0;
                }
            }
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.flush();
    }
}

/// A table of average microseconds per statement and phase since the last report, then reset.
/// Counts are of statements flushed by sessions (up to [`FLUSH_EVERY`] - 1 per session lag).
pub fn report() -> String {
    let mut out = String::new();
    let _ = write!(out, "{:<10} {:>9}", "kind", "count");
    for name in PHASE_NAMES {
        let _ = write!(out, " {name:>8}");
    }
    let _ = write!(out, " {:>8}", "other");
    out.push_str("   (avg us per statement)\n");
    for (k, row) in GLOBAL.iter().enumerate() {
        let taken: Vec<(u64, u64)> = row
            .iter()
            .map(|c| (c.ns.swap(0, Relaxed), c.count.swap(0, Relaxed)))
            .collect();
        let count = taken[Phase::Total as usize].1;
        if count == 0 {
            continue;
        }
        let _ = write!(out, "{:<10} {count:>9}", KIND_NAMES[k]);
        let avg = |ns: u64| ns as f64 / count as f64 / 1000.0;
        let mut parts = 0;
        for (p, (ns, _)) in taken.iter().enumerate() {
            let _ = write!(out, " {:>8.1}", avg(*ns));
            if p != Phase::Total as usize {
                parts += ns;
            }
        }
        let total = taken[Phase::Total as usize].0;
        let _ = write!(out, " {:>8.1}", avg(total.saturating_sub(parts)));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_timer_records_nothing() {
        let mut t = Timer::default();
        t.start(false);
        t.mark(Phase::Tso);
        assert!(t.last.is_none());
        t.set_kind(Kind::PointGet);
        t.end(Duration::from_micros(5), Duration::from_micros(10));
        assert_eq!(t.pending, 0);
    }

    #[test]
    fn enabled_timer_accumulates_per_kind() {
        let mut t = Timer::default();
        t.start(true);
        assert!(t.last.is_some());
        t.add(Phase::Tso, Duration::from_micros(30));
        t.set_kind(Kind::Sum);
        t.end(Duration::from_micros(5), Duration::from_micros(50));
        let row = &t.local[Kind::Sum as usize];
        assert_eq!(row[Phase::Tso as usize], (30_000, 1));
        assert_eq!(row[Phase::Write as usize], (5_000, 1));
        assert_eq!(row[Phase::Total as usize], (50_000, 1));
        // A statement without a kind (e.g. SET) is not recorded.
        t.start(true);
        t.end(Duration::from_micros(5), Duration::from_micros(50));
        assert_eq!(t.local[Kind::Sum as usize][Phase::Total as usize].1, 1);
        t.pending = 0; // keep the global counters clean
    }
}
