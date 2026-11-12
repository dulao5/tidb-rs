//! N4 §5 gate: "one body, macro-generated vector driver" against Go's hand-written vector
//! versions, on the inputs of Go's `BenchmarkVectorizedBuiltin*Func` (1024 rows; the same
//! value distributions and NULL ratios; see bench_test.go `defaultGener`, `rangeInt64Gener`,
//! `randString`).
//!
//! The five bodies are prototypes written as N4 §4 prescribes (from Go's row versions); the
//! real ports live with their function families (D08.4/D08.5/D08.6/D08.8). Each case runs
//! the vector driver (`-Vec`, Go `vecEvalXxx` incl. evaluating the column children) and the
//! row driver (`-Row`, Go's `NonVecBuiltinFunc`: `evalXxx` for each of the 1024 rows).
//!
//! Go numbers: `go test -c` in pkg/expression, then
//! `expr.test -test.run '^$' -test.bench 'BenchmarkVectorizedBuiltinArithmeticFunc$' builtinArithmeticPlusIntSig`.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use tidb_base::Error;
use tidb_base::errors::RedactLogDisable;
use tidb_base::mysql;
use tidb_chunk::Chunk;
use tidb_chunk::chunk::new_chunk_with_capacity;
use tidb_chunk::column::new_empty_column;
use tidb_expression::eval::{Args, BuiltinFunc, RpnProgram, RpnState, SigDesc, SigView};
use tidb_expression::{Column, EvalContext, EvalState, Expr, ExprRef, ScalarFunction, eval_fn};
use tidb_sessionctx::exprstatic::new_eval_context;
use tidb_types::errors::ErrOverflow;
use tidb_types::etc::FieldType;
use tidb_types::field_type::new_field_type;

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const N: usize = 1024;

// ---------------------------------------------------------------- the five bodies

#[inline]
fn unsigned(f: &impl SigView, i: usize) -> bool {
    mysql::has_unsigned_flag(f.arg_type(i).get_flag())
}

fn overflow(ctx: &dyn EvalContext, f: &BuiltinFunc, tp: &'static str) -> Error {
    ErrOverflow.gen_with_stack_by_args(vec![
        tp.into(),
        format!(
            "({} + {})",
            String::from_utf8_lossy(&f.arg(0).string_with_ctx(ctx, RedactLogDisable)),
            String::from_utf8_lossy(&f.arg(1).string_with_ctx(ctx, RedactLogDisable))
        )
        .into(),
    ])
}

/// The unsigned flags of the two arguments: bit 0 = lhs, bit 1 = rhs.
#[inline]
fn flags2(f: &impl SigView) -> u8 {
    u8::from(unsigned(f, 0)) | (u8::from(unsigned(f, 1)) << 1)
}

// go: pkg/expression/builtin_arithmetic.go (*builtinArithmeticPlusIntSig).evalInt (prototype)
// The `checked` form: (value, overflow) for the flag combination FL; the error is built by
// plus_int_err for the first overflowing row.
#[eval_fn(Int, checked(nflags = 2, flags = flags2, err = plus_int_err))]
fn plus_int<const FL: u8>(a: i64, b: i64) -> (i64, bool) {
    let (is_lhs_unsigned, is_rhs_unsigned) = (FL & 1 != 0, FL & 2 != 0);
    match (is_lhs_unsigned, is_rhs_unsigned) {
        (true, true) => {
            let (v, o) = (a as u64).overflowing_add(b as u64);
            (v as i64, o)
        }
        // uint64(a) + b must stay in [0, 2^64): a carry and a negative b cancel out.
        (true, false) => {
            let (v, o) = (a as u64).overflowing_add(b as u64);
            (v as i64, o != (b < 0))
        }
        (false, true) => {
            let (v, o) = (b as u64).overflowing_add(a as u64);
            (v as i64, o != (a < 0))
        }
        (false, false) => a.overflowing_add(b),
    }
}

#[cold]
fn plus_int_err(ctx: &dyn EvalContext, f: &impl SigView, _a: i64, _b: i64) -> Error {
    let tp = if unsigned(f, 0) || unsigned(f, 1) {
        "BIGINT UNSIGNED"
    } else {
        "BIGINT"
    };
    overflow(ctx, f, tp)
}

// go: (*builtinLTIntSig).evalInt + CompareInt + resOfLT (prototype). CompareInt evaluates
// both arguments before looking at NULL: `eager`.
#[eval_fn(Int, eager, checked(nflags = 2, flags = flags2, err = no_err))]
fn lt_int<const FL: u8>(arg0: i64, arg1: i64) -> (i64, bool) {
    let v = tidb_types::compare::compare_int(arg0, FL & 1 != 0, arg1, FL & 2 != 0);
    (i64::from(v < 0), false)
}

#[cold]
fn no_err(_ctx: &dyn EvalContext, _f: &impl SigView, _a: i64, _b: i64) -> Error {
    unreachable!("LT has no error rows")
}

