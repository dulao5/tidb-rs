//! Go's struct copy `*x` for the model structs.
//!
//! Several model types have a Go `Clone` method that differs from a plain copy (nil slices
//! become empty ones, `PolicyInfo.Clone` dereferences its settings), and the Rust port is
//! their `Clone` impl. A Go struct copy `nt := *t` copies such fields as they are (sharing
//! the pointers), so it must not call those `Clone` impls: [`GoCopy`] is that copy.

use std::collections::BTreeMap;

/// Go's assignment copy of a value: structs field by field, slices / maps / pointers by
/// their elements (Rust holds them by value), leaves with `Clone`.
pub(crate) trait GoCopy {
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
