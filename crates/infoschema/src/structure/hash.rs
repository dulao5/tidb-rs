//! go-file: pkg/structure/hash.go
//!
//! Hash values: one key per field. `HSet`, `HInc`, `HDel`, `HClear`, `updateHash`,
//! `loadHashValue` (read only by those writers), `hashFieldIntegerVal` and
//! `EncodeHashAutoIDKeyValue` are X (writes).

use tidb_base::Error;
use tidb_base::gostd::strconv;
use tidb_kv::key::Key;

use super::TxStructure;

/// HashPair is the pair for (field, value) in a hash.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HashPair {
    /// The field.
    pub field: Vec<u8>,
    /// The value.
    pub value: Vec<u8>,
}

/// `kv.Key.PrefixNext` of a raw key.
// glue: kv.Key(k).PrefixNext()
fn prefix_next(k: &[u8]) -> Vec<u8> {
    Key(k.to_vec()).prefix_next().0
}

impl TxStructure {
    /// HGet gets the value of a hash field.
    // go: TxStructure.HGet L1
    pub async fn h_get(&self, key: &[u8], field: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let data_key = self.encode_hash_data_key_inner(key, field);
        self.get_value_opt(data_key).await
    }

    /// HGetInt64 gets int64 value of a hash field. Go-shaped (M-ERRVAL).
    // go: TxStructure.HGetInt64 L1
    pub async fn h_get_int64(&self, key: &[u8], field: &[u8]) -> (i64, Result<(), Error>) {
        let value = match self.h_get(key, field).await {
            Err(e) => return (0, Err(e)),
            Ok(None) => return (0, Ok(())),
            Ok(Some(v)) => v,
        };
        let (n, err) = strconv::parse_int(&value, "ParseInt");
        (n, err.map_err(Error::plain))
    }

    /// HKeys gets all the fields in a hash. Go-shaped: the fields read before an error are
    /// returned with it.
    // go: TxStructure.HKeys L1
    pub async fn h_keys(&self, key: &[u8]) -> (Vec<Vec<u8>>, Result<(), Error>) {
        let mut keys = Vec::new();
        let err = self
            .iterate_hash(key, |field, _| {
                keys.push(field.to_vec());
                Ok(())
            })
            .await;
        (keys, err)
    }

    /// HGetAll gets all the fields and values in a hash. Go-shaped: the pairs read before
    /// an error are returned with it.
    // go: TxStructure.HGetAll L1
    pub async fn h_get_all(&self, key: &[u8]) -> (Vec<HashPair>, Result<(), Error>) {
        let mut res = Vec::new();
        let err = self
            .iterate_hash(key, |field, value| {
                res.push(HashPair {
                    field: field.to_vec(),
                    value: value.to_vec(),
                });
                Ok(())
            })
            .await;
        (res, err)
    }

    /// HGetIter iterates all the fields and values in a hash.
    // go: TxStructure.HGetIter L1
    pub async fn h_get_iter(
        &self,
        key: &[u8],
        mut f: impl FnMut(HashPair) -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.iterate_hash(key, |field, value| {
            f(HashPair {
                field: field.to_vec(),
                value: value.to_vec(),
            })
        })
        .await
    }

    /// HGetLen gets the number of fields in a hash.
    // go: TxStructure.HGetLen L1
    pub async fn h_get_len(&self, key: &[u8]) -> (u64, Result<(), Error>) {
        let mut hash_len: u64 = 0;
        let err = self
            .iterate_hash(key, |_, _| {
                hash_len += 1;
                Ok(())
            })
            .await;
        (hash_len, err)
    }

    /// HGetLastN gets latest N fields and values in hash. Go-shaped: the pairs read before
    /// an error are returned with it.
    // go: TxStructure.HGetLastN L1
    pub async fn h_get_last_n(&self, key: &[u8], num: usize) -> (Vec<HashPair>, Result<(), Error>) {
        let mut res: Vec<HashPair> = Vec::with_capacity(num);
        let err = self
            .iter_reverse_hash(key, |field, value| {
                res.push(HashPair {
                    field: field.to_vec(),
                    value: value.to_vec(),
                });
                Ok(res.len() < num)
            })
            .await;
        (res, err)
    }

