//! go-file: pkg/store/driver/txn/batch_getter.go
//!
//! `BufferBatchGetter`: a batch get that reads a transaction's buffer, then an optional
//! middle cache, then the snapshot. Go adapts TiDB's getters to client-go's interfaces and
//! calls client-go's `transaction.BufferBatchGetter`; that client-go type is ported here as
//! [`TikvBufferBatchGetter`] (ADR 0008: client-rust has no such helper). With one error type
//! (M-ERR), TiDB's and client-go's `ErrNotExist` are the same value.

use std::collections::HashMap;

use futures::future::BoxFuture;
use tidb_base::Error;
use tidb_kv::error::{ErrNotExist, is_err_not_found};
use tidb_kv::kv::{
    BatchGetOption, BatchGetter, GetOption, Getter, ValueEntry, batch_get_to_get_options,
};
use tidb_kv::{Context, Key};

/// BatchBufferGetter is the interface for BatchGet.
#[allow(clippy::len_without_is_empty)] // Go's interface has Len only
pub trait BatchBufferGetter: Getter {
    // glue: Go interface method BatchBufferGetter.Len
    fn len(&self) -> isize;
    // glue: Go interface method BatchBufferGetter.BatchGet
    /// BatchGet gets a batch of values, keys are in bytes slice format.
    fn batch_get<'a>(
        &'a self,
        ctx: &'a Context,
        keys: &'a [Vec<u8>],
        options: &'a [BatchGetOption],
    ) -> BoxFuture<'a, Result<HashMap<Vec<u8>, ValueEntry>, Error>>;
}

/// tikvBatchGetter is the BatchGetter struct for tikv
/// In order to directly call NewBufferBatchGetter in client-go
/// We need to implement the interface (transaction.BatchGetter) in client-go for
/// tikvBatchGetter
struct TikvBatchGetter<'s> {
    tidb_batch_getter: &'s dyn BatchGetter,
}

impl TikvBatchGetter<'_> {
    // go: tikvBatchGetter.BatchGet L2 (copies the keys; Go reinterprets the slice with unsafe)
    async fn batch_get(
        &self,
        ctx: &Context,
        keys: &[Vec<u8>],
        options: &[BatchGetOption],
    ) -> Result<HashMap<Vec<u8>, ValueEntry>, Error> {
        let kv_keys: Vec<Key> = keys.iter().map(|k| Key(k.clone())).collect();
        self.tidb_batch_getter
            .batch_get(ctx, &kv_keys, options)
            .await
    }
}

/// tikvBatchBufferGetter is the BatchBufferGetter struct for tikv
/// In order to directly call NewBufferBatchGetter in client-go
/// We need to implement the interface (transaction.BatchBufferGetter) in client-go for
/// tikvBatchBufferGetter
struct TikvBatchBufferGetter<'s> {
    tidb_middle_cache: Option<&'s dyn Getter>,
    tidb_buffer: &'s dyn BatchBufferGetter,
}

