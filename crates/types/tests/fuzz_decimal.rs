//! Random-input check without a Go oracle: no MyDecimal operation may hit a Rust integer
//! overflow panic. Go's fixed-width arithmetic wraps silently, so every place where Go can
//! wrap must be written `wrapping_*` (M-INT); an "attempt to ... with overflow" panic in an
//! overflow-checked build means a missed site. Other panics (index out of range, makeslice)
//! are Go's own panics and are expected for some inputs.
//!
//! Inputs are random decimal strings (`FromString`), integers and floats; results of every
//! op go back into the pool, so later ops see the objects arithmetic and rounding produce
//! (whole-word `digitsInt`, `-0`, truncated `digitsFrac`, dirty receivers).
//!
//! Short by default; longer runs:
//!   TIDB_TYPES_FUZZ_ITERS=2000000 cargo test -p tidb-types --test fuzz_decimal
//! `TIDB_TYPES_FUZZ_SEED` picks the seed (printed on failure).

use std::panic::{AssertUnwindSafe, catch_unwind};
use tidb_types::{
    MODE_CEILING, MODE_HALF_UP, MODE_TRUNCATE, MyDecimal, RoundMode, decimal_add, decimal_div,
    decimal_mod, decimal_mul, decimal_neg, decimal_peak, decimal_sub,
};

/// splitmix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn digits(rng: &mut Rng, n: u64, out: &mut String) {
    // Runs of 0 and 9 reach carries and word boundaries more often than uniform digits.
    let style = rng.below(4);
    for _ in 0..n {
        let c = match style {
            0 => b'0' + rng.below(10) as u8,
            1 => b"09"[rng.below(2) as usize],
            2 => b"000159"[rng.below(6) as usize],
            _ => b'9',
        };
        out.push(c as char);
    }
}

fn rand_dec_string(rng: &mut Rng) -> String {
    let mut s = String::new();
    if rng.chance(15) {
        s.push(' ');
    }
    match rng.below(10) {
        0..=3 => s.push('-'),
        4 => s.push('+'),
        _ => {}
    }
    let int_len = match rng.below(4) {
        0 => rng.below(3),
        1 => rng.below(20),
        2 => rng.below(70),
        _ => rng.below(100),
    };
    digits(rng, int_len, &mut s);
    if rng.chance(70) {
        s.push('.');
        let frac_len = match rng.below(3) {
            0 => rng.below(5),
            1 => rng.below(35),
            _ => rng.below(90),
        };
        digits(rng, frac_len, &mut s);
    }
    if rng.chance(15) {
        s.push(['e', 'E'][rng.below(2) as usize]);
        if rng.chance(50) {
            s.push('-');
        }
        let e = match rng.below(3) {
            0 => rng.below(10),
            1 => rng.below(100),
            _ => rng.next() >> rng.below(64),
        };
        s.push_str(&e.to_string());
    }
    if rng.chance(5) {
        s.push_str(["x", " ", "\t", "1x", "..", "１"][rng.below(6) as usize]);
    }
    s
}

fn rand_frac(rng: &mut Rng) -> isize {
    match rng.below(4) {
        0 => rng.range(-10, 10) as isize,
        1 => rng.range(-40, 40) as isize,
        2 => rng.range(-100, 100) as isize,
        _ => rng.range(-200, 200) as isize,
    }
}

fn rand_mode(rng: &mut Rng) -> RoundMode {
    [MODE_HALF_UP, MODE_TRUNCATE, MODE_CEILING][rng.below(3) as usize]
}

/// A new decimal from a random source; `recv` is the receiver (possibly dirty: FromString,
/// FromInt etc. do not reset the other fields, as in Go).
fn fresh(rng: &mut Rng, mut recv: MyDecimal) -> MyDecimal {
    match rng.below(10) {
        0 => {
            let v = if rng.chance(10) {
                [i64::MIN, i64::MIN + 1, i64::MAX, -1, 0][rng.below(5) as usize]
            } else {
                (rng.next() as i64) >> rng.below(64)
            };
            recv.from_int(v);
        }
        1 => {
            let v = if rng.chance(10) {
                [u64::MAX, u64::MAX - 1, 1 << 63, 0][rng.below(4) as usize]
            } else {
                rng.next() >> rng.below(64)
            };
            recv.from_uint(v);
        }
        2 => {
            let f = match rng.below(3) {
                0 => f64::from_bits(rng.next()),
                1 => (rng.next() >> 11) as f64 / (1u64 << rng.below(64)) as f64,
                _ => rng.range(-1_000_000, 1_000_000) as f64 * 0.1,
            };
            let _ = recv.from_float64(f);
        }
        _ => {
            let _ = recv.from_string(rand_dec_string(rng).as_bytes());
        }
    }
    recv
}

