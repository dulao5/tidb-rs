//! go-file: pkg/types/mydecimal.go
//!
//! Faithful port of Go TiDB v8.5.8 `MyDecimal` (block D01.2). Functions are in Go order.
//!
//! Carrier mappings used throughout (see specs/D01.2-mydecimal/plan.md and pilot-notes.md):
//! - Go `int` is `isize` (both are the pointer-sized signed integer). Slice and array
//!   indexes stay `isize` and are cast with `as usize` at the index: a negative Go index
//!   panics, and so does the huge `usize` it turns into.
//! - Go fixed-width arithmetic that can wrap uses `wrapping_*` and `as` casts (Rust debug
//!   builds panic on overflow); every such site is listed in pilot-notes.md.
//! - Go `error` values are `Result<(), Error>` (`Ok(())` is `nil`); a function returning
//!   `(T, error)` returns `(T, Result<(), Error>)` when T is meaningful with the error, and
//!   `Result<T, Error>` when every error return carries T's zero value.
//! - Go runtime panics (index out of range, makeslice) stay panics.

// Style lints that would rewrite Go's expressions and control flow (`x+1 <= n` → `x < n`,
// index loops → iterators, late initialisation in branches, struct-update syntax). A
// faithful port keeps Go's form so it can be compared side by side; correctness lints stay.
// Must be a subset of `[workspace.metadata.porting] style-lint-allow` (checked by converge).
#![allow(
    clippy::int_plus_one,
    clippy::field_reassign_with_default,
    clippy::needless_late_init,
    clippy::manual_range_contains,
    clippy::manual_slice_fill,
    clippy::needless_range_loop
)]

use crate::errors::{
    ERR_BAD_NUMBER, ERR_DIV_BY_ZERO, ERR_OVERFLOW, ERR_TRUNCATED, ERR_TRUNCATED_WRONG_VAL, Error,
    terror_log,
};
use crate::gostd;
use crate::helper::{is_digit, is_space, str_to_int};

/// `mysql.MaxDecimalScale` (pkg/parser/mysql/const.go).
// glue: stub blocked-by D00.2 (pkg/parser/mysql constants)
pub(crate) mod mysql {
    pub const MAX_DECIMAL_SCALE: i8 = 30;
}

/// RoundMode is the type for round mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoundMode(pub i32);

// constant values.
const TEN0: i32 = 1;
const TEN1: i32 = 10;
const TEN2: i32 = 100;
const TEN3: i32 = 1000;
const TEN4: i32 = 10000;
const TEN5: i32 = 100000;
const TEN6: i32 = 1000000;
const TEN7: i32 = 10000000;
const TEN8: i32 = 100000000;
const TEN9: i32 = 1000000000;

const MAX_WORD_BUF_LEN: usize = 9; // A MyDecimal holds 9 words.
const DIGITS_PER_WORD: isize = 9; // A word holds 9 digits.
const WORD_SIZE: isize = 4; // A word is 4 bytes int32.
const DIG_MASK: i32 = TEN8;
const WORD_BASE: i32 = TEN9;
const WORD_MAX: i32 = WORD_BASE - 1;
const NOT_FIXED_DEC: i8 = 31;

/// Round up to the next integer if positive or down to the next integer if negative.
pub const MODE_HALF_UP: RoundMode = RoundMode(5);
/// Truncate just truncates the decimal.
pub const MODE_TRUNCATE: RoundMode = RoundMode(10);
/// Ceiling is not supported now.
pub const MODE_CEILING: RoundMode = RoundMode(0);

const POW10OFF: isize = 81;

/// Go `var wordBufLen = 9`; Go tests change it, nothing else does.
const WORD_BUF_LEN: isize = 9;
#[rustfmt::skip]
const MOD9: [i8; 128] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8,
    0, 1, 2, 3, 4, 5, 6, 7, 8,
    0, 1, 2, 3, 4, 5, 6, 7, 8,
    0, 1, 2, 3, 4, 5, 6, 7, 8,
    0, 1, 2, 3, 4, 5, 6, 7, 8,
    0, 1, 2, 3, 4, 5, 6, 7, 8,
    0, 1, 2, 3, 4, 5, 6, 7, 8,
    0, 1, 2, 3, 4, 5, 6, 7, 8,
    0, 1, 2, 3, 4, 5, 6, 7, 8,
    0, 1, 2, 3, 4, 5, 6, 7, 8,
    0, 1, 2, 3, 4, 5, 6, 7, 8,
    0, 1, 2, 3, 4, 5, 6, 7, 8,
    0, 1, 2, 3, 4, 5, 6, 7, 8,
    0, 1, 2, 3, 4, 5, 6, 7, 8,
    0, 1,
];
#[rustfmt::skip]
const DIV9: [isize; 128] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 1, 1, 1, 1, 1,
    2, 2, 2, 2, 2, 2, 2, 2, 2,
    3, 3, 3, 3, 3, 3, 3, 3, 3,
    4, 4, 4, 4, 4, 4, 4, 4, 4,
    5, 5, 5, 5, 5, 5, 5, 5, 5,
    6, 6, 6, 6, 6, 6, 6, 6, 6,
    7, 7, 7, 7, 7, 7, 7, 7, 7,
    8, 8, 8, 8, 8, 8, 8, 8, 8,
    9, 9, 9, 9, 9, 9, 9, 9, 9,
    10, 10, 10, 10, 10, 10, 10, 10, 10,
    11, 11, 11, 11, 11, 11, 11, 11, 11,
    12, 12, 12, 12, 12, 12, 12, 12, 12,
    13, 13, 13, 13, 13, 13, 13, 13, 13,
    14, 14,
];
const POWERS10: [i32; 10] = [TEN0, TEN1, TEN2, TEN3, TEN4, TEN5, TEN6, TEN7, TEN8, TEN9];
const DIG2BYTES: [isize; 10] = [0, 1, 1, 2, 2, 3, 3, 4, 4, 4];
const FRAC_MAX: [i32; 8] = [
    900000000, 990000000, 999000000, 999900000, 999990000, 999999000, 999999900, 999999990,
];
const ZERO_MY_DECIMAL: MyDecimal = MyDecimal {
    digits_int: 0,
    digits_frac: 0,
    result_frac: 0,
    negative: false,
    word_buf: [0; MAX_WORD_BUF_LEN],
};
#[rustfmt::skip]
const POW10OFF81: [f64; 163] = [1e-81, 1e-80, 1e-79, 1e-78, 1e-77, 1e-76, 1e-75, 1e-74, 1e-73, 1e-72, 1e-71, 1e-70, 1e-69, 1e-68, 1e-67, 1e-66, 1e-65, 1e-64, 1e-63, 1e-62, 1e-61, 1e-60, 1e-59, 1e-58, 1e-57, 1e-56, 1e-55, 1e-54, 1e-53, 1e-52, 1e-51, 1e-50, 1e-49, 1e-48, 1e-47, 1e-46, 1e-45, 1e-44, 1e-43, 1e-42, 1e-41, 1e-40, 1e-39, 1e-38, 1e-37, 1e-36, 1e-35, 1e-34, 1e-33, 1e-32, 1e-31, 1e-30, 1e-29, 1e-28, 1e-27, 1e-26, 1e-25, 1e-24, 1e-23, 1e-22, 1e-21, 1e-20, 1e-19, 1e-18, 1e-17, 1e-16, 1e-15, 1e-14, 1e-13, 1e-12, 1e-11, 1e-10, 1e-9, 1e-8, 1e-7, 1e-6, 1e-5, 1e-4, 1e-3, 1e-2, 1e-1, 1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22, 1e23, 1e24, 1e25, 1e26, 1e27, 1e28, 1e29, 1e30, 1e31, 1e32, 1e33, 1e34, 1e35, 1e36, 1e37, 1e38, 1e39, 1e40, 1e41, 1e42, 1e43, 1e44, 1e45, 1e46, 1e47, 1e48, 1e49, 1e50, 1e51, 1e52, 1e53, 1e54, 1e55, 1e56, 1e57, 1e58, 1e59, 1e60, 1e61, 1e62, 1e63, 1e64, 1e65, 1e66, 1e67, 1e68, 1e69, 1e70, 1e71, 1e72, 1e73, 1e74, 1e75, 1e76, 1e77, 1e78, 1e79, 1e80, 1e81];

/// `f + x * y` the way Go gc compiles the word sums `f += float64(x) * pow10off81[...]` of
/// `ToFloat64` (mydecimal.go:1118 and :1125) on the same target (mapping rule `M-FMA`;
/// tracker `data/go_fma_sites.tsv`, from the `-gcflags=-S` assembly of go1.25.12):
///
/// | Go target | both sites | Rust target |
/// |---|---|---|
/// | amd64 GOAMD64=v1 (default, baseline) | not fused | `x86_64`: separate multiply and add |
/// | amd64 GOAMD64=v3 (informational) | not fused | (same) |
/// | arm64 | `FMADDD` | `aarch64`: `f64::mul_add` (one `fmadd` instruction) |
/// | ppc64le, riscv64, loong64 | fused | `f64::mul_add`; no vector set, untested |
/// | s390x | fused | none: big-endian, excluded by the wire format assertion |
///
/// Rust never contracts `x * y + z` by itself, so the non-fused form is exact Go v1 behaviour.
// go-part: MyDecimal.ToFloat64 L1 [M-FMA]
#[inline(always)]
fn to_float64_madd(f: f64, x: f64, y: f64) -> f64 {
    #[cfg(any(
        target_arch = "aarch64",
        all(target_arch = "powerpc64", target_endian = "little"),
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    {
        x.mul_add(y, f)
    }
    #[cfg(not(any(
        target_arch = "aarch64",
        all(target_arch = "powerpc64", target_endian = "little"),
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )))]
    {
        f + x * y
    }
}

/// get the zero of MyDecimal with the specified result fraction digits
// go: zeroMyDecimalWithFrac L1
fn zero_my_decimal_with_frac(frac: i8) -> MyDecimal {
    let mut zero = MyDecimal::default();
    zero.digits_frac = frac;
    zero.result_frac = frac;
    zero
}

/// add adds a and b and carry, returns the sum and new carry.
// go: add L1 [M-INT]
fn add(a: i32, b: i32, carry: i32) -> (i32, i32) {
    let mut sum = a.wrapping_add(b).wrapping_add(carry);
    let new_carry;
    if sum >= WORD_BASE {
        new_carry = 1;
        sum -= WORD_BASE;
    } else {
        new_carry = 0;
    }
    (sum, new_carry)
}

/// add2 adds a and b and carry, returns the sum and new carry.
/// It is only used in DecimalMul.
// go: add2 L1 [M-INT]
fn add2(a: i32, b: i32, mut carry: i32) -> (i32, i32) {
    let mut sum = i64::from(a) + i64::from(b) + i64::from(carry);
    if sum >= i64::from(WORD_BASE) {
        carry = 1;
        sum -= i64::from(WORD_BASE);
    } else {
        carry = 0;
    }

    if sum >= i64::from(WORD_BASE) {
        sum -= i64::from(WORD_BASE);
        carry += 1;
    }
    (sum as i32, carry)
}

/// sub subtracts b and carry from a, returns the diff and new carry.
// go: sub L1 [M-INT]
fn sub(a: i32, b: i32, carry: i32) -> (i32, i32) {
    let mut diff = a.wrapping_sub(b).wrapping_sub(carry);
    let new_carry;
    if diff < 0 {
        new_carry = 1;
        diff = diff.wrapping_add(WORD_BASE);
    } else {
        new_carry = 0;
    }
    (diff, new_carry)
}

/// sub2 subtracts b and carry from a, returns the diff and new carry.
/// the new carry may be 2.
// go: sub2 L1 [M-INT]
fn sub2(a: i32, b: i32, carry: i32) -> (i32, i32) {
    let mut diff = a.wrapping_sub(b).wrapping_sub(carry);
    let mut new_carray;
    if diff < 0 {
        new_carray = 1;
        diff = diff.wrapping_add(WORD_BASE);
    } else {
        new_carray = 0;
    }
    if diff < 0 {
        diff = diff.wrapping_add(WORD_BASE);
        new_carray += 1;
    }
    (diff, new_carray)
}

/// fixWordCntError limits word count in wordBufLen, and returns overflow or truncate error.
// go: fixWordCntError L1 [M-ERRVAL]
fn fix_word_cnt_error(words_int: isize, words_frac: isize) -> (isize, isize, Result<(), Error>) {
    if words_int + words_frac > WORD_BUF_LEN {
        if words_int > WORD_BUF_LEN {
            return (WORD_BUF_LEN, 0, Err(ERR_OVERFLOW));
        }
        return (words_int, WORD_BUF_LEN - words_int, Err(ERR_TRUNCATED));
    }
    (words_int, words_frac, Ok(()))
}

/// countLeadingZeroes returns the number of leading zeroes that can be removed from fraction.
///
/// @param   i    start index
/// @param   word value to compare against list of powers of 10
// go: countLeadingZeroes L1
fn count_leading_zeroes(mut i: isize, word: i32) -> isize {
    let mut leading = 0;
    while word < POWERS10[i as usize] {
        i -= 1;
        leading += 1;
    }
    leading
}

/// countTrailingZeros returns the number of trailing zeroes that can be removed from fraction.
///
/// @param   i    start index
/// @param   word  value to compare against list of powers of 10
// go: countTrailingZeroes L1
fn count_trailing_zeroes(mut i: isize, word: i32) -> isize {
    let mut trailing = 0;
    while word % POWERS10[i as usize] == 0 {
        i += 1;
        trailing += 1;
    }
    trailing
}

// go: digitsToWords L1
fn digits_to_words(digits: isize) -> isize {
    if digits + DIGITS_PER_WORD - 1 >= 0 && digits + DIGITS_PER_WORD - 1 < 128 {
        return DIV9[(digits + DIGITS_PER_WORD - 1) as usize];
    }
    (digits + DIGITS_PER_WORD - 1) / DIGITS_PER_WORD
}

/// MyDecimalStructSize is the struct size of MyDecimal.
pub const MY_DECIMAL_STRUCT_SIZE: usize = 40;

