//! Go's struct copy `*x` for the model structs.
//!
//! Several model types have a Go `Clone` method that differs from a plain copy (nil slices
//! become empty ones, `PolicyInfo.Clone` dereferences its settings), and the Rust port is
//! their `Clone` impl. A Go struct copy `nt := *t` copies such fields as they are (sharing
//! the pointers), so it must not call those `Clone` impls: [`GoCopy`] is that copy.
//!
//! Go pointers that long-lived holders share (`TableInfo.Columns`, `TableInfo.Indices`) are
//! `Arc`s (spec N13): the copy shares them as Go does, and a write goes through
//! [`make_mut`], which copies a shared value with [`GoCopy`] first.

use std::collections::BTreeMap;
use std::sync::Arc;

/// Go's assignment copy of a value: structs field by field, slices / maps / pointers by
/// their elements (Rust holds them by value), leaves with `Clone`.
pub trait GoCopy {
    // glue: Go's assignment copy
    fn go_copy(&self) -> Self;
}

impl<T: GoCopy> GoCopy for Option<T> {
    // glue: Go copy of a nilable value
    fn go_copy(&self) -> Self {
        self.as_ref().map(GoCopy::go_copy)
    }
}

impl<T: GoCopy> GoCopy for Box<T> {
    // glue: Go copy of a pointer (the pointee, held by value)
    fn go_copy(&self) -> Self {
        Box::new(self.as_ref().go_copy())
    }
}

impl<T> GoCopy for Arc<T> {
    // glue: Go copy of a shared pointer (the pointer: both copies point at the same value)
    fn go_copy(&self) -> Self {
        Arc::clone(self)
    }
}

/// Copy-on-write access to a shared model value (spec N13): the value itself when this is
/// the only holder; otherwise a [`GoCopy`] of it first (a plain struct copy, so the copy
/// equals the original: the Go `Clone` methods of some types turn nil slices into empty
/// ones, which [`Arc::make_mut`] would apply). Other holders keep the old value.
// glue: N13 copy-on-write of a shared Go pointer
pub fn make_mut<T: GoCopy>(x: &mut Arc<T>) -> &mut T {
    if Arc::get_mut(x).is_none() {
        *x = Arc::new(x.as_ref().go_copy());
    }
    Arc::get_mut(x).expect("unique after copy")
}

impl<T: GoCopy> GoCopy for Vec<T> {
    // glue: Go copy of a slice (the elements, held by value)
    fn go_copy(&self) -> Self {
        self.iter().map(GoCopy::go_copy).collect()
    }
}

impl<K: Clone + Ord, V: GoCopy> GoCopy for BTreeMap<K, V> {
    // glue: Go copy of a map (the elements, held by value)
    fn go_copy(&self) -> Self {
        self.iter().map(|(k, v)| (k.clone(), v.go_copy())).collect()
    }
}

/// `GoCopy` with `Clone` for types without a Go `Clone` method of their own.
macro_rules! go_copy_leaf {
    ($($t:ty),* $(,)?) => {
        $(impl $crate::gocopy::GoCopy for $t {
            // glue: Go copy of a value without Clone method
            fn go_copy(&self) -> Self {
                self.clone()
            }
        })*
    };
}
pub(crate) use go_copy_leaf;