/// Bit 0: the argument is unsigned; bit 1: the result type is unsigned.
#[inline]
fn cast_flags(f: &impl SigView) -> u8 {
    u8::from(unsigned(f, 0)) | (u8::from(mysql::has_unsigned_flag(f.tp.get_flag())) << 1)
}

// go: (*builtinCastIntAsRealSig).evalReal (prototype; inUnion = false). No error rows.
#[eval_fn(Real, checked(nflags = 2, flags = cast_flags, err = no_err1))]
fn cast_int_as_real<const FL: u8>(val: i64) -> (f64, bool) {
    let (unsigned_args0, unsigned_ret) = (FL & 1 != 0, FL & 2 != 0);
    let res = if !unsigned_ret && !unsigned_args0 {
        val as f64
    } else {
        val as u64 as f64
    };
    (res, false)
}

#[cold]
fn no_err1(_ctx: &dyn EvalContext, _f: &impl SigView, _a: i64) -> Error {
    unreachable!("CAST(int AS REAL) has no error rows")
}

const MAX_ALLOWED_PACKET: u64 = 64 << 20;

// go: (*builtinConcatSig).evalString (prototype)
#[eval_fn(String, lazy)]
fn concat(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    args: &mut impl Args,
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    for i in 0..args.len() {
        let Some(d) = args.string(ctx, i)? else {
            return Ok(None);
        };
        if (out.len() + d.len()) as u64 > MAX_ALLOWED_PACKET {
            return Err(Error::plain("max_allowed_packet"));
        }
        out.extend_from_slice(&d);
    }
    Ok(Some(()))
}

// go: (*builtinLikeSig).evalInt (prototype; the pattern argument is not constant)
#[eval_fn(Int)]
fn like(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    val_str: &[u8],
    pattern_str: &[u8],
    escape: i64,
) -> Result<Option<i64>, Error> {
    // Go: a new pattern per row (row version), one per call (vector version).
    let mut pattern = f.scratch(|| f.collator().pattern());
    pattern.compile(pattern_str, escape as u8);
    Ok(Some(i64::from(pattern.do_match(val_str))))
}

static PLUS_INT_SIG: SigDesc = SigDesc {
    name: "builtinArithmeticPlusIntSig",
    pb_code: 0,
    vectorized: true,
    drivers: PLUS_INT,
    args_fallback: tidb_expression::eval::ArgsFallback::None,
};
static LT_INT_SIG: SigDesc = SigDesc {
    name: "builtinLTIntSig",
    pb_code: 0,
    vectorized: true,
    drivers: LT_INT,
    args_fallback: tidb_expression::eval::ArgsFallback::None,
};
static CAST_INT_AS_REAL_SIG: SigDesc = SigDesc {
    name: "builtinCastIntAsRealSig",
    pb_code: 0,
    vectorized: true,
    drivers: CAST_INT_AS_REAL,
    args_fallback: tidb_expression::eval::ArgsFallback::None,
};
static CONCAT_SIG: SigDesc = SigDesc {
    name: "builtinConcatSig",
    pb_code: 0,
    vectorized: true,
    drivers: CONCAT,
    args_fallback: tidb_expression::eval::ArgsFallback::None,
};
static LIKE_SIG: SigDesc = SigDesc {
    name: "builtinLikeSig",
    pb_code: 0,
    vectorized: true,
    drivers: LIKE,
    args_fallback: tidb_expression::eval::ArgsFallback::None,
};

// ---------------------------------------------------------------- inputs

/// Go's `math/rand`-like source for the value distributions (not the same sequence).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn f64(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn int63(&mut self) -> i64 {
        (self.next() >> 1) as i64
    }
    fn intn(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Clone, Copy)]
enum Gen {
    /// defaultGener(0.2, ETInt)
    DefaultInt,
    /// rangeInt64Gener(begin, end): no NULL
    Range(i64, i64),
    /// defaultGener(0.2, ETString): randString
    DefaultString,
}

fn fill(chk: &mut Chunk, col: usize, g: Gen, rng: &mut Rng) {
    for _ in 0..N {
        match g {
            Gen::DefaultInt => {
                if rng.f64() < 0.2 {
                    chk.append_null(col);
                } else if rng.f64() < 0.5 {
                    chk.append_int64(col, -rng.int63());
                } else {
                    chk.append_int64(col, rng.int63());
                }
            }
            Gen::Range(b, e) => {
                let v = (rng.intn((e as i128 - b as i128) as u64) as i128 + b as i128) as i64;
                chk.append_int64(col, v);
            }
            Gen::DefaultString => {
                if rng.f64() < 0.2 {
                    chk.append_null(col);
                    continue;
                }
                let n = 10 + rng.intn(10) as usize;
                let s: Vec<u8> = (0..n)
                    .map(|_| {
                        let x = rng.intn(62) as u8;
                        if x < 10 {
                            b'0' + x
                        } else if x - 10 < 26 {
                            b'a' + x - 10
                        } else {
                            b'A' + x - 36
                        }
                    })
                    .collect();
                chk.append_string(col, &s);
            }
        }
    }
}