/// MyDecimal represents a decimal value.
///
/// The layout is Go's (`#[repr(C)]`, same field order, 40 bytes): chunk's wire format is the
/// in-memory layout of this struct ([`MyDecimal::from_wire`], [`MyDecimal::as_wire`]).
///
/// `==` is the derived field-by-field equality, like Go's struct `==` (and `TestReset`'s
/// deep equality): it compares the representation, not the value. `1.0 == 1.00` is false
/// (different `digitsFrac`), `-0` and `0` differ, and so do equal values with different
/// leftover words. For SQL value comparison use [`MyDecimal::compare`], which Go uses too.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MyDecimal {
    digits_int: i8, // the number of *decimal* digits before the point.

    digits_frac: i8, // the number of decimal digits after the point.

    result_frac: i8, // result fraction digits.

    negative: bool,

    ///  wordBuf is an array of int32 words.
    /// A word is an int32 value can hold 9 digits.(0 <= word < wordBase)
    word_buf: [i32; MAX_WORD_BUF_LEN],
}

const _: () = assert!(std::mem::size_of::<MyDecimal>() == MY_DECIMAL_STRUCT_SIZE);
const _: () = assert!(std::mem::align_of::<MyDecimal>() == 4);
const _: () = assert!(std::mem::offset_of!(MyDecimal, negative) == 3);
const _: () = assert!(std::mem::offset_of!(MyDecimal, word_buf) == 4);
// The wire format is Go's in-memory image, i.e. native byte order; from_wire/as_wire write
// the words little-endian. Every platform TiDB is released for (amd64, arm64) is
// little-endian; a big-endian build would disagree with Go's chunks on the same host.
const _: () = assert!(
    cfg!(target_endian = "little"),
    "MyDecimal wire format assumes a little-endian target"
);

impl MyDecimal {
    /// The decimal stored in chunk wire format: the 40-byte in-memory image of Go's
    /// `MyDecimal` (`digitsInt`, `digitsFrac`, `resultFrac` as int8, `negative` as one byte,
    /// then `wordBuf` as nine little-endian int32; N5a §3.3). Go writes native byte order;
    /// this assumes a little-endian target (asserted at compile time).
    ///
    /// Go reinterprets the bytes with `*(*MyDecimal)(unsafe.Pointer(&b[0]))` and checks
    /// nothing. A Rust `bool` must be 0 or 1, so byte 3 is read as `byte != 0`: gc tests a
    /// `bool` loaded from memory as a whole byte (`if d.negative`), so a corrupted image
    /// with e.g. `0x95` prints as negative in Go too (D01.7 review; replayed over random
    /// images). The other fields are not checked, as in Go: invalid values behave (or
    /// panic) later exactly as they do in Go. Never fails; the `Result` is kept for callers.
    // glue: Go's unsafe.Pointer cast of a chunk column element (util/chunk/column.go), N5a
    pub fn from_wire(b: &[u8; MY_DECIMAL_STRUCT_SIZE]) -> Result<MyDecimal, Error> {
        let negative = b[3] != 0;
        let mut word_buf = [0i32; MAX_WORD_BUF_LEN];
        for (i, w) in word_buf.iter_mut().enumerate() {
            let at = 4 + 4 * i;
            *w = i32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        }
        Ok(MyDecimal {
            digits_int: b[0] as i8,
            digits_frac: b[1] as i8,
            result_frac: b[2] as i8,
            negative,
            word_buf,
        })
    }

    /// The chunk wire format of the decimal (see [`MyDecimal::from_wire`]).
    // glue: Go's unsafe.Pointer cast of a chunk column element (util/chunk/column.go), N5a
    pub fn as_wire(&self) -> [u8; MY_DECIMAL_STRUCT_SIZE] {
        let mut b = [0u8; MY_DECIMAL_STRUCT_SIZE];
        b[0] = self.digits_int as u8;
        b[1] = self.digits_frac as u8;
        b[2] = self.result_frac as u8;
        b[3] = u8::from(self.negative);
        for (i, w) in self.word_buf.iter().enumerate() {
            b[4 + 4 * i..8 + 4 * i].copy_from_slice(&w.to_le_bytes());
        }
        b
    }
}

impl MyDecimal {
    /// IsNegative returns whether a decimal is negative.
    // go: MyDecimal.IsNegative L1
    pub fn is_negative(&self) -> bool {
        self.negative
    }

    /// GetDigitsFrac returns the digitsFrac.
    // go: MyDecimal.GetDigitsFrac L1
    pub fn get_digits_frac(&self) -> i8 {
        self.digits_frac
    }

    /// GetDigitsInt returns the digitsInt.
    // go: MyDecimal.GetDigitsInt L1
    pub fn get_digits_int(&self) -> i8 {
        self.digits_int
    }