impl TikvBatchBufferGetter<'_> {
    // go: tikvBatchBufferGetter.Get L1 [M-ERRVAL]
    #[allow(dead_code)] // client-go's BufferBatchGetter only calls BatchGet (Go keeps Get for the interface)
    async fn get(
        &self,
        ctx: &Context,
        k: &[u8],
        options: &[GetOption],
    ) -> Result<ValueEntry, Error> {
        let key = Key(k.to_vec());
        // Get from buffer
        let r = self.tidb_buffer.get(ctx, &key, options).await;
        let Some(middle_cache) = self.tidb_middle_cache else {
            return r.map_err(|e| {
                if is_err_not_found(&e) {
                    ErrNotExist.clone()
                } else {
                    e
                }
            });
        };
        match r {
            Ok(v) => return Ok(v),
            Err(e) if !is_err_not_found(&e) => return Err(e),
            Err(_) => {}
        }
        // Get from middle cache
        match middle_cache.get(ctx, &key, options).await {
            Ok(v) => Ok(v),
            // TiDB err NotExist to TiKV err NotExist
            // The BatchGet method in client-go will call this method
            // Therefore, the error needs to convert to TiKV's type, otherwise the error will
            // not be handled properly in client-go
            Err(_) => Err(ErrNotExist.clone()),
        }
    }

    // go: tikvBatchBufferGetter.BatchGet L1
    async fn batch_get(
        &self,
        ctx: &Context,
        keys: &[Vec<u8>],
        options: &[BatchGetOption],
    ) -> Result<HashMap<Vec<u8>, ValueEntry>, Error> {
        let mut buffer_values = self.tidb_buffer.batch_get(ctx, keys, options).await?;
        let Some(middle_cache) = self.tidb_middle_cache else {
            return Ok(buffer_values);
        };

        let get_options = batch_get_to_get_options(options);
        for key in keys {
            if !buffer_values.contains_key(key) {
                match middle_cache.get(ctx, &Key(key.clone()), &get_options).await {
                    Ok(val) => {
                        buffer_values.insert(key.clone(), val);
                    }
                    Err(err) => {
                        if is_err_not_found(&err) {
                            continue;
                        }
                        return Err(err);
                    }
                }
            }
        }
        Ok(buffer_values)
    }

    // go: tikvBatchBufferGetter.Len L1
    #[allow(dead_code)] // part of client-go's BatchBufferGetter interface; not called by BatchGet
    fn len(&self) -> isize {
        self.tidb_buffer.len()
    }
}

/// client-go `transaction.BufferBatchGetter` (txnkv/transaction/batch_getter.go).
struct TikvBufferBatchGetter<'s> {
    buffer: TikvBatchBufferGetter<'s>,
    snapshot: TikvBatchGetter<'s>,
}

impl TikvBufferBatchGetter<'_> {
    /// BatchGet gets a batch of values from the buffer, then the snapshot for the keys the
    /// buffer does not have; a key deleted in the buffer (an empty value) is not read.
    // go-l3: ADR-0008 client-go txnkv/transaction BufferBatchGetter.BatchGet
    async fn batch_get(
        &self,
        ctx: &Context,
        keys: &[Vec<u8>],
        options: &[BatchGetOption],
    ) -> Result<HashMap<Vec<u8>, ValueEntry>, Error> {
        let mut buffer_values = self.buffer.batch_get(ctx, keys, options).await?;
        if buffer_values.is_empty() {
            return self.snapshot.batch_get(ctx, keys, options).await;
        }
        let mut shrink_keys = Vec::with_capacity(keys.len().saturating_sub(buffer_values.len()));
        for key in keys {
            let Some(val) = buffer_values.get(key) else {
                shrink_keys.push(key.clone());
                continue;
            };
            // the deleted key should be removed from the result, and also no need to snapshot
            // read it again.
            if val.value.is_empty() {
                buffer_values.remove(key);
            }
        }
        let storage_values = self.snapshot.batch_get(ctx, &shrink_keys, options).await?;
        for (key, val) in storage_values {
            buffer_values.insert(key, val);
        }
        Ok(buffer_values)
    }
}

/// BufferBatchGetter is the type for BatchGet with MemBuffer.
pub struct BufferBatchGetter<'s> {
    tikv_buffer_batch_getter: TikvBufferBatchGetter<'s>,
}

/// NewBufferBatchGetter creates a new BufferBatchGetter.
// go: NewBufferBatchGetter L1
pub fn new_buffer_batch_getter<'s>(
    buffer: &'s dyn BatchBufferGetter,
    middle_cache: Option<&'s dyn Getter>,
    snapshot: &'s dyn BatchGetter,
) -> BufferBatchGetter<'s> {
    let tikv_buffer = TikvBatchBufferGetter {
        tidb_middle_cache: middle_cache,
        tidb_buffer: buffer,
    };
    let tikv_snapshot = TikvBatchGetter {
        tidb_batch_getter: snapshot,
    };
    BufferBatchGetter {
        tikv_buffer_batch_getter: TikvBufferBatchGetter {
            buffer: tikv_buffer,
            snapshot: tikv_snapshot,
        },
    }
}