fn longlong(unsigned: bool) -> FieldType {
    let mut ft = new_field_type(mysql::TypeLonglong);
    if unsigned {
        ft.set_flag(ft.get_flag() | mysql::UnsignedFlag);
    }
    ft
}

struct Case {
    name: String,
    expr: ExprRef,
    input: Chunk,
    ret_eval_int: bool,
}

fn case(
    name: &str,
    sig: &'static SigDesc,
    ret: FieldType,
    children: Vec<(FieldType, Gen)>,
) -> Case {
    let fts: Vec<FieldType> = children.iter().map(|c| c.0.clone()).collect();
    let mut input = new_chunk_with_capacity(&fts, N);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for (i, (_, g)) in children.iter().enumerate() {
        fill(&mut input, i, *g, &mut rng);
    }
    let args: Vec<ExprRef> = fts
        .iter()
        .enumerate()
        .map(|(i, ft)| {
            Arc::new(Expr::Column(Column {
                index: i as isize,
                ret_type: ft.clone(),
                ..Column::default()
            }))
        })
        .collect();
    let ret_eval_int = sig.drivers.eval_type() == Some(tidb_parser::types::ETInt);
    let function = BuiltinFunc::new(sig, args, ret.clone(), "utf8mb4", "utf8mb4_bin");
    let expr = Arc::new(Expr::ScalarFunction(ScalarFunction::new(
        name, ret, function,
    )));
    Case {
        name: name.into(),
        expr,
        input,
        ret_eval_int,
    }
}

fn cases() -> Vec<Case> {
    let s = new_field_type(mysql::TypeVarString);
    let int = longlong(false);
    let half = (i64::MIN / 2, i64::MAX / 2);
    let pos = (0, i64::MAX);
    vec![
        case(
            "PlusInt-SS",
            &PLUS_INT_SIG,
            int.clone(),
            vec![
                (longlong(false), Gen::Range(half.0, half.1)),
                (longlong(false), Gen::Range(half.0, half.1)),
            ],
        ),
        case(
            "PlusInt-UU",
            &PLUS_INT_SIG,
            int.clone(),
            vec![
                (longlong(true), Gen::Range(pos.0, pos.1)),
                (longlong(true), Gen::Range(pos.0, pos.1)),
            ],
        ),
        case(
            "PlusInt-SU",
            &PLUS_INT_SIG,
            int.clone(),
            vec![
                (longlong(false), Gen::Range(pos.0, pos.1)),
                (longlong(true), Gen::Range(pos.0, pos.1)),
            ],
        ),
        case(
            "PlusInt-US",
            &PLUS_INT_SIG,
            int.clone(),
            vec![
                (longlong(true), Gen::Range(pos.0, pos.1)),
                (longlong(false), Gen::Range(pos.0, pos.1)),
            ],
        ),
        case(
            "LTInt",
            &LT_INT_SIG,
            int.clone(),
            vec![
                (longlong(false), Gen::DefaultInt),
                (longlong(false), Gen::DefaultInt),
            ],
        ),
        case(
            "CastIntAsReal",
            &CAST_INT_AS_REAL_SIG,
            new_field_type(mysql::TypeDouble),
            vec![(longlong(false), Gen::DefaultInt)],
        ),
        case(
            "Concat",
            &CONCAT_SIG,
            s.clone(),
            vec![
                (s.clone(), Gen::DefaultString),
                (s.clone(), Gen::DefaultString),
                (s.clone(), Gen::DefaultString),
            ],
        ),
        case(
            "Like",
            &LIKE_SIG,
            int.clone(),
            vec![
                (s.clone(), Gen::DefaultString),
                (s.clone(), Gen::DefaultString),
                (longlong(false), Gen::Range(92, 93)),
            ],
        ),
    ]
}

fn bench(c: &mut Criterion) {
    let ctx = new_eval_context(vec![]);
    let ctx: &dyn EvalContext = &*ctx;
    let st = EvalState::new();
    for case in cases() {
        let prog = RpnProgram::compile(&case.expr);
        let mut rpn = RpnState::default();
        let mut result = new_empty_column(case.expr.as_scalar_function().unwrap().ret_type());
        // Errors would make the comparison meaningless: every case must evaluate cleanly.
        prog.eval(ctx, &st, &mut rpn, &case.input, &mut result)
            .expect("vector evaluation");
        c.bench_function(&format!("{}-Vec", case.name), |b| {
            b.iter(|| {
                prog.eval(ctx, &st, &mut rpn, black_box(&case.input), &mut result)
                    .unwrap()
            })
        });
        let f = &case.expr.as_scalar_function().unwrap().function;
        c.bench_function(&format!("{}-Row", case.name), |b| {
            b.iter(|| {
                for i in 0..case.input.num_rows() {
                    let row = case.input.get_row(i);
                    if case.ret_eval_int {
                        black_box(f.eval_int(ctx, &st, row).unwrap());
                    } else if f.sig.drivers.eval_type() == Some(tidb_parser::types::ETReal) {
                        black_box(f.eval_real(ctx, &st, row).unwrap());
                    } else {
                        black_box(f.eval_string(ctx, &st, row).unwrap());
                    }
                }
            })
        });
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