    /// String returns the decimal string representation rounded to resultFrac.
    // go: MyDecimal.String L1
    pub fn string(&self) -> String {
        let mut tmp = *self;
        let err = tmp.round_in_place(tmp.result_frac as isize, MODE_HALF_UP);
        terror_log(err);
        // Go's string([]byte) keeps any bytes; ToString only writes ASCII for valid states.
        match String::from_utf8(tmp.to_string()) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        }
    }

    // go: MyDecimal.stringSize L1 [M-INT]
    fn string_size(&self) -> isize {
        // sign, zero integer and dot.
        // Go: int(d.digitsInt + d.digitsFrac + 3) adds in int8 and wraps.
        self.digits_int
            .wrapping_add(self.digits_frac)
            .wrapping_add(3) as isize
    }

    // go: MyDecimal.removeLeadingZeros L1
    fn remove_leading_zeros(&self) -> (isize, isize) {
        let mut word_idx: isize = 0;
        let mut digits_int = self.digits_int as isize;
        let mut i = ((digits_int - 1) % DIGITS_PER_WORD) + 1;
        while digits_int > 0 && self.word_buf[word_idx as usize] == 0 {
            digits_int -= i;
            i = DIGITS_PER_WORD;
            word_idx += 1;
        }
        if digits_int > 0 {
            digits_int -= count_leading_zeroes(
                (digits_int - 1) % DIGITS_PER_WORD,
                self.word_buf[word_idx as usize],
            );
        } else {
            digits_int = 0;
        }
        (word_idx, digits_int)
    }

    // go: MyDecimal.removeTrailingZeros L1
    fn remove_trailing_zeros(&self) -> (isize, isize) {
        let mut digits_frac = self.digits_frac as isize;
        let mut i = ((digits_frac - 1) % DIGITS_PER_WORD) + 1;
        let mut last_word_idx =
            digits_to_words(self.digits_int as isize) + digits_to_words(self.digits_frac as isize);
        while digits_frac > 0 && self.word_buf[(last_word_idx - 1) as usize] == 0 {
            digits_frac -= i;
            i = DIGITS_PER_WORD;
            last_word_idx -= 1;
        }
        if digits_frac > 0 {
            digits_frac -= count_trailing_zeroes(
                9 - ((digits_frac - 1) % DIGITS_PER_WORD),
                self.word_buf[(last_word_idx - 1) as usize],
            );
        } else {
            digits_frac = 0;
        }
        (last_word_idx, digits_frac)
    }

    /// ToString converts decimal to its printable string representation without rounding.
    ///
    /// ```text
    ///  RETURN VALUE
    ///
    ///      str       - result string
    ///      errCode   - eDecOK/eDecTruncate/eDecOverflow
    /// ```
    // go: MyDecimal.ToString L1 [M-INT, M-NAME]
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> Vec<u8> {
        // Go make([]byte, n) panics for a negative n; so does the huge usize here.
        let mut str = vec![0u8; self.string_size() as usize];
        let mut digits_frac = self.digits_frac as isize;
        let (mut word_start_idx, mut digits_int) = self.remove_leading_zeros();
        if digits_int + digits_frac == 0 {
            digits_int = 1;
            word_start_idx = 0;
        }

        let mut digits_int_len = digits_int;
        if digits_int_len == 0 {
            digits_int_len = 1;
        }
        let digits_frac_len = digits_frac;
        let mut length = digits_int_len + digits_frac_len;
        if self.negative {
            length += 1;
        }
        if digits_frac > 0 {
            length += 1;
        }
        // Go: str = str[:length] (panics if length exceeds the capacity).
        let _ = &str[..length as usize];
        str.truncate(length as usize);
        let mut str_idx: isize = 0;
        if self.negative {
            str[str_idx as usize] = b'-';
            str_idx += 1;
        }
        let mut fill;
        if digits_frac > 0 {
            let mut frac_idx = str_idx + digits_int_len;
            fill = digits_frac_len - digits_frac;
            let mut word_idx = word_start_idx + digits_to_words(digits_int);
            str[frac_idx as usize] = b'.';
            frac_idx += 1;
            while digits_frac > 0 {
                let mut x = self.word_buf[word_idx as usize];
                word_idx += 1;
                let mut i = digits_frac.min(DIGITS_PER_WORD);
                while i > 0 {
                    let y = x / DIG_MASK;
                    str[frac_idx as usize] = (y as u8).wrapping_add(b'0');
                    frac_idx += 1;
                    x = x.wrapping_sub(y.wrapping_mul(DIG_MASK));
                    x = x.wrapping_mul(10);
                    i -= 1;
                }
                digits_frac -= DIGITS_PER_WORD;
            }
            while fill > 0 {
                str[frac_idx as usize] = b'0';
                frac_idx += 1;
                fill -= 1;
            }
        }
        fill = digits_int_len - digits_int;
        if digits_int == 0 {
            fill -= 1; /* symbol 0 before digital point */
        }
        while fill > 0 {
            str[str_idx as usize] = b'0';
            str_idx += 1;
            fill -= 1;
        }
        if digits_int > 0 {
            str_idx += digits_int;
            let mut word_idx = word_start_idx + digits_to_words(digits_int);
            while digits_int > 0 {
                word_idx -= 1;
                let mut x = self.word_buf[word_idx as usize];
                let mut i = digits_int.min(DIGITS_PER_WORD);
                while i > 0 {
                    let y = x / 10;
                    str_idx -= 1;
                    str[str_idx as usize] = b'0'.wrapping_add(x.wrapping_sub(y * 10) as u8);
                    x = y;
                    i -= 1;
                }
                digits_int -= DIGITS_PER_WORD;
            }
        } else {
            str[str_idx as usize] = b'0';
        }
        str
    }

    /// FromString parses decimal from string.
    ///
    /// Like Go, this does not reset the receiver first. `negative` is set for a leading
    /// `-` and is cleared only when all nine words end up zero, so on a receiver that held
    /// a negative value `"2"` parses as -2; words past the parsed digits keep their old
    /// values (and count in that all-zero test). Error paths that Go answers with
    /// `*d = zeroMyDecimal` do reset everything. Parse into `MyDecimal::default()` (Go
    /// callers use `new(MyDecimal)`) unless the old state is wanted.
    // go: MyDecimal.FromString L1 [M-INT, M-ERRVAL]
    pub fn from_string(&mut self, str: &[u8]) -> Result<(), Error> {
        let mut str = str;
        for i in 0..str.len() {
            if !is_space(str[i]) {
                str = &str[i..];
                break;
            }
        }
        if str.is_empty() {
            *self = ZERO_MY_DECIMAL;
            return Err(ERR_TRUNCATED_WRONG_VAL.gen_with_stack_by_args(&[b"DECIMAL", str]));
        }
        match str[0] {
            b'-' => {
                self.negative = true;
                // fallthrough
                str = &str[1..];
            }
            b'+' => {
                str = &str[1..];
            }
            _ => {}
        }
        let mut str_idx: isize = 0;
        while (str_idx as usize) < str.len() && is_digit(str[str_idx as usize]) {
            str_idx += 1;
        }
        let mut digits_int = str_idx;
        let mut digits_frac;
        let end_idx;
        if (str_idx as usize) < str.len() && str[str_idx as usize] == b'.' {
            let mut e = str_idx + 1;
            while (e as usize) < str.len() && is_digit(str[e as usize]) {
                e += 1;
            }
            end_idx = e;
            digits_frac = end_idx - str_idx - 1;
        } else {
            digits_frac = 0;
            end_idx = str_idx;
        }
        if digits_int + digits_frac == 0 {
            *self = ZERO_MY_DECIMAL;
            return Err(ERR_TRUNCATED_WRONG_VAL.gen_with_stack_by_args(&[b"DECIMAL", str]));
        }
        let words_int = digits_to_words(digits_int);
        let words_frac = digits_to_words(digits_frac);
        let (words_int, words_frac, mut err) = fix_word_cnt_error(words_int, words_frac);
        if err.is_err() {
            digits_frac = words_frac * DIGITS_PER_WORD;
            if err == Err(ERR_OVERFLOW) {
                digits_int = words_int * DIGITS_PER_WORD;
            }
        }
        self.digits_int = digits_int as i8;
        self.digits_frac = digits_frac as i8;
        let mut word_idx = words_int;
        let str_idx_tmp = str_idx;
        let mut word: i32 = 0;
        let mut inner_idx: isize = 0;
        while digits_int > 0 {
            digits_int -= 1;
            str_idx -= 1;
            word +=
                i32::from(str[str_idx as usize].wrapping_sub(b'0')) * POWERS10[inner_idx as usize];
            inner_idx += 1;
            if inner_idx == DIGITS_PER_WORD {
                word_idx -= 1;
                self.word_buf[word_idx as usize] = word;
                word = 0;
                inner_idx = 0;
            }
        }
        if inner_idx != 0 {
            word_idx -= 1;
            self.word_buf[word_idx as usize] = word;
        }

        word_idx = words_int;
        str_idx = str_idx_tmp;
        word = 0;
        inner_idx = 0;
        while digits_frac > 0 {
            digits_frac -= 1;
            str_idx += 1;
            word = i32::from(str[str_idx as usize].wrapping_sub(b'0')) + word * 10;
            inner_idx += 1;
            if inner_idx == DIGITS_PER_WORD {
                self.word_buf[word_idx as usize] = word;
                word_idx += 1;
                word = 0;
                inner_idx = 0;
            }
        }
        if inner_idx != 0 {
            self.word_buf[word_idx as usize] =
                word * POWERS10[(DIGITS_PER_WORD - inner_idx) as usize];
        }
        if end_idx + 1 <= str.len() as isize {
            if str[end_idx as usize] == b'e' || str[end_idx as usize] == b'E' {
                let (exponent, err1) = str_to_int(&str[(end_idx + 1) as usize..]);
                if let Err(e1) = err1 {
                    err = Err(e1); // errors.Cause(err1)
                    if err != Err(ERR_TRUNCATED) {
                        *self = ZERO_MY_DECIMAL;
                    }
                }
                if exponent > i64::from(i32::MAX / 2) {
                    let negative = self.negative;
                    max_decimal(WORD_BUF_LEN * DIGITS_PER_WORD, 0, self);
                    self.negative = negative;
                    err = Err(ERR_OVERFLOW);
                }
                if exponent < i64::from(i32::MIN / 2) && err != Err(ERR_OVERFLOW) {
                    *self = ZERO_MY_DECIMAL;
                    err = Err(ERR_TRUNCATED);
                }
                if err != Err(ERR_OVERFLOW) {
                    let shift_err = self.shift(exponent as isize);
                    if let Err(shift_err) = shift_err {
                        if shift_err == ERR_OVERFLOW {
                            let negative = self.negative;
                            max_decimal(WORD_BUF_LEN * DIGITS_PER_WORD, 0, self);
                            self.negative = negative;
                        }
                        err = Err(shift_err);
                    }
                }
            } else {
                let trimstr = gostd::trim_space(&str[end_idx as usize..]);
                if !trimstr.is_empty() {
                    err = Err(ERR_TRUNCATED);
                }
            }
        }
        let mut all_zero = true;
        for i in 0..WORD_BUF_LEN {
            if self.word_buf[i as usize] != 0 {
                all_zero = false;
                break;
            }
        }
        if all_zero {
            self.negative = false;
        }
        self.result_frac = self.digits_frac;
        err
    }

    /// Shift shifts decimal digits in given number (with rounding if it need), shift > 0 means shift to left shift,
    /// shift < 0 means right shift. In fact it is multiplying on 10^shift.
    ///
    /// RETURN
    ///
    ///  eDecOK          OK
    ///  eDecOverflow    operation lead to overflow, number is untoched
    ///  eDecTruncated   number was rounded to fit into buffer
    // go: MyDecimal.Shift L1 [M-INT, M-ERRVAL]
    pub fn shift(&mut self, shift: isize) -> Result<(), Error> {
        let mut err: Result<(), Error> = Ok(());
        if shift == 0 {
            return Ok(());
        }
        // digitBegin is index of first non zero digit (all indexes from 0).
        // digitEnd is index of position after last decimal digit.
        // point is index of digit position just after point.
        let point = digits_to_words(self.digits_int as isize) * DIGITS_PER_WORD;
        // new point position.
        let mut new_point = point + shift;
        // number of digits in result.
        let (mut digit_begin, mut digit_end) = self.digit_bounds();
        if digit_begin == digit_end {
            *self = ZERO_MY_DECIMAL;
            return Ok(());
        }

        let mut digits_int = new_point - digit_begin;
        if digits_int < 0 {
            digits_int = 0;
        }
        let mut digits_frac = digit_end - new_point;
        if digits_frac < 0 {
            digits_frac = 0;
        }
        let words_int = digits_to_words(digits_int);
        let mut words_frac = digits_to_words(digits_frac);
        let new_len = words_int + words_frac;
        if new_len > WORD_BUF_LEN {
            let lack = new_len - WORD_BUF_LEN;
            if words_frac < lack {
                return Err(ERR_OVERFLOW);
            }
            /* cut off fraction part to allow new number to fit in our buffer */
            err = Err(ERR_TRUNCATED);
            words_frac -= lack;
            let diff = digits_frac - words_frac * DIGITS_PER_WORD;
            self.round_in_place(digit_end - point - diff, MODE_HALF_UP)?;
            digit_end -= diff;
            digits_frac = words_frac * DIGITS_PER_WORD;
            if digit_end <= digit_begin {
                /*
                   We lost all digits (they will be shifted out of buffer), so we can
                   just return 0.
                */
                *self = ZERO_MY_DECIMAL;
                return Err(ERR_TRUNCATED);
            }
        }

        if shift % DIGITS_PER_WORD != 0 {
            let l_mini_shift;
            let r_mini_shift;
            let mini_shift;
            let do_left;
            /*
               Calculate left/right shift to align decimal digits inside our bug
               digits correctly.
            */
            if shift > 0 {
                l_mini_shift = shift % DIGITS_PER_WORD;
                r_mini_shift = DIGITS_PER_WORD - l_mini_shift;
                do_left = l_mini_shift <= digit_begin;
            } else {
                r_mini_shift = (-shift) % DIGITS_PER_WORD;
                l_mini_shift = DIGITS_PER_WORD - r_mini_shift;
                do_left = (DIGITS_PER_WORD * WORD_BUF_LEN - digit_end) < r_mini_shift;
            }
            if do_left {
                self.do_mini_left_shift(l_mini_shift, digit_begin, digit_end);
                mini_shift = -l_mini_shift;
            } else {
                self.do_mini_right_shift(r_mini_shift, digit_begin, digit_end);
                mini_shift = r_mini_shift;
            }
            new_point += mini_shift;
            /*
               If number is shifted and correctly aligned in buffer we can finish.
            */
            if shift + mini_shift == 0 && (new_point - digits_int) < DIGITS_PER_WORD {
                self.digits_int = digits_int as i8;
                self.digits_frac = digits_frac as i8;
                return err; /* already shifted as it should be */
            }
            digit_begin += mini_shift;
            digit_end += mini_shift;
        }

        /* if new 'decimal front' is in first digit, we do not need move digits */
        let new_front = new_point - digits_int;
        if new_front >= DIGITS_PER_WORD || new_front < 0 {
            /* need to move digits */
            let mut word_shift;
            if new_front > 0 {
                /* move left */
                word_shift = new_front / DIGITS_PER_WORD;
                let mut to = digit_begin / DIGITS_PER_WORD - word_shift;
                let mut barier = (digit_end - 1) / DIGITS_PER_WORD - word_shift;
                while to <= barier {
                    self.word_buf[to as usize] = self.word_buf[(to + word_shift) as usize];
                    to += 1;
                }
                barier += word_shift;
                while to <= barier {
                    self.word_buf[to as usize] = 0;
                    to += 1;
                }
                word_shift = -word_shift;
            } else {
                /* move right */
                word_shift = (1 - new_front) / DIGITS_PER_WORD;
                let mut to = (digit_end - 1) / DIGITS_PER_WORD + word_shift;
                let mut barier = digit_begin / DIGITS_PER_WORD + word_shift;
                while to >= barier {
                    self.word_buf[to as usize] = self.word_buf[(to - word_shift) as usize];
                    to -= 1;
                }
                barier -= word_shift;
                while to >= barier {
                    self.word_buf[to as usize] = 0;
                    to -= 1;
                }
            }
            let digit_shift = word_shift * DIGITS_PER_WORD;
            digit_begin += digit_shift;
            digit_end += digit_shift;
            new_point += digit_shift;
        }
        /*
           If there are gaps then fill them with 0.

           Only one of following 'for' loops will work because wordIdxBegin <= wordIdxEnd.
        */
        let word_idx_begin = digit_begin / DIGITS_PER_WORD;
        let word_idx_end = (digit_end - 1) / DIGITS_PER_WORD;
        let mut word_idx_new_point = 0;

        /* We don't want negative new_point below */
        if new_point != 0 {
            word_idx_new_point = (new_point - 1) / DIGITS_PER_WORD;
        }
        if word_idx_new_point > word_idx_end {
            while word_idx_new_point > word_idx_end {
                self.word_buf[word_idx_new_point as usize] = 0;
                word_idx_new_point -= 1;
            }
        } else {
            while word_idx_new_point < word_idx_begin {
                self.word_buf[word_idx_new_point as usize] = 0;
                word_idx_new_point += 1;
            }
        }
        self.digits_int = digits_int as i8;
        self.digits_frac = digits_frac as i8;
        err
    }

    /// digitBounds returns bounds of decimal digits in the number.
    ///
    ///  start - index (from 0 ) of first decimal digits.
    ///  end   - index of position just after last decimal digit.
    // go: MyDecimal.digitBounds L1
    fn digit_bounds(&self) -> (isize, isize) {
        let mut i;
        let mut start;
        let mut end;
        let mut buf_beg: isize = 0;
        let buf_len =
            digits_to_words(self.digits_int as isize) + digits_to_words(self.digits_frac as isize);
        let mut buf_end = buf_len - 1;

        /* find non-zero digit from number beginning */
        while buf_beg < buf_len && self.word_buf[buf_beg as usize] == 0 {
            buf_beg += 1;
        }
        if buf_beg >= buf_len {
            return (0, 0);
        }

        /* find non-zero decimal digit from number beginning */
        if buf_beg == 0 && self.digits_int > 0 {
            i = (self.digits_int as isize - 1) % DIGITS_PER_WORD;
            start = DIGITS_PER_WORD - i - 1;
        } else {
            i = DIGITS_PER_WORD - 1;
            start = buf_beg * DIGITS_PER_WORD;
        }
        if buf_beg < buf_len {
            start += count_leading_zeroes(i, self.word_buf[buf_beg as usize]);
        }

        /* find non-zero digit at the end */
        while buf_end > buf_beg && self.word_buf[buf_end as usize] == 0 {
            buf_end -= 1;
        }
        /* find non-zero decimal digit from the end */
        if buf_end == buf_len - 1 && self.digits_frac > 0 {
            i = (self.digits_frac as isize - 1) % DIGITS_PER_WORD + 1;
            end = buf_end * DIGITS_PER_WORD + i;
            i = DIGITS_PER_WORD - i + 1;
        } else {
            end = (buf_end + 1) * DIGITS_PER_WORD;
            i = 1;
        }
        end -= count_trailing_zeroes(i, self.word_buf[buf_end as usize]);
        (start, end)
    }

    /// doMiniLeftShift does left shift for alignment of data in buffer.
    ///
    ///  shift   number of decimal digits on which it should be shifted
    ///  beg/end bounds of decimal digits (see digitsBounds())
    ///
    /// NOTE
    ///
    ///  Result fitting in the buffer should be garanted.
    ///  'shift' have to be from 1 to digitsPerWord-1 (inclusive)
    // go: MyDecimal.doMiniLeftShift L1
    fn do_mini_left_shift(&mut self, shift: isize, beg: isize, end: isize) {
        let mut buf_from = beg / DIGITS_PER_WORD;
        let buf_end = (end - 1) / DIGITS_PER_WORD;
        let c_shift = DIGITS_PER_WORD - shift;
        let wb = &mut self.word_buf;
        if beg % DIGITS_PER_WORD < shift {
            wb[(buf_from - 1) as usize] = wb[buf_from as usize] / POWERS10[c_shift as usize];
        }
        while buf_from < buf_end {
            wb[buf_from as usize] = (wb[buf_from as usize] % POWERS10[c_shift as usize])
                * POWERS10[shift as usize]
                + wb[(buf_from + 1) as usize] / POWERS10[c_shift as usize];
            buf_from += 1;
        }
        wb[buf_from as usize] =
            (wb[buf_from as usize] % POWERS10[c_shift as usize]) * POWERS10[shift as usize];
    }

    /// doMiniRightShift does right shift for alignment of data in buffer.
    ///
    ///  shift   number of decimal digits on which it should be shifted
    ///  beg/end bounds of decimal digits (see digitsBounds())
    ///
    /// NOTE
    ///
    ///  Result fitting in the buffer should be garanted.
    ///  'shift' have to be from 1 to digitsPerWord-1 (inclusive)
    // go: MyDecimal.doMiniRightShift L1
    fn do_mini_right_shift(&mut self, shift: isize, beg: isize, end: isize) {
        let mut buf_from = (end - 1) / DIGITS_PER_WORD;
        let buf_end = beg / DIGITS_PER_WORD;
        let c_shift = DIGITS_PER_WORD - shift;
        let wb = &mut self.word_buf;
        if DIGITS_PER_WORD - ((end - 1) % DIGITS_PER_WORD + 1) < shift {
            wb[(buf_from + 1) as usize] =
                (wb[buf_from as usize] % POWERS10[shift as usize]) * POWERS10[c_shift as usize];
        }
        while buf_from > buf_end {
            wb[buf_from as usize] = wb[buf_from as usize] / POWERS10[shift as usize]
                + (wb[(buf_from - 1) as usize] % POWERS10[shift as usize])
                    * POWERS10[c_shift as usize];
            buf_from -= 1;
        }
        wb[buf_from as usize] /= POWERS10[shift as usize];
    }
}

