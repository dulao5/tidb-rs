//! go-file: pkg/structure/structure.go
//!
//! `pkg/structure`: TiDB's meta data layout over a key-value transaction. A *string*
//! is one key, a *hash* is one key per field under a common prefix, a *list* is a meta key
//! (left / right index) plus one key per element. Every key is
//! `prefix + EncodeBytes(key) + EncodeUint(type flag) [+ field / index]`.
//!
//! tidb-rs is read-only (D06.2 scoping): only the readers are ported. The writers (`Set`,
//! `Inc`, `Clear`, `HSet`, `HInc`, `HDel`, `HClear`, `LPush`, `RPush`, `LPop`, `RPop`,
//! `LSet`, `LClear`) run in Go TiDB (X, tracker rules), so [`TxStructure`] has no
//! read-writer and `ErrWriteOnSnapshot` is never returned.
//!
//! Reads are `async` (M-ASYNC: the `kv.Retriever` of tidb-kv returns futures). The
//! iteration callbacks are synchronous closures, as in Go.

#![allow(non_upper_case_globals)]

mod hash;
mod list;
mod string;
mod types;

use std::sync::Arc;
use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::dbterror::ClassStructure;
use tidb_base::errno;
use tidb_kv::Context;
use tidb_kv::error::ErrNotExist;
use tidb_kv::key::Key;
use tidb_kv::kv::Retriever;

pub use hash::HashPair;
pub use hash::ReverseHashIterator;
pub use hash::new_hash_reverse_iter;
pub use hash::new_hash_reverse_iter_begin_with_field;
pub use types::TypeFlag;

/// ErrInvalidHashKeyFlag used by structure
pub static ErrInvalidHashKeyFlag: LazyLock<Error> =
    LazyLock::new(|| ClassStructure.new_std(errno::ErrInvalidHashKeyFlag));
/// ErrInvalidListIndex used by structure
pub static ErrInvalidListIndex: LazyLock<Error> =
    LazyLock::new(|| ClassStructure.new_std(errno::ErrInvalidListIndex));
/// ErrInvalidListMetaData used by structure
pub static ErrInvalidListMetaData: LazyLock<Error> =
    LazyLock::new(|| ClassStructure.new_std(errno::ErrInvalidListMetaData));
/// ErrWriteOnSnapshot used by structure
pub static ErrWriteOnSnapshot: LazyLock<Error> =
    LazyLock::new(|| ClassStructure.new_std(errno::ErrWriteOnSnapshot));

/// TxStructure supports some simple data structures like string, hash, list, etc. and you
/// can use these in a transaction.
#[derive(Clone)]
pub struct TxStructure {
    reader: Arc<dyn Retriever>,
    prefix: Vec<u8>,
}

impl std::fmt::Debug for TxStructure {
    // glue: Debug without the reader
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxStructure")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

/// NewStructure creates a TxStructure with Retriever, RetrieverMutator and key prefix.
/// tidb-rs has no read-writer (the writers are X): this is `NewStructure(reader, nil,
/// prefix)`.
// go: NewStructure L2 (reader only)
pub fn new_structure(reader: Arc<dyn Retriever>, prefix: &[u8]) -> TxStructure {
    TxStructure {
        reader,
        prefix: prefix.to_vec(),
    }
}

impl TxStructure {
    /// `kv.GetValue(context.TODO(), t.reader, k)` with `ErrNotExist` turned into `None`
    /// (every reader of the package does that, except the list element reads).
    // glue: kv.GetValue + `if kv.ErrNotExist.Equal(err) { err = nil }`
    pub(crate) async fn get_value_opt(&self, k: Vec<u8>) -> Result<Option<Vec<u8>>, Error> {
        let ctx = Context::background();
        match tidb_kv::kv::get_value(&ctx, &*self.reader, &Key(k)).await {
            Ok(v) => Ok(Some(v)),
            Err(e) if ErrNotExist.equal(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `kv.GetValue(context.TODO(), t.reader, k)`.
    // glue: kv.GetValue
    pub(crate) async fn get_value(&self, k: Vec<u8>) -> Result<Vec<u8>, Error> {
        let ctx = Context::background();
        tidb_kv::kv::get_value(&ctx, &*self.reader, &Key(k)).await
    }
}
