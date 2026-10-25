//! Datum comparison and grouping keys with the column's collation (Go: `chunk.Compare` /
//! `codec.HashChunkRow` with `collate.GetCollator`).

use std::cmp::Ordering;

use tidb_distsql::Datum;
use tidb_distsql::dag::collation_id;
use tidb_meta::model::FieldType;
use tidb_mysql_protocol::SqlError;
use tidb_query_datatype::Collation;
use tidb_query_datatype::codec::collation::Collator;
use tidb_query_datatype::codec::collation::collator::*;

/// How strings of one column compare.
#[derive(Clone, Copy, Debug)]
pub struct StrCmp(Collation);

macro_rules! with_collator {
    ($c:expr, $C:ident => $body:expr) => {
        match $c {
            Collation::Binary => {
                type $C = CollatorBinary;
                $body
            }
            Collation::Utf8Mb4Bin => {
                type $C = CollatorUtf8Mb4Bin;
                $body
            }
            Collation::Utf8Mb4BinNoPadding | Collation::Utf8Mb40900Bin => {
                type $C = CollatorUtf8Mb4BinNoPadding;
                $body
            }
            Collation::Utf8Mb4GeneralCi => {
                type $C = CollatorUtf8Mb4GeneralCi;
                $body
            }
            Collation::Utf8Mb4UnicodeCi => {
                type $C = CollatorUtf8Mb4UnicodeCi;
                $body
            }
            Collation::Utf8Mb40900AiCi => {
                type $C = CollatorUtf8Mb40900AiCi;
                $body
            }
            Collation::Latin1Bin => {
                type $C = CollatorLatin1Bin;
                $body
            }
            Collation::GbkBin => {
                type $C = CollatorGbkBin;
                $body
            }
            Collation::GbkChineseCi => {
                type $C = CollatorGbkChineseCi;
                $body
            }
            Collation::Gb18030Bin => {
                type $C = CollatorGb18030Bin;
                $body
            }
            Collation::Gb18030ChineseCi => {
                type $C = CollatorGb18030ChineseCi;
                $body
            }
        }
    };
}

impl StrCmp {
    /// The collation of a column type. With new collations (the default since v4.0 clusters
    /// were bootstrapped with `new_collation_enabled`), ids are looked up negated like Go's
    /// `collate.GetCollator`; without, every non-binary collation compares bytes.
    pub fn of(ft: &FieldType, new_collation: bool) -> Result<StrCmp, SqlError> {
        let name = if ft.collate.is_empty() {
            "binary"
        } else {
            &ft.collate
        };
        let id = collation_id(name)
            .ok_or_else(|| SqlError::not_supported(format!("collation {name}")))?;
        let id = if new_collation { -id } else { id };
        let c = Collation::from_i32(id)
            .map_err(|_| SqlError::not_supported(format!("collation {name}")))?;
        Ok(StrCmp(c))
    }

    pub fn compare(self, a: &[u8], b: &[u8]) -> Result<Ordering, SqlError> {
        with_collator!(self.0, C => C::sort_compare(a, b, false)).map_err(codec_err)
    }

    pub fn sort_key(self, a: &[u8]) -> Result<Vec<u8>, SqlError> {
        with_collator!(self.0, C => C::sort_key(a)).map_err(codec_err)
    }
}

fn codec_err(e: impl std::fmt::Display) -> SqlError {
    SqlError::unknown(format!("collation error: {e}"))
}

/// Compares two non-NULL-or-NULL datums of the same column; NULL sorts first, as in Go.
pub fn compare(a: &Datum, b: &Datum, s: StrCmp) -> Result<Ordering, SqlError> {
    use Datum::*;
    Ok(match (a, b) {
        (Null, Null) => Ordering::Equal,
        (Null, _) => Ordering::Less,
        (_, Null) => Ordering::Greater,
        (I64(x), I64(y)) => x.cmp(y),
        (U64(x), U64(y)) => x.cmp(y),
        (I64(x), U64(y)) => i128::from(*x).cmp(&i128::from(*y)),
        (U64(x), I64(y)) => i128::from(*x).cmp(&i128::from(*y)),
        (F64(x), F64(y)) => x.total_cmp(y),
        (Dec(x), Dec(y)) => x.cmp(y),
        (Bytes(x), Bytes(y)) => s.compare(x, y)?,
        (Time(x), Time(y)) => x.cmp(y),
        (Dur(x), Dur(y)) => x.cmp(y),
        (x, y) => {
            return Err(SqlError::unknown(format!(
                "cannot compare datums {x:?} and {y:?}"
            )));
        }
    })
}