/// Round rounds the decimal to "frac" digits.
///
/// ```text
///     to   - result buffer. d == to is allowed
///     frac   - to what position after fraction point to round. can be negative!
///     roundMode  - round to nearest even or truncate
///      ModeHalfUp rounds normally.
///      ModeTruncate just truncates the decimal.
///
/// NOTES
///
///  frac can be negative !
///  one TRUNCATED error (line XXX below) isn't treated very logical :(
///
/// RETURN VALUE
///
///  nil/ErrTruncated/ErrOverflow
/// ```
///
/// Go allows `to == d`. Here `d` is a snapshot and `to_is_d` carries Go's `to != d` test:
/// every read of `d` in Go happens before the first write to `to` that is not followed by a
/// return, so reading the snapshot is the same as reading through the aliased pointer.
/// A free function (not a method) because `d` and `to` are separate parameters; the two
/// public entry points `round` and `round_in_place` follow it.
// go: MyDecimal.Round L1 [M-ALIAS, M-INT, M-ERRVAL]
fn round(
    d: &MyDecimal,
    to: &mut MyDecimal,
    to_is_d: bool,
    mut frac: isize,
    round_mode: RoundMode,
) -> Result<(), Error> {
    let mut err: Result<(), Error> = Ok(());
    // wordsFracTo is the number of fraction words in buffer.
    let mut words_frac_to = (frac + 1) / DIGITS_PER_WORD;
    if frac > 0 {
        words_frac_to = digits_to_words(frac);
    }
    let words_frac = digits_to_words(d.digits_frac as isize);
    let words_int = digits_to_words(d.digits_int as isize);

    let round_digit = round_mode.0;
    /* TODO - fix this code as it won't work for CEILING mode */

    if words_int + words_frac_to > WORD_BUF_LEN {
        words_frac_to = WORD_BUF_LEN - words_int;
        frac = words_frac_to * DIGITS_PER_WORD;
        err = Err(ERR_TRUNCATED);
    }
    if d.digits_int as isize + frac < 0 {
        *to = ZERO_MY_DECIMAL;
        return Ok(());
    }
    if !to_is_d {
        to.word_buf = d.word_buf;
        to.negative = d.negative;
        to.digits_int = (words_int.min(WORD_BUF_LEN) * DIGITS_PER_WORD) as i8;
    }
    if words_frac_to > words_frac {
        let mut idx = words_int + words_frac;
        while words_frac_to > words_frac {
            words_frac_to -= 1;
            to.word_buf[idx as usize] = 0;
            idx += 1;
        }
        to.digits_frac = frac as i8;
        to.result_frac = to.digits_frac;
        return err;
    }
    if frac >= d.digits_frac as isize {
        to.digits_frac = frac as i8;
        to.result_frac = to.digits_frac;
        return err;
    }

    // Do increment.
    let mut to_idx = words_int + words_frac_to - 1;
    if frac == words_frac_to * DIGITS_PER_WORD {
        let mut do_inc = false;
        match round_mode {
            // Notice: No support for ceiling mode now.
            MODE_CEILING => {
                // If any word after scale is not zero, do increment.
                // e.g ceiling 3.0001 to scale 1, gets 3.1
                let mut idx = to_idx + (words_frac - words_frac_to);
                while idx > to_idx {
                    if d.word_buf[idx as usize] != 0 {
                        do_inc = true;
                        break;
                    }
                    idx -= 1;
                }
            }
            MODE_HALF_UP => {
                let dig_after_scale = d.word_buf[(to_idx + 1) as usize] / DIG_MASK; // the first digit after scale.
                // If first digit after scale is equal to or greater than 5, do increment.
                do_inc = dig_after_scale >= 5;
            }
            MODE_TRUNCATE => {
                // Never round, just truncate.
                do_inc = false;
            }
            _ => {}
        }
        if do_inc {
            if to_idx >= 0 {
                to.word_buf[to_idx as usize] = to.word_buf[to_idx as usize].wrapping_add(1);
            } else {
                to_idx += 1;
                to.word_buf[to_idx as usize] = WORD_BASE;
            }
        } else if words_int + words_frac_to == 0 {
            *to = ZERO_MY_DECIMAL;
            return Ok(());
        }
    } else {
        /* TODO - fix this code as it won't work for CEILING mode */
        let pos = words_frac_to * DIGITS_PER_WORD - frac - 1;
        let mut shifted_number = to.word_buf[to_idx as usize] / POWERS10[pos as usize];
        let dig_after_scale = shifted_number % 10;
        if dig_after_scale > round_digit || (round_digit == 5 && dig_after_scale == 5) {
            shifted_number = shifted_number.wrapping_add(10);
        }
        to.word_buf[to_idx as usize] =
            POWERS10[pos as usize].wrapping_mul(shifted_number - dig_after_scale);
    }
    /*
       In case we're rounding e.g. 1.5e9 to 2.0e9, the decimal words inside
       the buffer are as follows.

       Before <1, 5e8>
       After  <2, 5e8>

       Hence we need to set the 2nd field to 0.
       The same holds if we round 1.5e-9 to 2e-9.
    */
    if words_frac_to < words_frac {
        let mut idx = words_int + words_frac_to;
        if frac == 0 && words_int == 0 {
            idx = 1;
        }
        while idx < WORD_BUF_LEN {
            to.word_buf[idx as usize] = 0;
            idx += 1;
        }
    }

    // Handle carry.
    let mut carry: i32;
    if to.word_buf[to_idx as usize] >= WORD_BASE {
        carry = 1;
        to.word_buf[to_idx as usize] -= WORD_BASE;
        while carry == 1 && to_idx > 0 {
            to_idx -= 1;
            (to.word_buf[to_idx as usize], carry) = add(to.word_buf[to_idx as usize], 0, carry);
        }
        if carry > 0 {
            if words_int + words_frac_to >= WORD_BUF_LEN {
                words_frac_to -= 1;
                frac = words_frac_to * DIGITS_PER_WORD;
                err = Err(ERR_TRUNCATED);
            }
            to_idx = words_int + words_frac_to.max(0);
            while to_idx > 0 {
                if to_idx < WORD_BUF_LEN {
                    to.word_buf[to_idx as usize] = to.word_buf[(to_idx - 1) as usize];
                } else {
                    err = Err(ERR_OVERFLOW);
                }
                to_idx -= 1;
            }
            to.word_buf[to_idx as usize] = 1;
            /* We cannot have more than 9 * 9 = 81 digits. */
            if (to.digits_int as isize) < DIGITS_PER_WORD * WORD_BUF_LEN {
                to.digits_int += 1;
            } else {
                err = Err(ERR_OVERFLOW);
            }
        }
    } else {
        loop {
            if to.word_buf[to_idx as usize] != 0 {
                break;
            }
            if to_idx == 0 {
                /* making 'zero' with the proper scale */
                let idx = words_frac_to + 1;
                to.digits_int = 1;
                to.digits_frac = frac.max(0) as i8;
                to.negative = false;
                while to_idx < idx {
                    to.word_buf[to_idx as usize] = 0;
                    to_idx += 1;
                }
                to.result_frac = to.digits_frac;
                return Ok(());
            }
            to_idx -= 1;
        }
    }
    /* Here we check 999.9 -> 1000 case when we need to increase intDigCnt */
    let first_dig = MOD9[to.digits_int as usize];
    if first_dig > 0 && to.word_buf[to_idx as usize] >= POWERS10[first_dig as usize] {
        to.digits_int = to.digits_int.wrapping_add(1);
    }
    if frac < 0 {
        frac = 0;
    }
    to.digits_frac = frac as i8;
    to.result_frac = to.digits_frac;
    err
}

impl MyDecimal {
    /// Round rounds the decimal to "frac" digits into `to` (a different decimal; for Go's
    /// `d.Round(d, ...)` use [`MyDecimal::round_in_place`]).
    // go-part: MyDecimal.Round L1 [M-ALIAS]
    pub fn round(
        &self,
        to: &mut MyDecimal,
        frac: isize,
        round_mode: RoundMode,
    ) -> Result<(), Error> {
        round(self, to, false, frac, round_mode)
    }

    /// Go's `d.Round(d, frac, roundMode)`.
    // go-part: MyDecimal.Round L1 [M-ALIAS]
    pub fn round_in_place(&mut self, frac: isize, round_mode: RoundMode) -> Result<(), Error> {
        let d = *self;
        round(&d, self, true, frac, round_mode)
    }

    /// FromInt sets the decimal value from int64.
    ///
    /// Like Go, this does not reset the receiver: `negative` is only set (for `val < 0`),
    /// never cleared, and `resultFrac` and the unused words keep their old values. Use a
    /// fresh `MyDecimal::default()` (Go: `NewDecFromInt`) unless that is intended.
    // go: MyDecimal.FromInt L1 [M-INT]
    pub fn from_int(&mut self, val: i64) -> &mut MyDecimal {
        let u_val: u64;
        if val < 0 {
            self.negative = true;
            u_val = val.wrapping_neg() as u64;
        } else {
            u_val = val as u64;
        }
        self.from_uint(u_val)
    }

    /// FromUint sets the decimal value from uint64.
    ///
    /// Like Go, this does not reset the receiver: `negative`, `resultFrac` and the unused
    /// words keep their old values (a negative receiver gives a negative result). Use a
    /// fresh `MyDecimal::default()` (Go: `NewDecFromUint`) unless that is intended.
    // go: MyDecimal.FromUint L1 [M-INT]
    pub fn from_uint(&mut self, val: u64) -> &mut MyDecimal {
        let mut x = val;
        let mut word_idx: isize = 1;
        while x >= WORD_BASE as u64 {
            word_idx += 1;
            x /= WORD_BASE as u64;
        }
        self.digits_frac = 0;
        self.digits_int = (word_idx * DIGITS_PER_WORD) as i8;
        x = val;
        while word_idx > 0 {
            word_idx -= 1;
            let y = x / WORD_BASE as u64;
            self.word_buf[word_idx as usize] = (x - y * WORD_BASE as u64) as i32;
            x = y;
        }
        self
    }

    /// ToInt returns int part of the decimal, returns the result and errcode.
    // go: MyDecimal.ToInt L1 [M-INT, M-ERRVAL]
    pub fn to_int(&self) -> (i64, Result<(), Error>) {
        let wb = i64::from(WORD_BASE);
        let mut x: i64 = 0;
        let mut word_idx = 0;
        let mut i = self.digits_int;
        while i > 0 {
            let y = x;
            /*
               Attention: trick!
               we're calculating -|from| instead of |from| here
               because |LONGLONG_MIN| > LONGLONG_MAX
               so we can convert -9223372036854775808 correctly
            */
            x = x
                .wrapping_mul(wb)
                .wrapping_sub(i64::from(self.word_buf[word_idx]));
            word_idx += 1;
            if y < i64::MIN / wb || x > y {
                /*
                   the decimal is bigger than any possible integer
                   return border integer depending on the sign
                */
                if self.negative {
                    return (i64::MIN, Err(ERR_OVERFLOW));
                }
                return (i64::MAX, Err(ERR_OVERFLOW));
            }
            i -= DIGITS_PER_WORD as i8;
        }
        /* boundary case: 9223372036854775808 */
        if !self.negative && x == i64::MIN {
            return (i64::MAX, Err(ERR_OVERFLOW));
        }
        if !self.negative {
            x = -x;
        }
        let mut i = self.digits_frac;
        while i > 0 {
            if self.word_buf[word_idx] != 0 {
                return (x, Err(ERR_TRUNCATED));
            }
            word_idx += 1;
            i -= DIGITS_PER_WORD as i8;
        }
        (x, Ok(()))
    }

    /// ToUint returns int part of the decimal, returns the result and errcode.
    // go: MyDecimal.ToUint L1 [M-INT, M-ERRVAL]
    pub fn to_uint(&self) -> (u64, Result<(), Error>) {
        if self.negative {
            return (0, Err(ERR_OVERFLOW));
        }
        let wb = WORD_BASE as u64;
        let mut x: u64 = 0;
        let mut word_idx = 0;
        let mut i = self.digits_int;
        while i > 0 {
            let y = x;
            x = x
                .wrapping_mul(wb)
                .wrapping_add(self.word_buf[word_idx] as u64);
            word_idx += 1;
            if y > u64::MAX / wb || x < y {
                return (u64::MAX, Err(ERR_OVERFLOW));
            }
            i -= DIGITS_PER_WORD as i8;
        }
        let mut i = self.digits_frac;
        while i > 0 {
            if self.word_buf[word_idx] != 0 {
                return (x, Err(ERR_TRUNCATED));
            }
            word_idx += 1;
            i -= DIGITS_PER_WORD as i8;
        }
        (x, Ok(()))
    }

    /// FromFloat64 creates a decimal from float64 value.
    ///
    /// Goes through `from_string`, so the receiver is not reset either (see there).
    // go: MyDecimal.FromFloat64 L1 [M-ERRVAL]
    pub fn from_float64(&mut self, f: f64) -> Result<(), Error> {
        let s = gostd::format_float_g(f);
        self.from_string(s.as_bytes())
    }

    /// ToFloat64 converts decimal to float64 value.
    ///
    /// The two word sums `f += float64(x) * pow10off81[...]` are multiply-adds that Go gc
    /// fuses into one FMA instruction on some targets only (mapping rule `M-FMA`), which can
    /// change the result in the last digit when `resultFrac < digitsFrac` (e.g. `DecimalDiv`
    /// results: `50.77 / 32` with fracIncr 4 gives 1.586563 on arm64, 1.586562 on amd64).
    /// They go through [`to_float64_madd`], which follows Go per target.
    // go: MyDecimal.ToFloat64 L1 [M-ERRVAL, M-FMA]
    pub fn to_float64(&self) -> (f64, Result<(), Error>) {
        let mut f: f64 = 0.0;
        let mut err = Ok(());
        let digits_int = self.digits_int as isize;
        let digits_frac = self.digits_frac as isize;
        // https://en.wikipedia.org/wiki/Double-precision_floating-point_format#IEEE_754_double-precision_binary_floating-point_format:_binary64
        // "The 53-bit significand precision gives from 15 to 17 significant decimal digits precision (2−53 ≈ 1.11 × 10−16).
        // If a decimal string with at most 15 significant digits is converted to IEEE 754 double-precision representation,
        // and then converted back to a decimal string with the same number of digits, the final result should match the original string."
        // The new method is about 10.5X faster than the old one according to the benchmark in types/mydecimal_benchmark_test.go.
        // The initial threshold here is 15, we adjusted it to 12 for compatibility with previous.
        // We did a full test of 12 significant digits to make sure it's correct and behaves as before.
        if digits_int + digits_frac > 12 {
            // Go: f, err = strconv.ParseFloat(...); f keeps ParseFloat's value on error.
            let parse_err;
            (f, parse_err) = gostd::parse_float(&self.string());
            if parse_err.is_err() {
                err = Err(ERR_OVERFLOW);
            }
            return (f, err);
        }
        let words_int = (digits_int - 1) / DIGITS_PER_WORD + 1;
        let mut word_idx: isize = 0;
        let mut i = 0;
        while i < digits_int {
            let x = self.word_buf[word_idx as usize];
            word_idx += 1;
            // Equivalent to f += float64(x) * math.Pow10((wordsInt-wordIdx)*digitsPerWord)
            // [M-FMA] mydecimal.go:1118
            f = to_float64_madd(
                f,
                f64::from(x),
                POW10OFF81[((words_int - word_idx) * DIGITS_PER_WORD + POW10OFF) as usize],
            );
            i += DIGITS_PER_WORD;
        }
        let frac_start = word_idx;
        let mut i = 0;
        while i < digits_frac {
            let x = self.word_buf[word_idx as usize];
            word_idx += 1;
            // Equivalent to f += float64(x) * math.Pow10(-digitsPerWord*(wordIdx-fracStart))
            // [M-FMA] mydecimal.go:1125
            f = to_float64_madd(
                f,
                f64::from(x),
                POW10OFF81[(-DIGITS_PER_WORD * (word_idx - frac_start) + POW10OFF) as usize],
            );
            i += DIGITS_PER_WORD;
        }
        // Equivalent to unit := math.Pow10(int(d.resultFrac))
        let unit = POW10OFF81[(self.result_frac as isize + POW10OFF) as usize];
        f = (f * unit).round() / unit;
        if self.negative {
            f = -f;
        }
        (f, err)
    }

