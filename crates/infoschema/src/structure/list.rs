//! go-file: pkg/structure/list.go
//!
//! List values: a meta key with the left / right index, one key per element. `LPush`,
//! `RPush`, `listPush`, `LPop`, `RPop`, `listPop`, `LSet`, `LClear` and `listMeta.Value`
//! are X (writes).

use tidb_base::Error;

use super::ErrInvalidListMetaData;
use super::TxStructure;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ListMeta {
    l_index: i64,
    r_index: i64,
}

impl ListMeta {
    // go: listMeta.IsEmpty L1
    fn is_empty(self) -> bool {
        self.l_index >= self.r_index
    }
}

/// Go's `make([][]byte, 0, length)` panics when the capacity is negative or exceeds
/// `maxAlloc / 24` (a slice header is 24 bytes): "runtime error: makeslice: cap out of
/// range". Below that Go allocates the capacity; Rust reserves at most a small amount (the
/// loop fails at the first missing element long before a large list is read).
// glue: Go makeslice cap check (M-PANIC) for LGetAll's make
fn go_make_cap(length: i64) -> usize {
    const MAX_ALLOC: i64 = 1 << 48;
    if !(0..=MAX_ALLOC / 24).contains(&length) {
        panic!("runtime error: makeslice: cap out of range");
    }
    usize::try_from(length).unwrap_or(0).min(1024)
}

impl TxStructure {
    /// LGetAll gets all elements of this list in order from right to left.
    // go: TxStructure.LGetAll L1
    pub async fn l_get_all(&self, key: &[u8]) -> Result<Option<Vec<Vec<u8>>>, Error> {
        let meta_key = self.encode_list_meta_key(key);
        let meta = self.load_list_meta(meta_key).await.map_err(|(_, e)| e)?;
        if meta.is_empty() {
            return Ok(None);
        }
        // Go: int(meta.RIndex - meta.LIndex), wrapping.
        let length = meta.r_index.wrapping_sub(meta.l_index);
        let mut elements = Vec::with_capacity(go_make_cap(length));
        let mut index = meta.r_index.wrapping_sub(1);
        while index >= meta.l_index {
            let e = self
                .get_value(self.encode_list_data_key(key, index))
                .await?;
            elements.push(e);
            index = index.wrapping_sub(1);
        }
        Ok(Some(elements))
    }

    /// LLen gets the length of a list. Go-shaped: the length is returned with an error.
    // go: TxStructure.LLen L1
    pub async fn l_len(&self, key: &[u8]) -> (i64, Result<(), Error>) {
        let meta_key = self.encode_list_meta_key(key);
        match self.load_list_meta(meta_key).await {
            Ok(m) => (m.r_index.wrapping_sub(m.l_index), Ok(())),
            Err((m, e)) => (m.r_index.wrapping_sub(m.l_index), Err(e)),
        }
    }

    /// LIndex gets an element from a list by its index.
    // go: TxStructure.LIndex L1
    pub async fn l_index(&self, key: &[u8], index: i64) -> Result<Option<Vec<u8>>, Error> {
        let meta_key = self.encode_list_meta_key(key);
        let meta = self.load_list_meta(meta_key).await.map_err(|(_, e)| e)?;
        if meta.is_empty() {
            return Ok(None);
        }
        let index = adjust_index(index, meta.l_index, meta.r_index);
        if index >= meta.l_index && index < meta.r_index {
            return self
                .get_value(self.encode_list_data_key(key, index))
                .await
                .map(Some);
        }
        Ok(None)
    }

    /// The list meta; on a malformed value Go returns `listMeta{0, 0}` with the error.
    // go: TxStructure.loadListMeta L1
    async fn load_list_meta(&self, meta_key: Vec<u8>) -> Result<ListMeta, (ListMeta, Error)> {
        let v = self
            .get_value_opt(meta_key)
            .await
            .map_err(|e| (ListMeta::default(), e))?;
        let meta = ListMeta::default();
        let Some(v) = v else {
            return Ok(meta);
        };
        if v.len() != 16 {
            return Err((meta, ErrInvalidListMetaData.clone()));
        }
        Ok(ListMeta {
            l_index: i64::from_be_bytes(v[0..8].try_into().expect("8 bytes")),
            r_index: i64::from_be_bytes(v[8..16].try_into().expect("8 bytes")),
        })
    }
}

// go: adjustIndex L1
fn adjust_index(index: i64, minv: i64, maxv: i64) -> i64 {
    if index >= 0 {
        return index.wrapping_add(minv);
    }
    index.wrapping_add(maxv)
}