    /// IterateHash iterates all the fields and values in hash.
    // go: TxStructure.IterateHash L1
    pub async fn iterate_hash(
        &self,
        key: &[u8],
        mut f: impl FnMut(&[u8], &[u8]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let data_prefix = self.hash_data_key_prefix(key);
        let start = Key(data_prefix.clone());
        let end = Key(prefix_next(&data_prefix));
        let mut it = self.reader.iter(&start, &end).await?;
        while it.valid() {
            if !it.key().starts_with(&data_prefix) {
                break;
            }
            let (_, field) = self.decode_hash_data_key(it.key())?;
            f(&field, it.value())?;
            it.next().await?;
        }
        Ok(())
    }

    /// IterateHashWithBoundedKey iterates all the fields and values in hash within
    /// [hashStartKey, hashEndKey). A key that does not decode is skipped.
    // go: TxStructure.IterateHashWithBoundedKey L1
    pub async fn iterate_hash_with_bounded_key(
        &self,
        hash_start_key: &[u8],
        hash_end_key: &[u8],
        mut f: impl FnMut(&[u8], &[u8], &[u8]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let start = Key(self.hash_data_key_prefix(hash_start_key));
        let end = Key(self.hash_data_key_prefix(hash_end_key));
        let mut it = self.reader.iter(&start, &end).await?;
        while it.valid() {
            match self.decode_hash_data_key(it.key()) {
                Err(_) => {
                    it.next().await?;
                    continue;
                }
                Ok((key, field)) => f(&key, &field, it.value())?,
            }
            it.next().await?;
        }
        Ok(())
    }

    // go: TxStructure.iterReverseHash L1
    async fn iter_reverse_hash(
        &self,
        key: &[u8],
        mut f: impl FnMut(&[u8], &[u8]) -> Result<bool, Error>,
    ) -> Result<(), Error> {
        let data_prefix = self.hash_data_key_prefix(key);
        let start = Key(prefix_next(&data_prefix));
        let lower = Key(Vec::new());
        let mut it = self.reader.iter_reverse(&start, &lower).await?;
        while it.valid() {
            if !it.key().starts_with(&data_prefix) {
                break;
            }
            let (_, field) = self.decode_hash_data_key(it.key())?;
            let more = f(&field, it.value())?;
            if !more {
                return Ok(());
            }
            it.next().await?;
        }
        Ok(())
    }
}

/// ReverseHashIterator is the reverse hash iterator.
///
/// Deviation (spec §4, L2): tidb-kv's `Retriever::iter_reverse` borrows its key arguments
/// for the iterator's lifetime, so an iterator cannot outlive the function that builds its
/// start key. The Rust iterator keeps the current entry and seeks again (`IterReverse(key,
/// nil)` on the same snapshot) for each `Next`: the same entries in the same order, one
/// seek per step instead of Go's batched scan.
pub struct ReverseHashIterator<'a> {
    t: &'a TxStructure,
    /// The current entry (raw key, value) of the underlying reverse iterator.
    cur: Option<(Vec<u8>, Vec<u8>)>,
    prefix: Vec<u8>,
    done: bool,
    field: Vec<u8>,
}

impl ReverseHashIterator<'_> {
    /// The last entry whose key is < k (`IterReverse(k, nil)`, first entry).
    // glue: one step of kv.Iterator over IterReverse
    async fn seek_before(&self, k: Vec<u8>) -> Result<Option<(Vec<u8>, Vec<u8>)>, Error> {
        let start = Key(k);
        let lower = Key(Vec::new());
        let it = self.t.reader.iter_reverse(&start, &lower).await?;
        Ok(if it.valid() {
            Some((it.key().0.clone(), it.value().to_vec()))
        } else {
            None
        })
    }

    /// Next implements the Iterator Next.
    // go: ReverseHashIterator.Next L2 (re-seeks, see the type)
    pub async fn next(&mut self) -> Result<(), Error> {
        let Some((k, _)) = self.cur.take() else {
            return Ok(());
        };
        self.cur = self.seek_before(k).await?;
        // Go reads Key() without checking Valid(): an exhausted iterator's key is nil,
        // which has no (non-empty) prefix.
        let Some((k, _)) = &self.cur else {
            self.done = true;
            return Ok(());
        };
        if !k.starts_with(&self.prefix) {
            self.done = true;
            return Ok(());
        }
        let (_, field) = self.t.decode_hash_data_key(k)?;
        self.field = field;
        Ok(())
    }

    /// Valid implements the Iterator Valid.
    // go: ReverseHashIterator.Valid L1
    pub fn valid(&self) -> bool {
        self.cur.is_some() && !self.done
    }

    /// Key implements the Iterator Key. Go's first Key (before any Next) is the zero
    /// field: the iterator positions without decoding.
    // go: ReverseHashIterator.Key L1
    pub fn key(&self) -> &[u8] {
        &self.field
    }

    /// Value implements the Iterator Value.
    // go: ReverseHashIterator.Value L1
    pub fn value(&self) -> &[u8] {
        self.cur.as_ref().map_or(&[], |(_, v)| v.as_slice())
    }

    /// Close Implements the Iterator Close.
    // go: ReverseHashIterator.Close L1
    pub fn close(&mut self) {}
}

/// NewHashReverseIter creates a reverse hash iterator.
// go: NewHashReverseIter L1
pub async fn new_hash_reverse_iter<'a>(
    t: &'a TxStructure,
    key: &[u8],
) -> Result<ReverseHashIterator<'a>, Error> {
    new_hash_reverse_iter_impl(t, key, &[]).await
}

/// NewHashReverseIterBeginWithField creates a reverse hash iterator, begin with field.
// go: NewHashReverseIterBeginWithField L1
pub async fn new_hash_reverse_iter_begin_with_field<'a>(
    t: &'a TxStructure,
    key: &[u8],
    field: &[u8],
) -> Result<ReverseHashIterator<'a>, Error> {
    new_hash_reverse_iter_impl(t, key, field).await
}

// go: newHashReverseIter L1
async fn new_hash_reverse_iter_impl<'a>(
    t: &'a TxStructure,
    key: &[u8],
    field: &[u8],
) -> Result<ReverseHashIterator<'a>, Error> {
    let data_prefix = t.hash_data_key_prefix(key);
    let iter_start = if field.is_empty() {
        prefix_next(&data_prefix)
    } else {
        prefix_next(&t.encode_hash_data_key_inner(key, field))
    };
    let mut it = ReverseHashIterator {
        t,
        cur: None,
        prefix: data_prefix,
        done: false,
        field: Vec::new(),
    };
    it.cur = it.seek_before(iter_start).await?;
    Ok(it)
}
