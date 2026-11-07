//! go-file: pkg/util/collate/ucadata/data.go
//!
//! The UCA tables of `unicode_ci` (UCA 4.0.0) and `utf8mb4_0900_ai_ci` (UCA 9.0.0), generated
//! by the tracker's `rsgen` from the Go tables (`ucadata/*_generated.go`).

#![allow(non_upper_case_globals)]

#[rustfmt::skip]
mod unicode_0900_ai_ci_data;
#[rustfmt::skip]
mod unicode_ci_data;

pub use unicode_0900_ai_ci_data::DUCET0900Table;
pub use unicode_ci_data::DUCET0400Table;

/// LongRune8 means the rune has at most 8 collation elements
pub const LongRune8: u64 = 0xFFFD;

/// The type of `DUCET0400Table` / `DUCET0900Table`: Go's
/// `struct { MapTable4 [N]uint64; LongRuneMap map[rune][2]uint64 }`.
#[derive(Debug)]
pub struct DucetTable {
    /// `MapTable4`: up to four 16-bit collation elements per rune, or `LongRune8`.
    pub map_table4: &'static [u64],
    /// `LongRuneMap`, sorted by rune.
    pub long_rune_map: &'static [(u32, [u64; 2])],
}

impl DucetTable {
    /// `LongRuneMap[r]`: the zero value for a rune without an entry, like a Go map.
    // glue: Go map index
    #[inline]
    pub fn long_rune(&self, r: u32) -> [u64; 2] {
        match self.long_rune_map.binary_search_by_key(&r, |e| e.0) {
            Ok(i) => self.long_rune_map[i].1,
            Err(_) => [0, 0],
        }
    }
}
