//! N9 §6.1 / §6.1a / §9: the cost and accuracy of TiDB-side CPU accounting per poll.
//!
//! Part 1 (clock costs): ns per call of `Instant::now`, `clock_gettime(CLOCK_MONOTONIC)`
//! and `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` on this machine.
//!
//! Part 2 (overhead): a tokio runtime runs `TASKS_PER_CORE` "statements" per worker thread,
//! once with one worker (2a, `current_thread`: the per-poll cost without cross-core noise) and
//! once with one worker per core (2b, `multi_thread`); each polls `POLLS` times, spinning for a fixed
//! amount of work per poll and then yielding. Every task is wrapped in the same poll wrapper;
//! only the accounting inside it differs:
//!
//! | mode | per poll | per window |
//! |---|---|---|
//! | `off` | nothing | — |
//! | `instant` | two `Instant` reads, wall time added to the task's total (§6.1 step 1) | — |
//! | `win1ms` / `win10ms` | `instant`, plus the poll's wall time recorded for its thread | when the window has passed: one thread-CPU read, the CPU since the last read split over the recorded polls by wall time (§6.1 steps 2–3) |
//! | `perpoll` | two thread-CPU reads (the rejected design, as reference) | — |
//!
//! The sample rate (§6.1a) is decided once per task: an unsampled task only reads `Instant`
//! and adds its wall time to the thread's "other" time, so that the window's CPU is not
//! attributed to the sampled tasks alone. Reported: process CPU (`getrusage`) per poll above
//! `off`, as ns and as % of `off`. All configurations run once per round, rounds repeated
//! (`3 x CPU_ACC_REPS`), median per configuration, so drift hits all of them alike.
//!
//! Part 3 (accuracy): half the tasks do 1 unit of work per poll, half 4 units. With windowed
//! accounting the CPU attributed to the heavy tasks over the light ones should come out at
//! the same ratio `perpoll` measures, and the attributed total should match the process CPU
//! spent in polls. Run once on idle cores and once oversubscribed (as many extra spinning
//! threads as cores), where windowed accounting splits preempted time over the window's
//! tasks instead of charging the running one. Note the two measure different things:
//! `perpoll` counts CPU inside polls only; a window also splits the runtime's own CPU between
//! polls (scheduling, wakeups) by wall time, so its total is closer to the process CPU.
//!
//! `cargo bench -p tidb-runtime --bench cpu_accounting`
//!
//! Environment: `CPU_ACC_POLLS` (polls per task, default 2000), `CPU_ACC_REPS` (runs per
//! configuration, median reported, default 5), `CPU_ACC_WORK_NS` (light work per poll,
//! default 1000).

use std::cell::RefCell;
use std::future::Future;
use std::hint::black_box;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use std::time::Instant;

const TASKS_PER_CORE: usize = 8;

// ---------------------------------------------------------------- clocks

// glue: benchmark
fn thread_cpu() -> Duration {
    let ts = nix::time::clock_gettime(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID)
        .expect("CLOCK_THREAD_CPUTIME_ID");
    Duration::new(ts.tv_sec() as u64, ts.tv_nsec() as u32)
}

// glue: benchmark
fn monotonic() -> Duration {
    let ts = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).expect("monotonic");
    Duration::new(ts.tv_sec() as u64, ts.tv_nsec() as u32)
}

// glue: benchmark
fn process_cpu() -> Duration {
    let r = nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_SELF)
        .expect("getrusage");
    let tv =
        |t: nix::sys::time::TimeVal| Duration::new(t.tv_sec() as u64, (t.tv_usec() as u32) * 1000);
    tv(r.user_time()) + tv(r.system_time())
}

// glue: benchmark
fn ns_per_call(n: u32, mut f: impl FnMut()) -> f64 {
    for _ in 0..n / 10 {
        f();
    }
    let t = Instant::now();
    for _ in 0..n {
        f();
    }
    t.elapsed().as_nanos() as f64 / f64::from(n)
}

// ---------------------------------------------------------------- work

/// Spin iterations per microsecond of CPU on this machine.
// glue: benchmark
fn calibrate() -> f64 {
    let iters = 50_000_000u64;
    let c0 = thread_cpu();
    spin(iters);
    let us = (thread_cpu() - c0).as_secs_f64() * 1e6;
    iters as f64 / us
}