    /// ToBin converts decimal to its binary fixed-length representation
    /// two representations of the same length can be compared with memcmp
    /// with the correct -1/0/+1 result
    ///
    /// See Go's comment on `ToBin` for the format. Go returns `([]byte, error)`; the bytes
    /// are meaningful together with ErrTruncated / ErrOverflow.
    // go: MyDecimal.ToBin L1 [M-ERRVAL]
    pub fn to_bin(&self, precision: isize, frac: isize) -> (Vec<u8>, Result<(), Error>) {
        self.write_bin(precision, frac, Vec::new())
    }

    /// WriteBin encode and write the binary encoded to target buffer
    ///
    /// Go appends to `buf` (re-slicing within its capacity when it fits); Rust takes the
    /// `Vec` by value, grows it with zeroed bytes and returns it.
    // go: MyDecimal.WriteBin L1 [M-INT, M-ERRVAL]
    pub fn write_bin(
        &self,
        precision: isize,
        frac: isize,
        mut buf: Vec<u8>,
    ) -> (Vec<u8>, Result<(), Error>) {
        if precision > DIGITS_PER_WORD * MAX_WORD_BUF_LEN as isize
            || precision < 0
            || frac > mysql::MAX_DECIMAL_SCALE as isize
            || frac < 0
        {
            return (buf, Err(ERR_BAD_NUMBER));
        }
        let mut err = Ok(());
        let mut mask: i32 = 0;
        if self.negative {
            mask = -1;
        }
        let mut digits_int = precision - frac;
        let words_int = digits_int / DIGITS_PER_WORD;
        let leading_digits = digits_int - words_int * DIGITS_PER_WORD;
        let words_frac = frac / DIGITS_PER_WORD;
        let trailing_digits = frac - words_frac * DIGITS_PER_WORD;

        let mut words_frac_from = self.digits_frac as isize / DIGITS_PER_WORD;
        let mut trailing_digits_from =
            self.digits_frac as isize - words_frac_from * DIGITS_PER_WORD;
        let mut int_size = words_int * WORD_SIZE + DIG2BYTES[leading_digits as usize];
        let mut frac_size = words_frac * WORD_SIZE + DIG2BYTES[trailing_digits as usize];
        let frac_size_from = words_frac_from * WORD_SIZE + DIG2BYTES[trailing_digits_from as usize];
        let origin_int_size = int_size;
        let origin_frac_size = frac_size;
        let buf_len = buf.len();
        buf.resize(buf_len + (int_size + frac_size) as usize, 0);
        let bin = &mut buf[buf_len..];
        let mut bin_idx: isize = 0;
        let (mut word_idx_from, digits_int_from) = self.remove_leading_zeros();
        if digits_int_from + frac_size_from == 0 {
            mask = 0;
            digits_int = 1;
        }

        let mut words_int_from = digits_int_from / DIGITS_PER_WORD;
        let mut leading_digits_from = digits_int_from - words_int_from * DIGITS_PER_WORD;
        let i_size_from = words_int_from * WORD_SIZE + DIG2BYTES[leading_digits_from as usize];

        if digits_int < digits_int_from {
            word_idx_from += words_int_from - words_int;
            if leading_digits_from > 0 {
                word_idx_from += 1;
            }
            if leading_digits > 0 {
                word_idx_from -= 1;
            }
            words_int_from = words_int;
            leading_digits_from = leading_digits;
            err = Err(ERR_OVERFLOW);
        } else if int_size > i_size_from {
            while int_size > i_size_from {
                int_size -= 1;
                bin[bin_idx as usize] = mask as u8;
                bin_idx += 1;
            }
        }

        if frac_size < frac_size_from
            || (frac_size == frac_size_from
                && (trailing_digits <= trailing_digits_from || words_frac <= words_frac_from))
        {
            if frac_size < frac_size_from
                || (frac_size == frac_size_from && trailing_digits < trailing_digits_from)
                || (frac_size == frac_size_from && words_frac < words_frac_from)
            {
                err = Err(ERR_TRUNCATED);
            }
            words_frac_from = words_frac;
            trailing_digits_from = trailing_digits;
        } else if frac_size > frac_size_from && trailing_digits_from > 0 {
            if words_frac == words_frac_from {
                trailing_digits_from = trailing_digits;
                frac_size = frac_size_from;
            } else {
                words_frac_from += 1;
                trailing_digits_from = 0;
            }
        }
        // xIntFrom part
        if leading_digits_from > 0 {
            let i = DIG2BYTES[leading_digits_from as usize];
            let x = (self.word_buf[word_idx_from as usize]
                % POWERS10[leading_digits_from as usize])
                ^ mask;
            word_idx_from += 1;
            write_word(&mut bin[bin_idx as usize..], x, i);
            bin_idx += i;
        }

        // wordsInt + wordsFrac part.
        let stop = word_idx_from + words_int_from + words_frac_from;
        while word_idx_from < stop {
            let x = self.word_buf[word_idx_from as usize] ^ mask;
            word_idx_from += 1;
            write_word(&mut bin[bin_idx as usize..], x, 4);
            bin_idx += WORD_SIZE;
        }

        // xFracFrom part
        if trailing_digits_from > 0 {
            let i = DIG2BYTES[trailing_digits_from as usize];
            let mut lim = trailing_digits;
            if words_frac_from < words_frac {
                lim = DIGITS_PER_WORD;
            }

            while trailing_digits_from < lim && DIG2BYTES[trailing_digits_from as usize] == i {
                trailing_digits_from += 1;
            }
            let x = (self.word_buf[word_idx_from as usize]
                / POWERS10[(DIGITS_PER_WORD - trailing_digits_from) as usize])
                ^ mask;
            write_word(&mut bin[bin_idx as usize..], x, i);
            bin_idx += i;
        }
        if frac_size > frac_size_from {
            let bin_idx_end = origin_int_size + origin_frac_size;
            while frac_size > frac_size_from && bin_idx < bin_idx_end {
                frac_size -= 1;
                bin[bin_idx as usize] = mask as u8;
                bin_idx += 1;
            }
        }
        bin[0] ^= 0x80;
        (buf, err)
    }

    /// ToHashKey removes the leading and trailing zeros and generates a hash key.
    /// Two Decimals dec0 and dec1 with different fraction will generate the same hash keys if dec0.Compare(dec1) == 0.
    // go: MyDecimal.ToHashKey L1 [M-INT, M-ERRVAL]
    pub fn to_hash_key(&self) -> (Vec<u8>, Result<(), Error>) {
        let (_, digits_int) = self.remove_leading_zeros();
        let (_, digits_frac) = self.remove_trailing_zeros();
        let mut prec = digits_int + digits_frac;
        if prec == 0 {
            // zeroDecimal
            prec = 1;
        }
        let (mut buf, mut err) = self.to_bin(prec, digits_frac);
        if err == Err(ERR_TRUNCATED) {
            // This err is caused by shorter digitsFrac;
            // After removing the trailing zeros from a Decimal,
            // so digitsFrac may be less than the real digitsFrac of the Decimal,
            // thus ErrTruncated may be raised, we can ignore it here.
            err = Ok(());
        }
        buf.push(digits_frac as u8);
        (buf, err)
    }

    /// PrecisionAndFrac returns the internal precision and frac number.
    // go: MyDecimal.PrecisionAndFrac L1
    pub fn precision_and_frac(&self) -> (isize, isize) {
        let frac = self.digits_frac as isize;
        let (_, digits_int) = self.remove_leading_zeros();
        let mut precision = digits_int + frac;
        if precision == 0 {
            precision = 1;
        }
        (precision, frac)
    }

    /// IsZero checks whether it's a zero decimal.
    // go: MyDecimal.IsZero L1
    pub fn is_zero(&self) -> bool {
        let mut is_zero = true;
        for val in self.word_buf {
            if val != 0 {
                is_zero = false;
                break;
            }
        }
        is_zero
    }

    /// FromBin Restores decimal from its binary fixed-length representation.
    ///
    /// Like Go, this does not reset the receiver: it sets `negative`, `resultFrac`, the
    /// digit counts and the words it reads; the other words keep their old values. An
    /// invalid precision/frac (`DecimalBinSize` error, size over 40) returns before
    /// touching the receiver at all; bad word values reset it to zero.
    // go: MyDecimal.FromBin L1 [M-INT, M-ERRVAL]
    pub fn from_bin(
        &mut self,
        bin: &[u8],
        precision: isize,
        frac: isize,
    ) -> (isize, Result<(), Error>) {
        if bin.is_empty() {
            *self = ZERO_MY_DECIMAL;
            return (0, Err(ERR_BAD_NUMBER));
        }
        let digits_int = precision - frac;
        let words_int = digits_int / DIGITS_PER_WORD;
        let leading_digits = digits_int - words_int * DIGITS_PER_WORD;
        let mut words_frac = frac / DIGITS_PER_WORD;
        let mut trailing_digits = frac - words_frac * DIGITS_PER_WORD;
        let mut words_int_to = words_int;
        if leading_digits > 0 {
            words_int_to += 1;
        }
        let mut words_frac_to = words_frac;
        if trailing_digits > 0 {
            words_frac_to += 1;
        }

        let mut bin_idx: isize = 0;
        let mut mask: i32 = -1;
        if bin[bin_idx as usize] & 0x80 > 0 {
            mask = 0;
        }
        let bin_size = match decimal_bin_size(precision, frac) {
            Ok(n) => n,
            Err(e) => return (0, Err(e)),
        };
        if !(0..=40).contains(&bin_size) {
            return (0, Err(ERR_BAD_NUMBER));
        }
        let mut d_copy_buf = [0u8; 40];
        let d_copy = &mut d_copy_buf[..bin_size as usize];
        let n = d_copy.len().min(bin.len());
        d_copy[..n].copy_from_slice(&bin[..n]);
        d_copy[0] ^= 0x80;
        let bin: &[u8] = d_copy;
        let old_words_int_to = words_int_to;
        let err;
        (words_int_to, words_frac_to, err) = fix_word_cnt_error(words_int_to, words_frac_to);
        if err.is_err() {
            if words_int_to < old_words_int_to {
                bin_idx +=
                    DIG2BYTES[leading_digits as usize] + (words_int - words_int_to) * WORD_SIZE;
            } else {
                trailing_digits = 0;
                words_frac = words_frac_to;
            }
        }
        self.negative = mask != 0;
        self.digits_int = (words_int * DIGITS_PER_WORD + leading_digits) as i8;
        self.digits_frac = (words_frac * DIGITS_PER_WORD + trailing_digits) as i8;

        let mut word_idx: isize = 0;
        if leading_digits > 0 {
            let i = DIG2BYTES[leading_digits as usize];
            let x = read_word(&bin[bin_idx as usize..], i);
            bin_idx += i;
            self.word_buf[word_idx as usize] = x ^ mask;
            if (self.word_buf[word_idx as usize] as u64)
                >= (POWERS10[(leading_digits + 1) as usize] as u64)
            {
                *self = ZERO_MY_DECIMAL;
                return (bin_size, Err(ERR_BAD_NUMBER));
            }
            if word_idx > 0 || self.word_buf[word_idx as usize] != 0 {
                word_idx += 1;
            } else {
                self.digits_int = self.digits_int.wrapping_sub(leading_digits as i8);
            }
        }
        let stop = bin_idx + words_int * WORD_SIZE;
        while bin_idx < stop {
            self.word_buf[word_idx as usize] = read_word(&bin[bin_idx as usize..], 4) ^ mask;
            if (self.word_buf[word_idx as usize] as u32) > WORD_MAX as u32 {
                *self = ZERO_MY_DECIMAL;
                return (bin_size, Err(ERR_BAD_NUMBER));
            }
            if word_idx > 0 || self.word_buf[word_idx as usize] != 0 {
                word_idx += 1;
            } else {
                self.digits_int = self.digits_int.wrapping_sub(DIGITS_PER_WORD as i8);
            }
            bin_idx += WORD_SIZE;
        }

        let stop = bin_idx + words_frac * WORD_SIZE;
        while bin_idx < stop {
            self.word_buf[word_idx as usize] = read_word(&bin[bin_idx as usize..], 4) ^ mask;
            if (self.word_buf[word_idx as usize] as u32) > WORD_MAX as u32 {
                *self = ZERO_MY_DECIMAL;
                return (bin_size, Err(ERR_BAD_NUMBER));
            }
            word_idx += 1;
            bin_idx += WORD_SIZE;
        }

        if trailing_digits > 0 {
            let i = DIG2BYTES[trailing_digits as usize];
            let x = read_word(&bin[bin_idx as usize..], i);
            self.word_buf[word_idx as usize] =
                (x ^ mask).wrapping_mul(POWERS10[(DIGITS_PER_WORD - trailing_digits) as usize]);
            if (self.word_buf[word_idx as usize] as u32) > WORD_MAX as u32 {
                *self = ZERO_MY_DECIMAL;
                return (bin_size, Err(ERR_BAD_NUMBER));
            }
        }

        if self.digits_int == 0 && self.digits_frac == 0 {
            *self = ZERO_MY_DECIMAL;
        }
        self.result_frac = frac as i8;
        (bin_size, err)
    }
}

