//! The unit tests of Go's `vec_group_checker_test.go`: `TestVecGroupCheckerDATARACE`,
//! `TestVecGroupChecker4GroupCount` (with `genTestChunk4VecGroupChecker`),
//! `TestVecGroupChecker` and `TestIssue53867`.
//!
//! Differences from the Go tests:
//! - `TestVecGroupCheckerDATARACE` sets the checker's buffer hooks so that the evaluation
//!   writes into the input chunk's column, then overwrites that column and checks that the
//!   first / last datums kept their values. The hooks are not ported (M-POOL); the port
//!   overwrites the input column after the split and checks the same.
//! - Go's `TestVecGroupChecker` changes the collation of the column's shared `FieldType`
//!   between the splits; the Rust expression owns its type, so the test gives the checker a
//!   new column expression with the changed type instead.
//! - Go's `math/rand` values are replaced by a fixed xorshift sequence (the test only needs
//!   distinct values per group and one random NULL group).

// Go identifiers for the type constants (M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::mysql::{
    MaxIntWidth, TypeJSON, TypeLonglong, TypeNewDecimal, TypeVarString, TypeVarchar,
};
use tidb_chunk::chunk::{Chunk, new_chunk_with_capacity};
use tidb_expression::context::BuildContext;
use tidb_expression::expression::EvalState;
use tidb_expression::{Column, Expr, ExprRef};
use tidb_parser::types::FieldType;
use tidb_sessionctx::exprctx::EvalContextRef;
use tidb_sessionctx::exprstatic as es;
use tidb_types::field_type::new_field_type;
use tidb_types::json::parse_binary_json_from_string;
use tidb_types::mydecimal::new_dec_from_int;

use super::vec_group_checker::{VecGroupChecker, new_vec_group_checker};

fn eval_ctx() -> EvalContextRef {
    es::new_expr_context(vec![]).get_eval_ctx()
}

fn column(ft: &FieldType, index: isize) -> ExprRef {
    Expr::Column(Column {
        ret_type: Arc::new(ft.clone()),
        index,
        ..Column::default()
    })
    .into_ref()
}

fn split(checker: &mut VecGroupChecker, chk: &Chunk) -> bool {
    checker
        .split_into_groups(&EvalState::default(), chk)
        .expect("SplitIntoGroups")
}