impl BufferBatchGetter<'_> {
    /// BatchGet implements the BatchGetter interface.
    // go: BufferBatchGetter.BatchGet L2 (copies the keys; Go reinterprets the slice with unsafe)
    pub async fn batch_get(
        &self,
        ctx: &Context,
        keys: &[Key],
        options: &[BatchGetOption],
    ) -> Result<HashMap<Vec<u8>, ValueEntry>, Error> {
        let tikv_keys: Vec<Vec<u8>> = keys.iter().map(|k| k.0.clone()).collect();
        self.tikv_buffer_batch_getter
            .batch_get(ctx, &tikv_keys, options)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidb_kv::kv::{KvFuture, new_value_entry};

    /// A map as a buffer / middle cache / snapshot; an empty value is a deleted key.
    struct MapGetter(HashMap<Vec<u8>, Vec<u8>>);

    impl Getter for MapGetter {
        fn get<'a>(
            &'a self,
            _ctx: &'a Context,
            k: &'a Key,
            _options: &'a [GetOption],
        ) -> KvFuture<'a, ValueEntry> {
            Box::pin(async move {
                match self.0.get(&k.0) {
                    Some(v) => Ok(new_value_entry(v.clone(), 0)),
                    None => Err(ErrNotExist.clone()),
                }
            })
        }
    }

    impl BatchBufferGetter for MapGetter {
        fn len(&self) -> isize {
            self.0.len() as isize
        }
        fn batch_get<'a>(
            &'a self,
            _ctx: &'a Context,
            keys: &'a [Vec<u8>],
            _options: &'a [BatchGetOption],
        ) -> BoxFuture<'a, Result<HashMap<Vec<u8>, ValueEntry>, Error>> {
            Box::pin(async move {
                Ok(keys
                    .iter()
                    .filter_map(|k| {
                        self.0
                            .get(k)
                            .map(|v| (k.clone(), new_value_entry(v.clone(), 0)))
                    })
                    .collect())
            })
        }
    }

    impl BatchGetter for MapGetter {
        fn batch_get<'a>(
            &'a self,
            _ctx: &'a Context,
            keys: &'a [Key],
            _options: &'a [BatchGetOption],
        ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
            Box::pin(async move {
                Ok(keys
                    .iter()
                    .filter_map(|k| {
                        self.0
                            .get(&k.0)
                            .filter(|v| !v.is_empty())
                            .map(|v| (k.0.clone(), new_value_entry(v.clone(), 0)))
                    })
                    .collect())
            })
        }
    }

    fn map(kvs: &[(&str, &str)]) -> MapGetter {
        MapGetter(
            kvs.iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
                .collect(),
        )
    }

    /// Go's batch_getter_test.go TestBufferBatchGetter.
    #[tokio::test]
    async fn buffer_batch_getter() {
        let snap = map(&[("a", "a"), ("b", "b"), ("c", "c"), ("d", "d")]);
        let buffer = map(&[("a", "a1"), ("b", ""), ("e", "e")]);
        let middle = map(&[("c", "c2")]);
        let getter = new_buffer_batch_getter(&buffer, Some(&middle), &snap);
        let keys: Vec<Key> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .map(|k| Key(k.as_bytes().to_vec()))
            .collect();
        let result = getter
            .batch_get(&Context::background(), &keys, &[])
            .await
            .unwrap();
        let get = |k: &str| result.get(k.as_bytes()).map(|e| e.value.clone());
        assert_eq!(result.len(), 4);
        assert_eq!(get("a"), Some(b"a1".to_vec()));
        assert_eq!(get("c"), Some(b"c2".to_vec()));
        assert_eq!(get("d"), Some(b"d".to_vec()));
        assert_eq!(get("e"), Some(b"e".to_vec()));
        assert_eq!(get("b"), None);

        let tb = TikvBatchBufferGetter {
            tidb_middle_cache: Some(&middle),
            tidb_buffer: &buffer,
        };
        assert_eq!(tb.len(), 3);
        let ctx = Context::background();
        assert_eq!(tb.get(&ctx, b"c", &[]).await.unwrap().value, b"c2");
        assert!(is_err_not_found(
            &tb.get(&ctx, b"z", &[]).await.unwrap_err()
        ));
    }
}