#[inline(never)]
// glue: benchmark
fn spin(iters: u64) {
    let mut x = 0u64;
    for i in 0..iters {
        x = black_box(x.wrapping_mul(31).wrapping_add(i));
    }
    black_box(x);
}

/// A future that yields once.
struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();
    // glue: benchmark
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

// ---------------------------------------------------------------- accounting

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Off,
    Instant,
    Window(Duration),
    PerPoll,
}

impl Mode {
    // glue: benchmark
    fn name(self) -> String {
        match self {
            Mode::Off => "off".into(),
            Mode::Instant => "instant".into(),
            Mode::Window(w) => format!("win{}ms", w.as_millis()),
            Mode::PerPoll => "perpoll".into(),
        }
    }
}

/// One worker thread's open window.
struct Win {
    /// The run this window belongs to (`GEN`); a window left over from an earlier run is
    /// discarded.
    generation: u64,
    last_cpu: Duration,
    last_read: Instant,
    /// Sampled polls since the last read: (task's CPU total, wall ns).
    polls: Vec<(Arc<AtomicU64>, u64)>,
    sampled_wall: u64,
    /// Wall ns of unsampled polls since the last read.
    other_wall: u64,
}

/// The current run.
static GEN: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static WIN: RefCell<Option<Win>> = const { RefCell::new(None) };
}

/// Close the thread's window: split the CPU since the last read over the window's polls.
// glue: benchmark
fn close_window(w: &mut Win, now: Instant) {
    let cpu = thread_cpu();
    let delta = cpu.saturating_sub(w.last_cpu).as_nanos() as u64;
    let total = w.sampled_wall + w.other_wall;
    if total > 0 {
        for (acc, wall) in w.polls.drain(..) {
            acc.fetch_add(
                (u128::from(delta) * u128::from(wall) / u128::from(total)) as u64,
                Ordering::Relaxed,
            );
        }
    }
    w.polls.clear();
    w.sampled_wall = 0;
    w.other_wall = 0;
    w.last_cpu = cpu;
    w.last_read = now;
}

/// The poll wrapper.
struct Accounted {
    inner: Pin<Box<dyn Future<Output = ()> + Send>>,
    mode: Mode,
    sampled: bool,
    /// Wall time of the task's polls (task-local, §3.6).
    wall: u64,
    /// CPU attributed to the task.
    cpu: Arc<AtomicU64>,
}

impl Future for Accounted {
    type Output = u64;
    // glue: benchmark
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u64> {
        let this = &mut *self;
        let r = match this.mode {
            Mode::Off => this.inner.as_mut().poll(cx),
            Mode::Instant => {
                let t0 = Instant::now();
                let r = this.inner.as_mut().poll(cx);
                if this.sampled {
                    this.wall += t0.elapsed().as_nanos() as u64;
                }
                r
            }
            Mode::Window(win) => {
                let t0 = Instant::now();
                let r = this.inner.as_mut().poll(cx);
                let t1 = Instant::now();
                let wall = (t1 - t0).as_nanos() as u64;
                let (sampled, acc) = (this.sampled, &this.cpu);
                if sampled {
                    this.wall += wall;
                }
                WIN.with(|w| {
                    let mut w = w.borrow_mut();
                    if !sampled {
                        if let Some(w) = w.as_mut()
                            && w.generation == GEN.load(Ordering::Relaxed)
                        {
                            w.other_wall += wall;
                        }
                        return;
                    }
                    let generation = GEN.load(Ordering::Relaxed);
                    if w.as_ref().is_some_and(|w| w.generation != generation) {
                        *w = None;
                    }
                    let w = w.get_or_insert_with(|| Win {
                        generation,
                        last_cpu: thread_cpu(),
                        last_read: t1,
                        polls: Vec::with_capacity(64),
                        sampled_wall: 0,
                        other_wall: 0,
                    });
                    match w.polls.last_mut() {
                        Some((a, n)) if Arc::ptr_eq(a, acc) => *n += wall,
                        _ => w.polls.push((acc.clone(), wall)),
                    }
                    w.sampled_wall += wall;
                    if t1 - w.last_read >= win {
                        close_window(w, t1);
                    }
                });
                r
            }
            Mode::PerPoll => {
                if this.sampled {
                    let c0 = thread_cpu();
                    let r = this.inner.as_mut().poll(cx);
                    this.cpu
                        .fetch_add((thread_cpu() - c0).as_nanos() as u64, Ordering::Relaxed);
                    r
                } else {
                    let t0 = Instant::now();
                    let r = this.inner.as_mut().poll(cx);
                    black_box(t0.elapsed());
                    r
                }
            }
        };
        r.map(|()| this.wall)
    }
}