/// DecimalBinSize returns the size of array to hold a binary representation of a decimal.
// go: DecimalBinSize L1 [M-ERRVAL]
pub fn decimal_bin_size(precision: isize, frac: isize) -> Result<isize, Error> {
    let digits_int = precision - frac;
    let words_int = digits_int / DIGITS_PER_WORD;
    let words_frac = frac / DIGITS_PER_WORD;
    let x_int = digits_int - words_int * DIGITS_PER_WORD;
    let x_frac = frac - words_frac * DIGITS_PER_WORD;
    if x_int < 0
        || x_int >= DIG2BYTES.len() as isize
        || x_frac < 0
        || x_frac >= DIG2BYTES.len() as isize
    {
        return Err(ERR_BAD_NUMBER);
    }
    Ok(words_int * WORD_SIZE
        + DIG2BYTES[x_int as usize]
        + words_frac * WORD_SIZE
        + DIG2BYTES[x_frac as usize])
}

// go: readWord L1 [M-INT]
fn read_word(b: &[u8], size: isize) -> i32 {
    let mut x: i32 = 0;
    match size {
        1 => {
            x = i32::from(b[0] as i8);
        }
        2 => {
            x = (i32::from(b[0] as i8) << 8) + i32::from(b[1]);
        }
        3 => {
            if b[0] & 128 > 0 {
                x = ((255u32 << 24)
                    | (u32::from(b[0]) << 16)
                    | (u32::from(b[1]) << 8)
                    | u32::from(b[2])) as i32;
            } else {
                x = ((u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2])) as i32;
            }
        }
        4 => {
            x = i32::from(b[3])
                + (i32::from(b[2]) << 8)
                + (i32::from(b[1]) << 16)
                + (i32::from(b[0] as i8) << 24);
        }
        _ => {}
    }
    x
}

// go: writeWord L1 [M-INT]
fn write_word(b: &mut [u8], word: i32, size: isize) {
    let v = word as u32;
    match size {
        1 => {
            b[0] = word as u8;
        }
        2 => {
            b[0] = (v >> 8) as u8;
            b[1] = v as u8;
        }
        3 => {
            b[0] = (v >> 16) as u8;
            b[1] = (v >> 8) as u8;
            b[2] = v as u8;
        }
        4 => {
            b[0] = (v >> 24) as u8;
            b[1] = (v >> 16) as u8;
            b[2] = (v >> 8) as u8;
            b[3] = v as u8;
        }
        _ => {}
    }
}

impl MyDecimal {
    /// Compare compares one decimal to another, returns -1/0/1.
    // go: MyDecimal.Compare L1
    pub fn compare(&self, to: &MyDecimal) -> isize {
        if self.negative == to.negative {
            let (cmp, err) = match do_sub(self, to, None) {
                Ok(cmp) => (cmp, Ok(())),
                Err(e) => (0, Err(e)),
            };
            terror_log(err);
            return cmp;
        }
        if self.negative {
            return -1;
        }
        1
    }

    /// MarshalJSON implements Marshaler.MarshalJSON interface.
    ///
    /// Go marshals `jsonMyDecimal{DigitsInt, DigitsFrac, ResultFrac, Negative, WordBuf}`
    /// with encoding/json; the output is written directly in the same form.
    // go: MyDecimal.MarshalJSON L1 [M-ERRVAL]
    pub fn marshal_json(&self) -> Result<Vec<u8>, Error> {
        let words: Vec<String> = self.word_buf.iter().map(i32::to_string).collect();
        Ok(format!(
            "{{\"DigitsInt\":{},\"DigitsFrac\":{},\"ResultFrac\":{},\"Negative\":{},\"WordBuf\":[{}]}}",
            self.digits_int,
            self.digits_frac,
            self.result_frac,
            self.negative,
            words.join(",")
        )
        .into_bytes())
    }

    /// UnmarshalJSON implements Unmarshaler.UnmarshalJSON interface.
    // go: MyDecimal.UnmarshalJSON L1 [M-ERRVAL]
    pub fn unmarshal_json(&mut self, data: &[u8]) -> Result<(), Error> {
        let r = gostd::json_unmarshal_my_decimal(data);
        match r {
            Ok(r) => {
                self.digits_int = r.digits_int;
                self.digits_frac = r.digits_frac;
                self.result_frac = r.result_frac;
                self.negative = r.negative;
                self.word_buf = r.word_buf;
                Ok(())
            }
            Err(msg) => Err(Error::other(msg)),
        }
    }
}

/// DecimalNeg reverses decimal's sign.
// go: DecimalNeg L1
pub fn decimal_neg(from: &MyDecimal) -> MyDecimal {
    let mut to = *from;
    if from.is_zero() {
        return to;
    }
    to.negative = !from.negative;
    to
}

/// DecimalAdd adds two decimals, sets the result to 'to'.
/// Note: DO NOT use `from1` or `from2` as `to` since the metadata
/// of `to` may be changed during evaluating.
// go: DecimalAdd L1 [M-ALIAS, M-ERRVAL]
pub fn decimal_add(from1: &MyDecimal, from2: &MyDecimal, to: &mut MyDecimal) -> Result<(), Error> {
    let (from1, from2) = validate_args(from1, from2, to);
    to.result_frac = from1.result_frac.max(from2.result_frac);
    if from1.negative == from2.negative {
        return do_add(from1, from2, to);
    }
    do_sub(from1, from2, Some(to)).map(|_| ())
}

/// DecimalSub subs one decimal from another, sets the result to 'to'.
// go: DecimalSub L1 [M-ALIAS, M-ERRVAL]
pub fn decimal_sub(from1: &MyDecimal, from2: &MyDecimal, to: &mut MyDecimal) -> Result<(), Error> {
    let (from1, from2) = validate_args(from1, from2, to);
    to.result_frac = from1.result_frac.max(from2.result_frac);
    if from1.negative == from2.negative {
        return do_sub(from1, from2, Some(to)).map(|_| ());
    }
    do_add(from1, from2, to)
}

/// Go also copies `f1`/`f2` when they are the same pointer as `to`, and returns early when
/// `to` is nil. Aliasing follows `M-ALIAS`: `to` is `&mut` and `f1`/`f2` are shared borrows,
/// so a caller that wants `to == from` copies `from` first (`MyDecimal: Copy`), which is
/// exactly Go's copy. The nil branch has no counterpart: `&mut` cannot be null, no caller in
/// v8.5.8 passes a nil `to`, and Go's DecimalAdd/Sub/Mul/Div/Mod write `to.resultFrac` right
/// after this call, so a nil `to` would panic there anyway.
// go: validateArgs L1 [M-ALIAS]
fn validate_args<'a>(
    f1: &'a MyDecimal,
    f2: &'a MyDecimal,
    to: &mut MyDecimal,
) -> (&'a MyDecimal, &'a MyDecimal) {
    to.digits_frac = 0;
    to.digits_int = 0;
    to.result_frac = 0;
    to.negative = false;
    for i in 0..to.word_buf.len() {
        to.word_buf[i] = 0;
    }
    (f1, f2)
}

/// Go returns `(cmp int, err error)`; every error return has cmp = 0.
// go: doSub L1 [M-INT, M-ERRVAL]
fn do_sub<'a>(
    mut from1: &'a MyDecimal,
    mut from2: &'a MyDecimal,
    to: Option<&mut MyDecimal>,
) -> Result<isize, Error> {
    let mut words_int1 = digits_to_words(from1.digits_int as isize);
    let mut words_frac1 = digits_to_words(from1.digits_frac as isize);
    let mut words_int2 = digits_to_words(from2.digits_int as isize);
    let mut words_frac2 = digits_to_words(from2.digits_frac as isize);
    let mut words_frac_to = words_frac1.max(words_frac2);

    let mut start1: isize = 0;
    let mut stop1 = words_int1;
    let mut idx1: isize = 0;
    let mut start2: isize = 0;
    let mut stop2 = words_int2;
    let mut idx2: isize = 0;
    if from1.word_buf[idx1 as usize] == 0 {
        while idx1 < stop1 && from1.word_buf[idx1 as usize] == 0 {
            idx1 += 1;
        }
        start1 = idx1;
        words_int1 = stop1 - idx1;
    }
    if from2.word_buf[idx2 as usize] == 0 {
        while idx2 < stop2 && from2.word_buf[idx2 as usize] == 0 {
            idx2 += 1;
        }
        start2 = idx2;
        words_int2 = stop2 - idx2;
    }

    let mut carry: i32 = 0;
    if words_int2 > words_int1 {
        carry = 1;
    } else if words_int2 == words_int1 {
        let mut end1 = stop1 + words_frac1 - 1;
        let mut end2 = stop2 + words_frac2 - 1;
        while idx1 <= end1 && from1.word_buf[end1 as usize] == 0 {
            end1 -= 1;
        }
        while idx2 <= end2 && from2.word_buf[end2 as usize] == 0 {
            end2 -= 1;
        }
        words_frac1 = end1 - stop1 + 1;
        words_frac2 = end2 - stop2 + 1;
        while idx1 <= end1
            && idx2 <= end2
            && from1.word_buf[idx1 as usize] == from2.word_buf[idx2 as usize]
        {
            idx1 += 1;
            idx2 += 1;
        }
        if idx1 <= end1 {
            if idx2 <= end2 && from2.word_buf[idx2 as usize] > from1.word_buf[idx1 as usize] {
                carry = 1;
            } else {
                carry = 0;
            }
        } else {
            if idx2 > end2 {
                let Some(to) = to else {
                    return Ok(0);
                };
                *to = zero_my_decimal_with_frac(to.result_frac);
                return Ok(0);
            }
            carry = 1;
        }
    }

    let Some(to) = to else {
        if (carry > 0) == from1.negative {
            // from2 is negative too.
            return Ok(1);
        }
        return Ok(-1);
    };

    to.negative = from1.negative;

    /* ensure that always idx1 > idx2 (and wordsInt1 >= wordsInt2) */
    if carry > 0 {
        std::mem::swap(&mut from1, &mut from2);
        std::mem::swap(&mut start1, &mut start2);
        std::mem::swap(&mut words_int1, &mut words_int2);
        std::mem::swap(&mut words_frac1, &mut words_frac2);
        to.negative = !to.negative;
    }

    let err;
    (words_int1, words_frac_to, err) = fix_word_cnt_error(words_int1, words_frac_to);
    let mut idx_to = words_int1 + words_frac_to;
    to.digits_frac = from1.digits_frac;
    if to.digits_frac < from2.digits_frac {
        to.digits_frac = from2.digits_frac;
    }
    to.digits_int = (words_int1 * DIGITS_PER_WORD) as i8;
    if err.is_err() {
        if to.digits_frac > (words_frac_to * DIGITS_PER_WORD) as i8 {
            to.digits_frac = (words_frac_to * DIGITS_PER_WORD) as i8;
        }
        if words_frac1 > words_frac_to {
            words_frac1 = words_frac_to;
        }
        if words_frac2 > words_frac_to {
            words_frac2 = words_frac_to;
        }
        if words_int2 > words_int1 {
            words_int2 = words_int1;
        }
    }
    carry = 0;

    /* part 1 - max(frac) ... min (frac) */
    if words_frac1 > words_frac2 {
        idx1 = start1 + words_int1 + words_frac1;
        stop1 = start1 + words_int1 + words_frac2;
        idx2 = start2 + words_int2 + words_frac2;
        while words_frac_to > words_frac1 {
            words_frac_to -= 1;
            idx_to -= 1;
            to.word_buf[idx_to as usize] = 0;
        }
        while idx1 > stop1 {
            idx_to -= 1;
            idx1 -= 1;
            to.word_buf[idx_to as usize] = from1.word_buf[idx1 as usize];
        }
    } else {
        idx1 = start1 + words_int1 + words_frac1;
        idx2 = start2 + words_int2 + words_frac2;
        stop2 = start2 + words_int2 + words_frac1;
        while words_frac_to > words_frac2 {
            words_frac_to -= 1;
            idx_to -= 1;
            to.word_buf[idx_to as usize] = 0;
        }
        while idx2 > stop2 {
            idx_to -= 1;
            idx2 -= 1;
            (to.word_buf[idx_to as usize], carry) = sub(0, from2.word_buf[idx2 as usize], carry);
        }
    }

    /* part 2 - min(frac) ... wordsInt2 */
    while idx2 > start2 {
        idx_to -= 1;
        idx1 -= 1;
        idx2 -= 1;
        (to.word_buf[idx_to as usize], carry) = sub(
            from1.word_buf[idx1 as usize],
            from2.word_buf[idx2 as usize],
            carry,
        );
    }

    /* part 3 - wordsInt2 ... wordsInt1 */
    while carry > 0 && idx1 > start1 {
        idx_to -= 1;
        idx1 -= 1;
        (to.word_buf[idx_to as usize], carry) = sub(from1.word_buf[idx1 as usize], 0, carry);
    }
    while idx1 > start1 {
        idx_to -= 1;
        idx1 -= 1;
        to.word_buf[idx_to as usize] = from1.word_buf[idx1 as usize];
    }
    while idx_to > 0 {
        idx_to -= 1;
        to.word_buf[idx_to as usize] = 0;
    }
    err.map(|()| 0)
}

