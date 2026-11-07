//! go-file: pkg/kv/kv.go
//!
//! Only `StoreType` (N0: moved to block D00.2, the rest of `pkg/kv` is block D07.0).

/// StoreType represents the type of a store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StoreType(pub u8);

/// TiKV means the type of a store is TiKV.
pub const TiKV: StoreType = StoreType(0);
/// TiFlash means the type of a store is TiFlash.
pub const TiFlash: StoreType = StoreType(1);
/// TiDB means the type of a store is TiDB.
pub const TiDB: StoreType = StoreType(2);
/// UnSpecified means the store type is unknown (Go: an untyped constant 255).
pub const UnSpecified: StoreType = StoreType(255);

impl StoreType {
    /// Name returns the name of store type.
    // go: StoreType.Name L1
    pub fn name(self) -> &'static str {
        if self == TiFlash {
            return "tiflash";
        } else if self == TiDB {
            return "tidb";
        } else if self == TiKV {
            return "tikv";
        }
        "unspecified"
    }
}