/// Whether task `id` is sampled at `rate` (a fixed pseudo-random draw per task).
// glue: benchmark
fn sampled(id: u64, rate: f64) -> bool {
    let mut z = id.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z as f64 / u64::MAX as f64) < rate
}

struct RunResult {
    wall: Duration,
    cpu: Duration,
    /// CPU attributed to sampled light / heavy tasks, and their counts.
    light: (u64, usize),
    heavy: (u64, usize),
}

// glue: benchmark
fn run(
    rt: &tokio::runtime::Runtime,
    cores: usize,
    mode: Mode,
    rate: f64,
    polls: usize,
    light_iters: u64,
) -> RunResult {
    let tasks = cores * TASKS_PER_CORE;
    let accs: Vec<Arc<AtomicU64>> = (0..tasks).map(|_| Arc::new(AtomicU64::new(0))).collect();
    GEN.fetch_add(1, Ordering::Relaxed);
    let c0 = process_cpu();
    let t0 = Instant::now();
    rt.block_on(async {
        let mut hs = Vec::with_capacity(tasks);
        for (i, acc) in accs.iter().enumerate() {
            let iters = if i % 2 == 0 {
                light_iters
            } else {
                light_iters * 4
            };
            let inner = Box::pin(async move {
                for _ in 0..polls {
                    spin(iters);
                    YieldNow(false).await;
                }
            });
            hs.push(tokio::spawn(Accounted {
                inner,
                mode,
                sampled: sampled(i as u64, rate),
                wall: 0,
                cpu: acc.clone(),
            }));
        }
        for h in hs {
            black_box(h.await.unwrap());
        }
    });
    let wall = t0.elapsed();
    let cpu = process_cpu() - c0;
    let mut light = (0, 0);
    let mut heavy = (0, 0);
    for (i, acc) in accs.iter().enumerate() {
        if !sampled(i as u64, rate) {
            continue;
        }
        let v = acc.load(Ordering::Relaxed);
        let slot = if i % 2 == 0 { &mut light } else { &mut heavy };
        slot.0 += v;
        slot.1 += 1;
    }
    RunResult {
        wall,
        cpu,
        light,
        heavy,
    }
}

// glue: benchmark
fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