// go: doAdd L1 [M-INT, M-ERRVAL]
fn do_add(from1: &MyDecimal, from2: &MyDecimal, to: &mut MyDecimal) -> Result<(), Error> {
    let mut words_int1 = digits_to_words(from1.digits_int as isize);
    let mut words_frac1 = digits_to_words(from1.digits_frac as isize);
    let mut words_int2 = digits_to_words(from2.digits_int as isize);
    let mut words_frac2 = digits_to_words(from2.digits_frac as isize);
    let mut words_int_to = words_int1.max(words_int2);
    let mut words_frac_to = words_frac1.max(words_frac2);

    let x: i32 = if words_int1 > words_int2 {
        from1.word_buf[0]
    } else if words_int2 > words_int1 {
        from2.word_buf[0]
    } else {
        from1.word_buf[0].wrapping_add(from2.word_buf[0])
    };
    if x > WORD_MAX - 1 {
        /* yes, there is */
        words_int_to += 1;
        to.word_buf[0] = 0; /* safety */
    }

    let err;
    (words_int_to, words_frac_to, err) = fix_word_cnt_error(words_int_to, words_frac_to);
    if err == Err(ERR_OVERFLOW) {
        max_decimal(WORD_BUF_LEN * DIGITS_PER_WORD, 0, to);
        return err;
    }
    let mut idx_to = words_int_to + words_frac_to;
    to.negative = from1.negative;
    to.digits_int = (words_int_to * DIGITS_PER_WORD) as i8;
    to.digits_frac = from1.digits_frac.max(from2.digits_frac);

    if err.is_err() {
        if to.digits_frac > (words_frac_to * DIGITS_PER_WORD) as i8 {
            to.digits_frac = (words_frac_to * DIGITS_PER_WORD) as i8;
        }
        if words_frac1 > words_frac_to {
            words_frac1 = words_frac_to;
        }
        if words_frac2 > words_frac_to {
            words_frac2 = words_frac_to;
        }
        if words_int1 > words_int_to {
            words_int1 = words_int_to;
        }
        if words_int2 > words_int_to {
            words_int2 = words_int_to;
        }
    }
    let (mut dec1, mut dec2) = (from1, from2);
    let mut idx1;
    let mut idx2;
    let mut stop;
    let mut stop2: isize = 0;
    /* part 1 - max(frac) ... min (frac) */
    if words_frac1 > words_frac2 {
        idx1 = words_int1 + words_frac1;
        stop = words_int1 + words_frac2;
        idx2 = words_int2 + words_frac2;
        if words_int1 > words_int2 {
            stop2 = words_int1 - words_int2;
        }
    } else {
        idx1 = words_int2 + words_frac2;
        stop = words_int2 + words_frac1;
        idx2 = words_int1 + words_frac1;
        if words_int2 > words_int1 {
            stop2 = words_int2 - words_int1;
        }
        (dec1, dec2) = (from2, from1);
    }
    while idx1 > stop {
        idx_to -= 1;
        idx1 -= 1;
        to.word_buf[idx_to as usize] = dec1.word_buf[idx1 as usize];
    }

    /* part 2 - min(frac) ... min(digitsInt) */
    let mut carry: i32 = 0;
    while idx1 > stop2 {
        idx1 -= 1;
        idx2 -= 1;
        idx_to -= 1;
        (to.word_buf[idx_to as usize], carry) = add(
            dec1.word_buf[idx1 as usize],
            dec2.word_buf[idx2 as usize],
            carry,
        );
    }

    /* part 3 - min(digitsInt) ... max(digitsInt) */
    stop = 0;
    if words_int1 > words_int2 {
        idx1 = words_int1 - words_int2;
        dec1 = from1;
    } else {
        idx1 = words_int2 - words_int1;
        dec1 = from2;
    }
    while idx1 > stop {
        idx_to -= 1;
        idx1 -= 1;
        (to.word_buf[idx_to as usize], carry) = add(dec1.word_buf[idx1 as usize], 0, carry);
    }
    if carry > 0 {
        idx_to -= 1;
        to.word_buf[idx_to as usize] = 1;
    }
    err
}

// go: maxDecimal L1 [M-INT]
fn max_decimal(precision: isize, mut frac: isize, to: &mut MyDecimal) {
    let mut digits_int = precision - frac;
    to.negative = false;
    to.digits_int = digits_int as i8;
    let mut idx: isize = 0;
    if digits_int > 0 {
        let first_word_digits = digits_int % DIGITS_PER_WORD;
        if first_word_digits > 0 {
            to.word_buf[idx as usize] = POWERS10[first_word_digits as usize] - 1; /* get 9 99 999 ... */
            idx += 1;
        }
        digits_int /= DIGITS_PER_WORD;
        while digits_int > 0 {
            to.word_buf[idx as usize] = WORD_MAX;
            idx += 1;
            digits_int -= 1;
        }
    }
    to.digits_frac = frac as i8;
    if frac > 0 {
        let last_digits = frac % DIGITS_PER_WORD;
        frac /= DIGITS_PER_WORD;
        while frac > 0 {
            to.word_buf[idx as usize] = WORD_MAX;
            idx += 1;
            frac -= 1;
        }
        if last_digits > 0 {
            to.word_buf[idx as usize] = FRAC_MAX[(last_digits - 1) as usize];
        }
    }
}

/// DecimalMul multiplies two decimals.
///
/// ```text
///      from1, from2 - factors
///      to      - product
///
///  RETURN VALUE
///    E_DEC_OK/E_DEC_TRUNCATED/E_DEC_OVERFLOW;
///
///  NOTES
///    in this implementation, with wordSize=4 we have digitsPerWord=9,
///    and 63-digit number will take only 7 words (basically a 7-digit
///    "base 999999999" number).  Thus there's no need in fast multiplication
///    algorithms, 7-digit numbers can be multiplied with a naive O(n*n)
///    method.
/// ```
// go: DecimalMul L1 [M-ALIAS, M-INT, M-ERRVAL]
pub fn decimal_mul(from1: &MyDecimal, from2: &MyDecimal, to: &mut MyDecimal) -> Result<(), Error> {
    let (from1, from2) = validate_args(from1, from2, to);
    let err;
    let words_int1 = digits_to_words(from1.digits_int as isize);
    let mut words_frac1 = digits_to_words(from1.digits_frac as isize);
    let mut words_int2 = digits_to_words(from2.digits_int as isize);
    let mut words_frac2 = digits_to_words(from2.digits_frac as isize);
    let mut words_int_to = digits_to_words(from1.digits_int as isize + from2.digits_int as isize);
    let mut words_frac_to = words_frac1 + words_frac2;
    let mut idx1 = words_int1;
    let mut idx2 = words_int2;
    let mut idx_to: isize;
    let mut tmp1 = words_int_to;
    let mut tmp2 = words_frac_to;
    // Go adds the int8 fields in int8 and wraps.
    to.result_frac = from1
        .result_frac
        .wrapping_add(from2.result_frac)
        .min(mysql::MAX_DECIMAL_SCALE);
    (words_int_to, words_frac_to, err) = fix_word_cnt_error(words_int_to, words_frac_to);
    to.negative = from1.negative != from2.negative;
    to.digits_frac = from1.digits_frac.wrapping_add(from2.digits_frac);
    if to.digits_frac > NOT_FIXED_DEC {
        to.digits_frac = NOT_FIXED_DEC;
    }
    to.digits_int = (words_int_to * DIGITS_PER_WORD) as i8;
    if err == Err(ERR_OVERFLOW) {
        return err;
    }
    if err.is_err() {
        if to.digits_frac > (words_frac_to * DIGITS_PER_WORD) as i8 {
            to.digits_frac = (words_frac_to * DIGITS_PER_WORD) as i8;
        }
        if to.digits_int > (words_int_to * DIGITS_PER_WORD) as i8 {
            to.digits_int = (words_int_to * DIGITS_PER_WORD) as i8;
        }
        if tmp1 > words_int_to {
            tmp1 -= words_int_to;
            tmp2 = tmp1 >> 1;
            words_int2 -= tmp1 - tmp2;
            words_frac1 = 0;
            words_frac2 = 0;
        } else {
            tmp2 -= words_frac_to;
            tmp1 = tmp2 >> 1;
            if words_frac1 <= words_frac2 {
                words_frac1 -= tmp1;
                words_frac2 -= tmp2 - tmp1;
            } else {
                words_frac2 -= tmp1;
                words_frac1 -= tmp2 - tmp1;
            }
        }
    }
    let mut start_to = words_int_to + words_frac_to - 1;
    let start2 = idx2 + words_frac2 - 1;
    let stop1 = idx1 - words_int1;
    let stop2 = idx2 - words_int2;
    to.word_buf = ZERO_MY_DECIMAL.word_buf;

    idx1 += words_frac1 - 1;
    while idx1 >= stop1 {
        let mut carry: i32 = 0;
        idx_to = start_to;
        idx2 = start2;
        while idx2 >= stop2 {
            // int64 bounds: valid words give p < 1e18; any two int32 words give
            // |p| <= 2^62, and hi*wb <= 2^62 + wb: no int64 wrap even for invalid words.
            let p =
                i64::from(from1.word_buf[idx1 as usize]) * i64::from(from2.word_buf[idx2 as usize]);
            let hi = (p / i64::from(WORD_BASE)) as i32;
            let lo = (p - i64::from(hi) * i64::from(WORD_BASE)) as i32;
            (to.word_buf[idx_to as usize], carry) = add2(to.word_buf[idx_to as usize], lo, carry);
            carry = carry.wrapping_add(hi);
            idx2 -= 1;
            idx_to -= 1;
        }
        if carry > 0 {
            if idx_to < 0 {
                return Err(ERR_OVERFLOW);
            }
            (to.word_buf[idx_to as usize], carry) = add2(to.word_buf[idx_to as usize], 0, carry);
        }
        idx_to -= 1;
        while carry > 0 {
            if idx_to < 0 {
                return Err(ERR_OVERFLOW);
            }
            (to.word_buf[idx_to as usize], carry) = add(to.word_buf[idx_to as usize], 0, carry);
            idx_to -= 1;
        }
        start_to -= 1;
        idx1 -= 1;
    }

    /* Now we have to check for -0.000 case */
    if to.negative {
        let mut idx: isize = 0;
        let end = words_int_to + words_frac_to;
        loop {
            if to.word_buf[idx as usize] != 0 {
                break;
            }
            idx += 1;
            /* We got decimal zero */
            if idx == end {
                *to = zero_my_decimal_with_frac(to.result_frac);
                break;
            }
        }
    }

    idx_to = 0;
    let mut d_to_move = words_int_to + digits_to_words(to.digits_frac as isize);
    while to.word_buf[idx_to as usize] == 0 && to.digits_int > DIGITS_PER_WORD as i8 {
        idx_to += 1;
        to.digits_int -= DIGITS_PER_WORD as i8;
        d_to_move -= 1;
    }
    if idx_to > 0 {
        let mut cur_idx: isize = 0;
        while d_to_move > 0 {
            to.word_buf[cur_idx as usize] = to.word_buf[idx_to as usize];
            cur_idx += 1;
            idx_to += 1;
            d_to_move -= 1;
        }
    }
    err
}

/// DecimalDiv does division of two decimals.
///
/// from1    - dividend
/// from2    - divisor
/// to       - quotient
/// fracIncr - increment of fraction
// go: DecimalDiv L1 [M-ALIAS, M-INT, M-ERRVAL]
pub fn decimal_div(
    from1: &MyDecimal,
    from2: &MyDecimal,
    to: &mut MyDecimal,
    frac_incr: isize,
) -> Result<(), Error> {
    let (from1, from2) = validate_args(from1, from2, to);
    // Go: min(from1.resultFrac+int8(fracIncr), mysql.MaxDecimalScale) — int8 truncation and
    // wrapping addition.
    to.result_frac = from1
        .result_frac
        .wrapping_add(frac_incr as i8)
        .min(mysql::MAX_DECIMAL_SCALE);
    do_div_mod(from1, from2, Some(to), None, frac_incr)
}

/// DecimalMod does modulus of two decimals.
///
/// ```text
///      from1   - dividend
///      from2   - divisor
///      to      - modulus
///
///  RETURN VALUE
///    E_DEC_OK/E_DEC_TRUNCATED/E_DEC_OVERFLOW/E_DEC_DIV_ZERO;
///
///  NOTES
///    see do_div_mod()
///
///  DESCRIPTION
///    the modulus R in    R = M mod N
///
///   is defined as
///
///     0 <= |R| < |M|
///     sign R == sign M
///     R = M - k*N, where k is integer
///
///   thus, there's no requirement for M or N to be integers
/// ```
// go: DecimalMod L1 [M-ALIAS, M-ERRVAL]
pub fn decimal_mod(from1: &MyDecimal, from2: &MyDecimal, to: &mut MyDecimal) -> Result<(), Error> {
    let (from1, from2) = validate_args(from1, from2, to);
    to.result_frac = from1.result_frac.max(from2.result_frac);
    do_div_mod(from1, from2, None, Some(to), 0)
}