/// Appends a grouping key for `d`: equal keys iff the values are equal under the column's
/// comparison (collation for strings, numeric value for numbers).
pub fn push_group_key(key: &mut Vec<u8>, d: &Datum, s: StrCmp) -> Result<(), SqlError> {
    use Datum::*;
    let mut tagged = |tag: u8, bytes: &[u8]| {
        key.push(tag);
        key.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        key.extend_from_slice(bytes);
    };
    match d {
        Null => tagged(0, &[]),
        I64(v) => tagged(1, &i128::from(*v).to_be_bytes()),
        U64(v) => tagged(1, &i128::from(*v).to_be_bytes()),
        F64(v) => {
            // -0.0 == 0.0
            let v = if *v == 0.0 { 0.0 } else { *v };
            tagged(2, &v.to_bits().to_be_bytes())
        }
        // Decimals with different scales (1.0 / 1.00) must group together.
        Dec(v) => tagged(3, normalize_decimal(&v.to_string()).as_bytes()),
        Bytes(b) => tagged(4, &s.sort_key(b)?),
        Time(t) => tagged(5, t.to_string().as_bytes()),
        Dur(t) => tagged(6, &t.to_nanos().to_be_bytes()),
        other => {
            return Err(SqlError::not_supported(format!(
                "grouping by value {other:?}"
            )));
        }
    }
    Ok(())
}

fn normalize_decimal(s: &str) -> String {
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    };
    if s == "-0" {
        "0".to_owned()
    } else {
        s.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use tidb_meta::model::tp;

    use super::*;

    fn str_ft(collate: &str) -> FieldType {
        FieldType {
            tp: tp::STRING,
            charset: "utf8mb4".into(),
            collate: collate.into(),
            ..FieldType::default()
        }
    }

    #[test]
    fn collations() {
        let bin = StrCmp::of(&str_ft("utf8mb4_bin"), true).unwrap();
        assert_eq!(bin.compare(b"a", b"a  ").unwrap(), Ordering::Equal);
        assert_eq!(bin.compare(b"B", b"a").unwrap(), Ordering::Less);
        let ci = StrCmp::of(&str_ft("utf8mb4_general_ci"), true).unwrap();
        assert_eq!(ci.compare(b"B", b"a").unwrap(), Ordering::Greater);
        assert_eq!(ci.sort_key(b"A").unwrap(), ci.sort_key(b"a ").unwrap());
        let raw = StrCmp::of(&str_ft("binary"), true).unwrap();
        assert_eq!(raw.compare(b"a", b"a ").unwrap(), Ordering::Less);
        // Old collation framework: utf8mb4_general_ci compares bytes.
        let old = StrCmp::of(&str_ft("utf8mb4_general_ci"), false).unwrap();
        assert_eq!(old.compare(b"B", b"a").unwrap(), Ordering::Less);
    }

    #[test]
    fn datum_order_and_keys() {
        let s = StrCmp::of(&str_ft("utf8mb4_bin"), true).unwrap();
        assert_eq!(
            compare(&Datum::Null, &Datum::I64(i64::MIN), s).unwrap(),
            Ordering::Less
        );
        assert_eq!(
            compare(&Datum::I64(-1), &Datum::U64(u64::MAX), s).unwrap(),
            Ordering::Less
        );
        let key = |d: &Datum| {
            let mut k = Vec::new();
            push_group_key(&mut k, d, s).unwrap();
            k
        };
        let dec = |t: &str| Datum::Dec(t.parse().unwrap());
        assert_eq!(key(&dec("1.50")), key(&dec("1.5")));
        assert_eq!(key(&dec("2.000")), key(&dec("2")));
        assert_ne!(key(&dec("20")), key(&dec("2")));
        assert_eq!(
            key(&Datum::Bytes(b"x ".to_vec())),
            key(&Datum::Bytes(b"x".to_vec()))
        );
        assert_ne!(key(&Datum::Null), key(&Datum::Bytes(Vec::new())));
        assert_eq!(key(&Datum::I64(5)), key(&Datum::U64(5)));
    }
}
