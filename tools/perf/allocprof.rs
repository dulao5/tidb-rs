// Allocation profiler for tidb-server (measurement only; never part of a release build).
// tools/perf/alloc-profile.sh copies this file into the server crate for one build. It counts
// every allocation by its call stack while the file named by ALLOC_PROF_FLAG exists; when the
// file is removed it writes the stacks with their counts to ALLOC_PROF_OUT
// (tools/perf/alloc_report.py summarises them).
use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tikv_jemallocator::Jemalloc;

const DEPTH: usize = 64;
static ON: AtomicBool = AtomicBool::new(false);
thread_local! { static IN: Cell<bool> = const { Cell::new(false) }; }
type Stacks = HashMap<[usize; DEPTH], (u64, u64)>; // (allocs, reallocs)
static MAP: Mutex<Option<Stacks>> = Mutex::new(None);

pub struct Prof;

fn record(realloc: bool) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    let _ = IN.try_with(|f| {
        if f.get() {
            return;
        }
        f.set(true);
        let mut ips = [0usize; DEPTH];
        let mut n = 0;
        unsafe {
            backtrace::trace_unsynchronized(|fr| {
                ips[n] = fr.ip() as usize;
                n += 1;
                n < DEPTH
            });
        }
        let mut m = MAP.lock().unwrap();
        let e = m.get_or_insert_with(HashMap::new).entry(ips).or_default();
        if realloc {
            e.1 += 1;
        } else {
            e.0 += 1;
        }
        f.set(false);
    });
}

unsafe impl GlobalAlloc for Prof {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        record(false);
        unsafe { Jemalloc.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        record(false);
        unsafe { Jemalloc.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { Jemalloc.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        record(true);
        unsafe { Jemalloc.realloc(p, l, n) }
    }
}

fn skip(name: &str) -> bool {
    const P: &[&str] = &[
        "allocprof", "alloc::", "<alloc::", "core::", "<core::", "std::", "<std::", "__rust",
        "hashbrown::", "<hashbrown::", "_rjem", "backtrace::", "smallvec::", "<smallvec::",
        "bytes::", "<bytes::", "rust_begin", "<T as alloc", "<[T]", "<T as core", "indexmap::",
        "<indexmap::", "<T as ", "<&T as ", "<str as", "<String as", "<Vec<",
    ];
    P.iter().any(|p| name.starts_with(p))
}

fn strip_hash(s: String) -> String {
    match s.rfind("::h") {
        Some(i) if s.len() - i == 19 => s[..i].to_owned(),
        _ => s,
    }
}

fn frames(ips: &[usize; DEPTH]) -> Vec<String> {
    let mut out = Vec::new();
    for &ip in ips.iter().take_while(|ip| **ip != 0) {
        let mut names = Vec::new();
        backtrace::resolve(ip as *mut _, |sym| {
            let name = sym.name().map(|n| strip_hash(n.to_string())).unwrap_or_default();
            let loc = match (sym.filename(), sym.lineno()) {
                (Some(f), Some(l)) => format!("{}:{}", f.display(), l),
                _ => String::new(),
            };
            names.push(format!("{name}  [{loc}]"));
        });
        // Inlined frames come innermost first.
        out.extend(names);
    }
    out
}

fn report(path: &str) {
    let m = MAP.lock().unwrap().take().unwrap_or_default();
    let mut s = String::new();
    for (ips, c) in &m {
        s += &format!("@ {} {}\n", c.0, c.1);
        for f in frames(ips) {
            s += &format!("\t{f}\n");
        }
    }
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, s).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

pub fn start() {
    let (Ok(flag), Ok(out)) = (std::env::var("ALLOC_PROF_FLAG"), std::env::var("ALLOC_PROF_OUT"))
    else {
        return;
    };
    std::thread::spawn(move || {
        loop {
            while !std::path::Path::new(&flag).exists() {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            ON.store(true, Ordering::SeqCst);
            while std::path::Path::new(&flag).exists() {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            ON.store(false, Ordering::SeqCst);
            IN.with(|f| f.set(true));
            report(&out);
            IN.with(|f| f.set(false));
        }
    });
}