/// Go takes `to, mod *MyDecimal`, exactly one of them non-nil, and writes through `to = mod`
/// when `mod` is set; `is_mod` replaces the later `mod != nil` tests.
// go: doDivMod L1 [M-INT, M-ERRVAL]
fn do_div_mod(
    from1: &MyDecimal,
    from2: &MyDecimal,
    to: Option<&mut MyDecimal>,
    r#mod: Option<&mut MyDecimal>,
    mut frac_incr: isize,
) -> Result<(), Error> {
    let frac1 = digits_to_words(from1.digits_frac as isize) * DIGITS_PER_WORD;
    let mut prec1 = from1.digits_int as isize + frac1;
    let frac2 = digits_to_words(from2.digits_frac as isize) * DIGITS_PER_WORD;
    let mut prec2 = from2.digits_int as isize + frac2;
    let is_mod = r#mod.is_some();
    let to: &mut MyDecimal = match r#mod {
        Some(m) => m,
        None => to.expect("doDivMod: to and mod are both nil"),
    };

    /* removing all the leading zeros */
    let mut i = ((prec2 - 1) % DIGITS_PER_WORD) + 1;
    let mut idx2: isize = 0;
    while prec2 > 0 && from2.word_buf[idx2 as usize] == 0 {
        prec2 -= i;
        i = DIGITS_PER_WORD;
        idx2 += 1;
    }
    if prec2 <= 0 {
        /* short-circuit everything: from2 == 0 */
        return Err(ERR_DIV_BY_ZERO);
    }

    prec2 -= count_leading_zeroes((prec2 - 1) % DIGITS_PER_WORD, from2.word_buf[idx2 as usize]);
    i = ((prec1 - 1) % DIGITS_PER_WORD) + 1;
    let mut idx1: isize = 0;
    while prec1 > 0 && from1.word_buf[idx1 as usize] == 0 {
        prec1 -= i;
        i = DIGITS_PER_WORD;
        idx1 += 1;
    }
    if prec1 <= 0 {
        /* short-circuit everything: from1 == 0 */
        *to = zero_my_decimal_with_frac(to.result_frac);
        return Ok(());
    }
    prec1 -= count_leading_zeroes((prec1 - 1) % DIGITS_PER_WORD, from1.word_buf[idx1 as usize]);

    /* let's fix fracIncr, taking into account frac1,frac2 increase */
    frac_incr -= frac1 - from1.digits_frac as isize + frac2 - from2.digits_frac as isize;
    if frac_incr < 0 {
        frac_incr = 0;
    }

    let mut digits_int_to = (prec1 - frac1) - (prec2 - frac2);
    if from1.word_buf[idx1 as usize] >= from2.word_buf[idx2 as usize] {
        digits_int_to += 1;
    }
    let mut words_int_to;
    if digits_int_to < 0 {
        digits_int_to /= DIGITS_PER_WORD;
        words_int_to = 0;
    } else {
        words_int_to = digits_to_words(digits_int_to);
    }
    let mut words_frac_to: isize = 0;
    let mut err: Result<(), Error> = Ok(());
    if is_mod {
        // we're calculating N1 % N2.
        // The result will have
        // digitsFrac=max(frac1, frac2), as for subtraction
        // digitsInt=from2.digitsInt
        to.negative = from1.negative;
        to.digits_frac = from1.digits_frac.max(from2.digits_frac);
    } else {
        words_frac_to = digits_to_words(frac1 + frac2 + frac_incr);
        (words_int_to, words_frac_to, err) = fix_word_cnt_error(words_int_to, words_frac_to);
        to.negative = from1.negative != from2.negative;
        to.digits_int = (words_int_to * DIGITS_PER_WORD) as i8;
        to.digits_frac = (words_frac_to * DIGITS_PER_WORD) as i8;
    }
    let mut idx_to: isize = 0;
    let stop_to = words_int_to + words_frac_to;
    if !is_mod {
        while digits_int_to < 0 && idx_to < WORD_BUF_LEN {
            to.word_buf[idx_to as usize] = 0;
            idx_to += 1;
            digits_int_to += 1;
        }
    }
    i = digits_to_words(prec1);
    let mut len1 = i + digits_to_words(2 * frac2 + frac_incr + 1) + 1;
    if len1 < 3 {
        len1 = 3;
    }

    // Go make([]int32, len1) panics for a negative len1; so does the huge usize here.
    let mut tmp1 = vec![0i32; len1 as usize];
    {
        // Go: copy(tmp1, from1.wordBuf[idx1:idx1+i]); the slice expression panics like Go's.
        let src = &from1.word_buf[idx1 as usize..(idx1 + i) as usize];
        let n = tmp1.len().min(src.len());
        tmp1[..n].copy_from_slice(&src[..n]);
    }

    let mut start1: isize = 0;
    let mut stop1;
    let start2 = idx2;
    let mut stop2 = idx2 + digits_to_words(prec2) - 1;

    /* removing end zeroes */
    while from2.word_buf[stop2 as usize] == 0 && stop2 >= start2 {
        stop2 -= 1;
    }
    let len2 = stop2 - start2;
    stop2 += 1;

    /*
       calculating norm2 (normalized from2.wordBuf[start2]) - we need from2.wordBuf[start2] to be large
       (at least > DIG_BASE/2), but unlike Knuth's Alg. D we don't want to
       normalize input numbers (as we don't make a copy of the divisor).
       Thus we normalize first dec1 of buf2 only, and we'll normalize tmp1[start1]
       on the fly for the purpose of guesstimation only.
       It's also faster, as we're saving on normalization of from2.
    */
    //
    // int64 bounds (M-INT). For a valid divisor (words in [0, wb), leading word w >= 1)
    // normFactor = wb/(w+1) is in [0, wb/2]. For any int32 words (UnmarshalJSON and
    // from_wire do not check them, nor does Go) |normFactor| <= wb and every product
    // below is at most wb * 2^31 ~ 2.1e18 < 2^63: these two lines cannot wrap. A divisor
    // word of -1 makes w+1 == 0 and panics with an integer divide by zero, as in Go.
    let wb = i64::from(WORD_BASE);
    let norm_factor = wb / i64::from(from2.word_buf[start2 as usize].wrapping_add(1));
    let mut norm2 = (norm_factor * i64::from(from2.word_buf[start2 as usize])) as i32;
    if len2 > 0 {
        norm2 = norm2.wrapping_add(
            (norm_factor * i64::from(from2.word_buf[(start2 + 1) as usize]) / wb) as i32,
        );
    }
    let mut dcarry: i32 = 0;
    if tmp1[start1 as usize] < from2.word_buf[start2 as usize] {
        dcarry = tmp1[start1 as usize];
        start1 += 1;
    }

    // main loop
    let mut guess: i64;
    while idx_to < stop_to {
        /* short-circuit, if possible */
        if dcarry == 0 && tmp1[start1 as usize] < from2.word_buf[start2 as usize] {
            guess = 0;
        } else {
            /* D3: make a guess */
            // int64 bounds for valid words (w = from2.wordBuf[start2] in [1, wb)):
            // - x = tmp1[start1] + dcarry*wb < wb^2 = 1e18. The partial remainder is below
            //   the divisor, so dcarry <= w and x < (w+1)*wb, hence normFactor*x <= wb^2;
            //   normFactor*y/wb < 5e17; the sum stays below 1.5e18.
            // - guess is clamped to [0, wb); guess*w and wordBuf[start2+1]*guess < 1e18.
            // - Knuth's D3 guess is at most 2 above the true digit (TAOCP 4.3.1, Thm B), so
            //   x - guess*w is in (-2w, 2wb) and (x - guess*w)*wb + y in (-2e18, 2e18).
            // Invalid words (unchecked by UnmarshalJSON and from_wire, as in Go) can push
            // these past 2^63, where Go's int64 arithmetic wraps: wrapping_* keeps that.
            let mut x = i64::from(tmp1[start1 as usize]).wrapping_add(i64::from(dcarry) * wb);
            let y = i64::from(tmp1[(start1 + 1) as usize]);
            guess = norm_factor
                .wrapping_mul(x)
                .wrapping_add(norm_factor * y / wb)
                .wrapping_div(i64::from(norm2));
            if guess >= wb {
                guess = wb - 1;
            }

            if len2 > 0 {
                /* remove normalization */
                if i64::from(from2.word_buf[(start2 + 1) as usize]).wrapping_mul(guess)
                    > x.wrapping_sub(guess.wrapping_mul(i64::from(from2.word_buf[start2 as usize])))
                        .wrapping_mul(wb)
                        .wrapping_add(y)
                {
                    guess -= 1;
                }
                if i64::from(from2.word_buf[(start2 + 1) as usize]).wrapping_mul(guess)
                    > x.wrapping_sub(guess.wrapping_mul(i64::from(from2.word_buf[start2 as usize])))
                        .wrapping_mul(wb)
                        .wrapping_add(y)
                {
                    guess -= 1;
                }
            }

            /* D4: multiply and subtract */
            idx2 = stop2;
            idx1 = start1 + len2;
            let mut carry: i32 = 0;
            while idx2 > start2 {
                idx2 -= 1;
                // Valid words: guess and the word are below wb, x < 1e18. With invalid
                // words guess can be any int64 (Go wraps), hence wrapping_mul.
                x = guess.wrapping_mul(i64::from(from2.word_buf[idx2 as usize]));
                let hi = (x / wb) as i32;
                let lo = (x - i64::from(hi) * wb) as i32;
                (tmp1[idx1 as usize], carry) = sub2(tmp1[idx1 as usize], lo, carry);
                carry = carry.wrapping_add(hi);
                idx1 -= 1;
            }
            if dcarry < carry {
                carry = 1;
            } else {
                carry = 0;
            }

            /* D5: check the remainder */
            if carry > 0 {
                /* D6: correct the guess */
                guess -= 1;
                idx2 = stop2;
                idx1 = start1 + len2;
                carry = 0;
                while idx2 > start2 {
                    idx2 -= 1;
                    (tmp1[idx1 as usize], carry) =
                        add(tmp1[idx1 as usize], from2.word_buf[idx2 as usize], carry);
                    idx1 -= 1;
                }
            }
        }
        if !is_mod {
            to.word_buf[idx_to as usize] = guess as i32;
        }
        dcarry = tmp1[start1 as usize];
        start1 += 1;
        idx_to += 1;
    }
    if is_mod {
        /*
           now the result is in tmp1, it has
           digitsInt=prec1-frac1
           digitsFrac=max(frac1, frac2)
        */
        if dcarry != 0 {
            start1 -= 1;
            tmp1[start1 as usize] = dcarry;
        }
        idx_to = 0;

        digits_int_to = prec1 - frac1 - start1 * DIGITS_PER_WORD;
        if digits_int_to < 0 {
            /* If leading zeroes in the fractional part were earlier stripped */
            words_int_to = digits_int_to / DIGITS_PER_WORD;
        } else {
            words_int_to = digits_to_words(digits_int_to);
        }

        words_frac_to = digits_to_words(to.digits_frac as isize);
        err = Ok(());
        if words_int_to == 0 && words_frac_to == 0 {
            *to = ZERO_MY_DECIMAL;
            return err;
        }
        if words_int_to <= 0 {
            if -words_int_to >= WORD_BUF_LEN {
                *to = ZERO_MY_DECIMAL;
                return Err(ERR_TRUNCATED);
            }
            stop1 = start1 + words_int_to + words_frac_to;
            words_frac_to += words_int_to;
            to.digits_int = 0;
            while words_int_to < 0 {
                to.word_buf[idx_to as usize] = 0;
                idx_to += 1;
                words_int_to += 1;
            }
        } else {
            if words_int_to > WORD_BUF_LEN {
                to.digits_int = (DIGITS_PER_WORD * WORD_BUF_LEN) as i8;
                to.digits_frac = 0;
                return Err(ERR_OVERFLOW);
            }
            stop1 = start1 + words_int_to + words_frac_to;
            to.digits_int = (words_int_to * DIGITS_PER_WORD).min(from2.digits_int as isize) as i8;
        }
        if words_int_to + words_frac_to > WORD_BUF_LEN {
            stop1 -= words_int_to + words_frac_to - WORD_BUF_LEN;
            words_frac_to = WORD_BUF_LEN - words_int_to;
            to.digits_frac = (words_frac_to * DIGITS_PER_WORD) as i8;
            err = Err(ERR_TRUNCATED);
        }
        while start1 < stop1 {
            to.word_buf[idx_to as usize] = tmp1[start1 as usize];
            idx_to += 1;
            start1 += 1;
        }
    }
    (idx_to, digits_int_to) = to.remove_leading_zeros();
    to.digits_int = digits_int_to as i8;
    if idx_to != 0 {
        to.word_buf.copy_within(idx_to as usize.., 0);
    }

    if to.is_zero() {
        to.negative = false;
    }
    err
}

/// DecimalPeak returns the length of the encoded decimal.
// go: DecimalPeak L1 [M-ERRVAL]
pub fn decimal_peak(b: &[u8]) -> Result<isize, Error> {
    if b.len() < 3 {
        return Err(ERR_BAD_NUMBER);
    }
    let precision = b[0] as isize;
    let frac = b[1] as isize;
    let bin_size = decimal_bin_size(precision, frac)?;
    Ok(bin_size + 2)
}

/// NewDecFromInt creates a MyDecimal from int.
// go: NewDecFromInt L1
pub fn new_dec_from_int(i: i64) -> MyDecimal {
    *MyDecimal::default().from_int(i)
}

/// NewDecFromUint creates a MyDecimal from uint.
// go: NewDecFromUint L1
pub fn new_dec_from_uint(i: u64) -> MyDecimal {
    *MyDecimal::default().from_uint(i)
}

/// NewDecFromFloatForTest creates a MyDecimal from float, as it returns no error, it should only be used in test.
// go: NewDecFromFloatForTest L1
pub fn new_dec_from_float_for_test(f: f64) -> MyDecimal {
    let mut dec = MyDecimal::default();
    if let Err(err) = dec.from_float64(f) {
        // Go: log.Panic("encountered error", zap.Error(err), zap.String("DecimalStr", ...))
        panic!(
            "encountered error: {err}, DecimalStr: {}",
            gostd::format_float_g(f)
        );
    }
    dec
}

/// NewDecFromStringForTest creates a MyDecimal from string, as it returns no error, it should only be used in test.
// go: NewDecFromStringForTest L1
pub fn new_dec_from_string_for_test(s: &str) -> MyDecimal {
    let mut dec = MyDecimal::default();
    if let Err(err) = dec.from_string(s.as_bytes()) {
        // Go: log.Panic("encountered error", zap.Error(err), zap.String("DecimalStr", s))
        panic!("encountered error: {err}, DecimalStr: {s}");
    }
    dec
}

/// NewMaxOrMinDec returns the max or min value decimal for given precision and fraction.
// go: NewMaxOrMinDec L1
pub fn new_max_or_min_dec(negative: bool, prec: isize, frac: isize) -> MyDecimal {
    // Go make([]byte, prec+2) panics for a negative length; so does the huge usize here.
    let mut str = vec![0u8; (prec + 2) as usize];
    for i in 0..str.len() {
        str[i] = b'9';
    }
    if negative {
        str[0] = b'-';
    } else {
        str[0] = b'+';
    }
    str[(1 + prec - frac) as usize] = b'.';
    let mut dec = MyDecimal::default();
    let err = dec.from_string(&str);
    terror_log(err);
    dec
}

impl std::fmt::Display for MyDecimal {
    // glue: Go's fmt.Stringer is String(); Display delegates to it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.string())
    }
}