go_copy_leaf!(
    bool,
    i8,
    i16,
    i32,
    i64,
    isize,
    u8,
    u16,
    u32,
    u64,
    usize,
    f64,
    String,
    ()
);

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{GoCopy, make_mut};
    use crate::{ColumnInfo, IndexColumn, IndexInfo, TableInfo};

    fn table() -> TableInfo {
        TableInfo {
            columns: Some(
                (0..3)
                    .map(|i| {
                        Arc::new(ColumnInfo {
                            id: i,
                            offset: i as isize,
                            ..Default::default()
                        })
                    })
                    .collect(),
            ),
            indices: Some(vec![Arc::new(IndexInfo {
                id: 1,
                columns: None,
                ..Default::default()
            })]),
            ..Default::default()
        }
    }

    /// Go `nt := *t` shares the columns; a write to one column of the copy copies that
    /// column only, and the original does not see it (spec N13 §3).
    #[test]
    fn go_copy_shares_and_make_mut_copies_one() {
        let t = table();
        let mut nt = t.go_copy();
        make_mut(&mut nt.columns.as_mut().unwrap()[1]).offset = 7;
        let (oc, nc) = (t.columns.as_ref().unwrap(), nt.columns.as_ref().unwrap());
        assert!(Arc::ptr_eq(&oc[0], &nc[0]));
        assert!(!Arc::ptr_eq(&oc[1], &nc[1]));
        assert!(Arc::ptr_eq(&oc[2], &nc[2]));
        assert_eq!((oc[1].offset, nc[1].offset), (1, 7));
        assert!(Arc::ptr_eq(
            &t.indices.as_ref().unwrap()[0],
            &nt.indices.as_ref().unwrap()[0]
        ));
    }

    /// The copy of `make_mut` is Go's struct copy, not Go's `IndexInfo.Clone` (which turns
    /// nil `Columns` into an empty slice, so the JSON would change from `null` to `[]`).
    #[test]
    fn make_mut_copies_with_go_copy() {
        let t = table();
        let mut nt = t.go_copy();
        make_mut(&mut nt.indices.as_mut().unwrap()[0]).id = 9;
        let ni = &nt.indices.as_ref().unwrap()[0];
        assert!(ni.columns.is_none());
        assert_eq!(t.indices.as_ref().unwrap()[0].id, 1);
        // Unshared: in place.
        let p = Arc::as_ptr(&nt.indices.as_ref().unwrap()[0]);
        make_mut(&mut nt.indices.as_mut().unwrap()[0]).id = 10;
        assert_eq!(Arc::as_ptr(&nt.indices.as_ref().unwrap()[0]), p);
    }

    /// `MoveColumnInfo` on a copy of a shared `TableInfo` copies only the columns and the
    /// indices whose offsets change; the other indices stay shared (Lucky's review of #230).
    #[test]
    fn move_column_info_copies_only_changed_indices() {
        let idx = |id: i64, off: isize| {
            Arc::new(IndexInfo {
                id,
                columns: Some(vec![IndexColumn {
                    offset: off,
                    ..Default::default()
                }]),
                ..Default::default()
            })
        };
        let mut t = table();
        t.columns.as_mut().unwrap().push(Arc::new(ColumnInfo {
            id: 3,
            offset: 3,
            ..Default::default()
        }));
        t.indices = Some(vec![idx(1, 0), idx(2, 3), idx(3, 1)]);
        let mut nt = t.go_copy();
        nt.move_column_info(1, 2);
        let (oi, ni) = (t.indices.as_ref().unwrap(), nt.indices.as_ref().unwrap());
        assert!(Arc::ptr_eq(&oi[0], &ni[0]));
        assert!(Arc::ptr_eq(&oi[1], &ni[1]));
        assert!(!Arc::ptr_eq(&oi[2], &ni[2]));
        let off = |i: &IndexInfo| i.columns.as_ref().unwrap()[0].offset;
        assert_eq!((off(&oi[2]), off(&ni[2])), (1, 2));
        let (oc, nc) = (t.columns.as_ref().unwrap(), nt.columns.as_ref().unwrap());
        assert!(Arc::ptr_eq(&oc[0], &nc[0]));
        assert!(Arc::ptr_eq(&oc[3], &nc[3]));
        assert_eq!(
            (nc[1].id, nc[1].offset, nc[2].id, nc[2].offset),
            (2, 1, 1, 2)
        );
        assert_eq!((oc[1].id, oc[1].offset), (1, 1));
    }

    /// Go `TableInfo.Clone` clones every column and index: nothing is shared.
    #[test]
    fn table_info_clone_is_deep() {
        let t = table();
        let c = t.clone();
        for (a, b) in t.columns.iter().flatten().zip(c.columns.iter().flatten()) {
            assert!(!Arc::ptr_eq(a, b));
            assert_eq!(a, b);
        }
        for (a, b) in t.indices.iter().flatten().zip(c.indices.iter().flatten()) {
            assert!(!Arc::ptr_eq(a, b));
        }
    }
}
