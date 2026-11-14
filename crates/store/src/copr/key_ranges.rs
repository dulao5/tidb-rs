//! go-file: pkg/store/copr/key_ranges.go
//!
//! `KeyRanges`: a list of key ranges whose middle part is a sub-slice of the caller's slice
//! and that may carry one extra range in front and one at the back, so that splitting the
//! ranges of a request by region does not copy them.
//!
//! Go shares the middle part by re-slicing (`r.mid[from:to]`); the port shares it through
//! an `Arc` and an offset/length window. Elements are never written after construction,
//! in Go or here.

use std::sync::Arc;

use tidb_kv::KeyRange;
use tikv_client::proto::coprocessor;

use super::tikv::sort_search;

/// The middle part of a [`KeyRanges`]: Go's `[]kv.KeyRange` window into a shared array.
#[derive(Clone, Debug, Default)]
struct Mid {
    buf: Arc<[KeyRange]>,
    off: usize,
    len: usize,
}

impl Mid {
    // glue: the window as a slice
    fn as_slice(&self) -> &[KeyRange] {
        &self.buf[self.off..self.off + self.len]
    }

    /// Go `mid[from:to]` (panics like Go when out of range).
    // glue: Go re-slicing mid[from:to]
    fn slice(&self, from: usize, to: usize) -> Mid {
        let _ = &self.as_slice()[from..to];
        Mid {
            buf: self.buf.clone(),
            off: self.off + from,
            len: to - from,
        }
    }
}

/// KeyRanges is like []kv.KeyRange, but may has extra elements at head/tail.
/// It's for avoiding alloc big slice during build copTask.
#[derive(Clone, Debug, Default)]
pub struct KeyRanges {
    pub(crate) first: Option<KeyRange>,
    mid: Mid,
    pub(crate) last: Option<KeyRange>,
}

/// NewKeyRanges constructs a KeyRanges instance.
// go: NewKeyRanges L1
pub fn new_key_ranges(ranges: Vec<KeyRange>) -> KeyRanges {
    let len = ranges.len();
    KeyRanges {
        first: None,
        mid: Mid {
            buf: ranges.into(),
            off: 0,
            len,
        },
        last: None,
    }
}