// glue: benchmark
fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// glue: benchmark
fn main() {
    // `cargo bench` passes `--bench`; nothing else to parse.
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let polls = env("CPU_ACC_POLLS", 2000) as usize;
    let reps = env("CPU_ACC_REPS", 5) as usize;
    let work_ns = env("CPU_ACC_WORK_NS", 1000);

    println!("# N9 §6.1 CPU accounting microbenchmark\n");
    println!(
        "platform: {} {}, {cores} cores; {} tasks x {polls} polls; light work {work_ns} ns/poll, heavy 4x; median of {reps} runs\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        cores * TASKS_PER_CORE,
    );

    println!("## 1. Clock read cost\n");
    println!("| clock | ns / call |\n|---|---|");
    let n = 2_000_000;
    println!(
        "| `Instant::now` | {:.1} |",
        ns_per_call(n, || {
            black_box(Instant::now());
        })
    );
    println!(
        "| `clock_gettime(CLOCK_MONOTONIC)` | {:.1} |",
        ns_per_call(n, || {
            black_box(monotonic());
        })
    );
    println!(
        "| `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` | {:.1} |",
        ns_per_call(n / 4, || {
            black_box(thread_cpu());
        })
    );

    let per_us = calibrate();
    let light_iters = (per_us * work_ns as f64 / 1000.0).max(1.0) as u64;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cores)
        .build()
        .unwrap();

    let modes = [
        Mode::Instant,
        Mode::Window(Duration::from_millis(1)),
        Mode::Window(Duration::from_millis(10)),
        Mode::PerPoll,
    ];
    let rates = [0.0, 0.01, 1.0];

    // Overhead: every configuration once per round, rounds repeated, so drift (frequency,
    // other load) hits all configurations alike; the median per configuration is reported.
    let configs: Vec<(Mode, f64)> = std::iter::once((Mode::Off, 0.0))
        .chain(
            modes
                .iter()
                .flat_map(|m| rates.iter().map(move |r| (*m, *r))),
        )
        .collect();
    let single = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    for (title, rt, workers) in [
        (
            "2a. Overhead, one worker thread (current_thread runtime)",
            &single,
            1,
        ),
        (
            "2b. Overhead, one worker per core (multi_thread runtime)",
            &rt,
            cores,
        ),
    ] {
        let rounds = reps * 3;
        let mut cpu: Vec<Vec<f64>> = vec![Vec::new(); configs.len()];
        run(rt, workers, Mode::Off, 0.0, polls / 4, light_iters);
        for _ in 0..rounds {
            for (i, (mode, rate)) in configs.iter().enumerate() {
                let r = run(rt, workers, *mode, *rate, polls, light_iters);
                cpu[i].push(r.cpu.as_nanos() as f64);
                if std::env::var_os("CPU_ACC_DEBUG").is_some() {
                    eprintln!(
                        "{workers} {} {rate} cpu={:?} wall={:?}",
                        mode.name(),
                        r.cpu,
                        r.wall
                    );
                }
            }
        }
        let total_polls = (workers * TASKS_PER_CORE * polls) as f64;
        let off = median(cpu[0].clone());
        println!("\n## {title}\n");
        let mut offs = cpu[0].clone();
        offs.sort_by(f64::total_cmp);
        let (lo, hi) = (offs[offs.len() / 4], offs[offs.len() * 3 / 4]);
        println!(
            "`off`: {:.0} ns CPU per poll (work + scheduling); median of {rounds} interleaved rounds. Noise: `off` itself varies by {:.1} ns / poll between its 25th and 75th percentile runs; differences smaller than that are not significant.\n",
            off / total_polls,
            (hi - lo) / total_polls
        );
        println!("| mode | sample rate | ns / poll above off | % of off |\n|---|---|---|---|");
        for (i, (mode, rate)) in configs.iter().enumerate().skip(1) {
            let c = median(cpu[i].clone());
            println!(
                "| {} | {}% | {:+.1} | {:+.2}% |",
                mode.name(),
                rate * 100.0,
                (c - off) / total_polls,
                (c - off) / off * 100.0
            );
        }
    }

    println!("\n## 3. Accuracy (100% sampled)\n");
    println!(
        "heavy/light: CPU attributed per heavy task over per light task (the work ratio is 4; `perpoll` measures the real ratio). attributed/process: CPU attributed to all tasks over process CPU.\n"
    );
    println!("| load | mode | heavy/light | attributed/process |\n|---|---|---|---|");
    for oversub in [false, true] {
        let stop = Arc::new(AtomicBool::new(false));
        let spinners: Vec<_> = if oversub {
            (0..cores)
                .map(|_| {
                    let stop = stop.clone();
                    std::thread::spawn(move || {
                        while !stop.load(Ordering::Relaxed) {
                            spin(10_000);
                        }
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        for mode in &modes[1..] {
            let mut ratios = Vec::new();
            let mut shares = Vec::new();
            for _ in 0..reps {
                let r = run(&rt, cores, *mode, 1.0, polls, light_iters);
                let light = r.light.0 as f64 / r.light.1 as f64;
                let heavy = r.heavy.0 as f64 / r.heavy.1 as f64;
                ratios.push(heavy / light);
                // Under oversubscription the process CPU includes the spinners: compare
                // against the polls' own share only when idle.
                shares.push((r.light.0 + r.heavy.0) as f64 / r.cpu.as_nanos() as f64);
                black_box(r.wall);
            }
            println!(
                "| {} | {} | {:.3} | {} |",
                if oversub { "oversubscribed" } else { "idle" },
                mode.name(),
                median(ratios),
                if oversub {
                    "n/a (spinners)".to_string()
                } else {
                    format!("{:.3}", median(shares))
                }
            );
        }
        stop.store(true, Ordering::Relaxed);
        for s in spinners {
            s.join().unwrap();
        }
    }
}