/// A fixed stand-in for Go's `rand.Int63` / `rand.Intn`.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn int63(&mut self) -> i64 {
        (self.next() >> 1) as i64
    }
    fn intn(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[test]
fn vec_group_checker_datarace() {
    let ctx = eval_ctx();
    for m_type in [TypeVarString, TypeNewDecimal, TypeJSON] {
        let ft = new_field_type(m_type);
        let exprs = vec![column(&ft, 0)];
        let mut vgc = new_vec_group_checker(Arc::clone(&ctx), true, exprs);

        let mut chk = new_chunk_with_capacity(std::slice::from_ref(&ft), 1);
        match m_type {
            TypeVarString => chk.column_mut(0).append_string(b"abc"),
            TypeNewDecimal => chk.column_mut(0).append_my_decimal(&new_dec_from_int(123)),
            _ => chk
                .column_mut(0)
                .append_json(&parse_binary_json_from_string(br#"{"123":123}"#).unwrap()),
        }

        split(&mut vgc, &chk);

        let check = |vgc: &VecGroupChecker| match m_type {
            TypeVarString => {
                assert_eq!(vgc.first_row_datums[0].get_string(), b"abc");
                assert_eq!(vgc.last_row_datums[0].get_string(), b"abc");
            }
            TypeNewDecimal => {
                assert_eq!(
                    vgc.first_row_datums[0].get_mysql_decimal().to_string(),
                    b"123"
                );
                assert_eq!(
                    vgc.last_row_datums[0].get_mysql_decimal().to_string(),
                    b"123"
                );
            }
            _ => {
                assert_eq!(
                    vgc.first_row_datums[0].get_mysql_json().to_string(),
                    r#"{"123": 123}"#
                );
                assert_eq!(
                    vgc.last_row_datums[0].get_mysql_json().to_string(),
                    r#"{"123": 123}"#
                );
            }
        };
        check(&vgc);
        // Overwrite the input column: the datums are copies.
        chk.reset();
        match m_type {
            TypeVarString => chk.column_mut(0).append_string(b"edf"),
            TypeNewDecimal => chk.column_mut(0).append_my_decimal(&new_dec_from_int(456)),
            _ => chk
                .column_mut(0)
                .append_json(&parse_binary_json_from_string(br#"{"456":456}"#).unwrap()),
        }
        check(&vgc);
    }
}

// go-part: genTestChunk4VecGroupChecker
fn gen_test_chunk4_vec_group_checker(
    rng: &mut XorShift,
    chk_rows: &[usize],
    same_num: usize,
) -> (Vec<ExprRef>, Vec<Chunk>) {
    let fts = [new_field_type(TypeLonglong)];
    let mut inputs: Vec<Chunk> = chk_rows
        .iter()
        .map(|&n| new_chunk_with_capacity(&fts, n))
        .collect();
    let num_rows: usize = chk_rows.iter().sum();
    let num_groups = if num_rows.is_multiple_of(same_num) {
        num_rows / same_num
    } else {
        num_rows / same_num + 1
    };

    let mut null_pos = rng.intn(num_groups) as isize;
    let mut cnt = 0;
    let mut val = rng.int63();
    for (i, input) in inputs.iter_mut().enumerate() {
        let col = input.column_mut(0);
        col.resize_int64(chk_rows[i], false);
        for j in 0..chk_rows[i] {
            if cnt == same_num {
                val = rng.int63();
                cnt = 0;
                null_pos -= 1;
            }
            if null_pos == 0 {
                col.set_null(j, true);
            } else {
                col.int64s_mut()[j].set(val);
            }
            cnt += 1;
        }
    }

    let mut ft = new_field_type(TypeLonglong);
    ft.set_flen(MaxIntWidth);
    (vec![column(&ft, 0)], inputs)
}

#[test]
fn vec_group_checker4_group_count() {
    struct Case {
        chunk_rows: &'static [usize],
        expected_groups: usize,
        expected_flag: [bool; 2],
        same_num: usize,
    }
    let test_cases = [
        Case {
            chunk_rows: &[1024, 1],
            expected_groups: 1025,
            expected_flag: [false, false],
            same_num: 1,
        },
        Case {
            chunk_rows: &[1024, 1],
            expected_groups: 1,
            expected_flag: [false, true],
            same_num: 1025,
        },
        Case {
            chunk_rows: &[1, 1],
            expected_groups: 1,
            expected_flag: [false, true],
            same_num: 2,
        },
        Case {
            chunk_rows: &[1, 1],
            expected_groups: 2,
            expected_flag: [false, false],
            same_num: 1,
        },
        Case {
            chunk_rows: &[2, 2],
            expected_groups: 2,
            expected_flag: [false, false],
            same_num: 2,
        },
        Case {
            chunk_rows: &[2, 2],
            expected_groups: 1,
            expected_flag: [false, true],
            same_num: 4,
        },
    ];

    let ctx = eval_ctx();
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    for test_case in &test_cases {
        let (expr, input_chks) =
            gen_test_chunk4_vec_group_checker(&mut rng, test_case.chunk_rows, test_case.same_num);
        let mut group_checker = new_vec_group_checker(Arc::clone(&ctx), true, expr);
        let mut group_num = 0;
        for (i, input_chk) in input_chks.iter().enumerate() {
            let flag = split(&mut group_checker, input_chk);
            assert_eq!(test_case.expected_flag[i], flag);
            if flag {
                group_num += group_checker.group_count() - 1;
            } else {
                group_num += group_checker.group_count();
            }
        }
        assert_eq!(test_case.expected_groups, group_num);
    }
}

#[test]
fn vec_group_checker() {
    let mut tp = new_field_type(TypeVarchar);
    let ctx = eval_ctx();
    let mut group_checker = new_vec_group_checker(Arc::clone(&ctx), true, vec![column(&tp, 0)]);

    let mut chk = new_chunk_with_capacity(&[tp.clone()], 6);
    chk.reset();
    for s in ["aaa", "AAA", "😜", "😃", "À", "A"] {
        chk.column_mut(0).append_string(s.as_bytes());
    }

    tp.set_collate("bin");
    group_checker.group_by_items = vec![column(&tp, 0)];
    group_checker.reset();
    split(&mut group_checker, &chk);
    for i in 0..6 {
        let (b, e) = group_checker.get_next_group();
        assert_eq!(b, i);
        assert_eq!(e, i + 1);
    }
    assert!(group_checker.is_exhausted());

    for collate in ["utf8_general_ci", "utf8_unicode_ci"] {
        tp.set_collate(collate);
        group_checker.group_by_items = vec![column(&tp, 0)];
        group_checker.reset();
        split(&mut group_checker, &chk);
        for i in 0..3 {
            let (b, e) = group_checker.get_next_group();
            assert_eq!(b, i * 2, "{collate}");
            assert_eq!(e, i * 2 + 2, "{collate}");
        }
        assert!(group_checker.is_exhausted());
    }

    // test padding
    tp.set_collate("utf8_bin");
    tp.set_flen(6);
    group_checker.group_by_items = vec![column(&tp, 0)];
    chk.reset();
    for s in ["a", "a  ", "a    "] {
        chk.column_mut(0).append_string(s.as_bytes());
    }
    group_checker.reset();
    split(&mut group_checker, &chk);
    let (b, e) = group_checker.get_next_group();
    assert_eq!(b, 0);
    assert_eq!(e, 3);
    assert!(group_checker.is_exhausted());
}

#[test]
fn issue53867() {
    // Go passes a nil context; the Rust checker needs one (it is not used here).
    let mut checker = new_vec_group_checker(eval_ctx(), true, Vec::new());
    checker.group_offset = vec![0; 20];
    checker.next_group_id = 10;
    checker.group_count = 15;
    assert!(!checker.is_exhausted());
    checker.reset();
    assert!(checker.is_exhausted());
}