impl KeyRanges {
    /// Go `fmt.Sprintf("[%q, %q]", ran.StartKey, ran.EndKey)` per range: `kv.Key` is a
    /// `fmt.Stringer` (hex), so `%q` quotes the hex string.
    // go: KeyRanges.String L1
    pub fn string(&self) -> String {
        let mut s = String::new();
        self.r#do(|ran| {
            s += &format!(
                "[\"{}\", \"{}\"]",
                ran.start_key.string(),
                ran.end_key.string()
            );
        });
        s
    }

    /// Len returns the count of ranges.
    // go: KeyRanges.Len L1
    pub fn len(&self) -> usize {
        let mut l = 0;
        if self.first.is_some() {
            l += 1;
        }
        l += self.mid.len;
        if self.last.is_some() {
            l += 1;
        }
        l
    }

    /// `Len() == 0` (clippy's companion of [`len`](Self::len)).
    // glue: is_empty is required next to len by clippy::len_without_is_empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `&KeyRanges{first: first, mid: mid, last: last}` (Go's tests and the replay build
    /// every shape directly).
    // glue: composite literal of the unexported fields (Go tests)
    pub fn from_parts(
        first: Option<KeyRange>,
        mid: Vec<KeyRange>,
        last: Option<KeyRange>,
    ) -> KeyRanges {
        let mut r = new_key_ranges(mid);
        r.first = first;
        r.last = last;
        r
    }

    /// Whether the first and last ranges are present and the length of the middle part
    /// (the replay compares the internal layout with Go's, which `Split` and `Slice` decide).
    // glue: test accessor of the unexported fields first / mid / last
    pub fn shape(&self) -> (bool, usize, bool) {
        (self.first.is_some(), self.mid.len, self.last.is_some())
    }

    /// RefAt returns the reference at the ith position without copy.
    ///
    /// Go returns `r.last` (nil when there is no last range) for an index past the middle;
    /// dereferencing that nil is a panic, so does the port.
    // go: KeyRanges.RefAt L1
    pub fn ref_at(&self, mut i: usize) -> &KeyRange {
        if let Some(first) = &self.first {
            if i == 0 {
                return first;
            }
            i -= 1;
        }
        if i < self.mid.len {
            return &self.mid.as_slice()[i];
        }
        self.last
            .as_ref()
            .expect("invalid memory address or nil pointer dereference")
    }

    /// At returns the range at the ith position.
    // go: KeyRanges.At L1
    pub fn at(&self, i: usize) -> KeyRange {
        self.ref_at(i).clone()
    }

    /// Slice returns the sub ranges [from, to).
    // go: KeyRanges.Slice L1
    #[allow(clippy::implicit_saturating_sub)] // Go's `if from > 0 { from-- }`
    pub fn slice(&self, mut from: usize, mut to: usize) -> KeyRanges {
        let mut ran = KeyRanges::default();
        if self.first.is_some() {
            if from == 0 && to > 0 {
                ran.first = self.first.clone();
            }
            if from > 0 {
                from -= 1;
            }
            if to > 0 {
                to -= 1;
            }
        }
        if to <= self.mid.len {
            ran.mid = self.mid.slice(from, to);
        } else {
            if from <= self.mid.len {
                ran.mid = self.mid.slice(from, self.mid.len);
            }
            if from < to {
                ran.last = self.last.clone();
            }
        }
        ran
    }

    /// Do applies a functions to all ranges.
    // go: KeyRanges.Do L1
    pub fn r#do(&self, mut f: impl FnMut(&KeyRange)) {
        if let Some(first) = &self.first {
            f(first);
        }
        for r in self.mid.as_slice() {
            f(r);
        }
        if let Some(last) = &self.last {
            f(last);
        }
    }

    /// Split ranges into (left, right) by key.
    // go: KeyRanges.Split L1
    pub fn split(&self, key: &[u8]) -> (KeyRanges, KeyRanges) {
        let n = sort_search(self.len() as isize, |i| {
            let cur = self.ref_at(i);
            cur.end_key.is_empty() || cur.end_key.cmp(key) > 0
        }) as usize;
        // If a range p contains the key, it will split to 2 parts.
        if n < self.len() {
            let p = self.at(n);
            if tidb_kv::Key(key.to_vec()).cmp(&p.start_key) > 0 {
                let mut left = self.slice(0, n);
                left.last = Some(KeyRange {
                    start_key: p.start_key,
                    end_key: tidb_kv::Key(key.to_vec()),
                });
                let mut right = self.slice(n + 1, self.len());
                right.first = Some(KeyRange {
                    start_key: tidb_kv::Key(key.to_vec()),
                    end_key: p.end_key,
                });
                return (left, right);
            }
        }
        (self.slice(0, n), self.slice(n, self.len()))
    }

    /// ToRanges converts ranges to []kv.KeyRange.
    // go: KeyRanges.ToRanges L1
    pub fn to_ranges(&self) -> Vec<KeyRange> {
        let mut ranges = Vec::with_capacity(self.len());
        self.r#do(|ran| ranges.push(ran.clone()));
        ranges
    }

    /// ToPBRanges converts ranges to wire type.
    // go: KeyRanges.ToPBRanges L2 (copies the keys into the prost messages; Go reinterprets kv.KeyRange as coprocessor.KeyRange with unsafe.Pointer, same bytes)
    pub fn to_pb_ranges(&self) -> Vec<coprocessor::KeyRange> {
        let mut ranges = Vec::with_capacity(self.len());
        self.r#do(|ran| {
            ranges.push(coprocessor::KeyRange {
                start: ran.start_key.0.clone(),
                end: ran.end_key.0.clone(),
            })
        });
        ranges
    }
}