/// One random op over the pool; results are pushed back.
fn step(rng: &mut Rng, pool: &mut Vec<MyDecimal>) {
    let pick = |rng: &mut Rng, pool: &[MyDecimal]| pool[rng.below(pool.len() as u64) as usize];
    let a = pick(rng, pool);
    let b = pick(rng, pool);
    // The receiver of an op: fresh, or a dirty one from the pool.
    let mut to = if rng.chance(50) {
        MyDecimal::default()
    } else {
        pick(rng, pool)
    };
    let mut out: Option<MyDecimal> = None;
    match rng.below(22) {
        0 => out = Some(fresh(rng, to)),
        1 => {
            let _ = a.to_string();
            let _ = a.string();
        }
        2 => {
            let _ = a.to_int();
            let _ = a.to_uint();
        }
        3 => {
            let _ = a.to_float64();
        }
        4 => {
            let _ = a.round(&mut to, rand_frac(rng), rand_mode(rng));
            out = Some(to);
        }
        5 => {
            let mut d = a;
            let _ = d.round_in_place(rand_frac(rng), rand_mode(rng));
            out = Some(d);
        }
        6 => {
            let mut d = a;
            let _ = d.shift(rand_frac(rng));
            out = Some(d);
        }
        7 => {
            let _ = decimal_add(&a, &b, &mut to);
            out = Some(to);
        }
        8 => {
            let _ = decimal_sub(&a, &b, &mut to);
            out = Some(to);
        }
        9 => {
            let _ = decimal_mul(&a, &b, &mut to);
            out = Some(to);
        }
        10 => {
            let _ = decimal_div(&a, &b, &mut to, rng.range(0, 12) as isize);
            out = Some(to);
        }
        11 => {
            let _ = decimal_mod(&a, &b, &mut to);
            out = Some(to);
        }
        12 => out = Some(decimal_neg(&a)),
        13 => {
            let _ = a.compare(&b);
        }
        14 | 15 => {
            let (prec, frac) = if rng.chance(70) {
                a.precision_and_frac()
            } else {
                (rng.range(-5, 70) as isize, rng.range(-5, 40) as isize)
            };
            let (mut bin, _) = a.to_bin(prec, frac);
            if !bin.is_empty() && rng.chance(30) {
                let i = rng.below(bin.len() as u64) as usize;
                bin[i] = rng.next() as u8;
            }
            let _ = decimal_peak(&bin);
            let (p2, f2) = if rng.chance(80) {
                (prec, frac)
            } else {
                (rng.range(-5, 70) as isize, rng.range(-5, 40) as isize)
            };
            let _ = to.from_bin(&bin, p2, f2);
            out = Some(to);
        }
        16 => {
            let _ = a.to_hash_key();
        }
        17 => {
            let _ = a.precision_and_frac();
            let _ = a.is_zero();
        }
        18 => {
            if let Ok(j) = a.marshal_json() {
                let _ = to.unmarshal_json(&j);
                out = Some(to);
            }
        }
        19 => {
            if let Ok(d) = MyDecimal::from_wire(&a.as_wire()) {
                out = Some(d);
            }
        }
        _ => out = Some(fresh(rng, MyDecimal::default())),
    }
    if let Some(d) = out {
        if pool.len() < 4096 {
            pool.push(d);
        } else {
            let i = rng.below(pool.len() as u64) as usize;
            pool[i] = d;
        }
    }
}

#[test]
fn fuzz_no_integer_overflow() {
    let iters = env_u64("TIDB_TYPES_FUZZ_ITERS", 30_000);
    let seed = env_u64("TIDB_TYPES_FUZZ_SEED", 0x2026_1106);

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let mut rng = Rng(seed);
    let mut pool: Vec<MyDecimal> = (0..64)
        .map(|_| fresh(&mut rng, MyDecimal::default()))
        .collect();
    // Go-style panics by message (index/slice bounds, makeslice etc.).
    let mut go_panics: std::collections::BTreeMap<String, u64> = Default::default();
    let mut overflow: Option<(u64, String)> = None;
    for i in 0..iters {
        // Each step gets its own generator state so a failure can be replayed alone.
        let mut step_rng = Rng(rng.next());
        // A panicking step has not touched the pool (results are pushed last).
        let r = catch_unwind(AssertUnwindSafe(|| step(&mut step_rng, &mut pool)));
        if let Err(p) = r {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            if msg.contains("attempt to") && msg.contains("overflow") {
                overflow = Some((i, msg));
                break;
            }
            let kind: String = msg
                .split(':')
                .next()
                .unwrap_or_default()
                .chars()
                .take(60)
                .collect();
            *go_panics.entry(kind).or_default() += 1;
        }
    }
    std::panic::set_hook(hook);
    eprintln!("fuzz: seed {seed:#x}, {iters} steps; Go-style panics: {go_panics:?}");
    if let Some((i, msg)) = overflow {
        panic!("integer overflow at step {i} (seed {seed:#x}): {msg}");
    }
}
