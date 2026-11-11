//! go-file: pkg/structure/string.go
//!
//! String values. `Set` / `Inc` / `Clear` are X (writes).

use tidb_base::Error;
use tidb_base::gostd::strconv;
use tidb_kv::key::Key;

use super::TxStructure;

impl TxStructure {
    /// Get gets the string value of a key.
    // go: TxStructure.Get L1
    pub async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let ek = self.encode_string_data_key(key);
        self.get_value_opt(ek).await
    }

    /// GetInt64 gets the int64 value of a key. Go-shaped (M-ERRVAL): a value out of range
    /// returns the clamped value with the error.
    // go: TxStructure.GetInt64 L1
    pub async fn get_int64(&self, key: &[u8]) -> (i64, Result<(), Error>) {
        let v = match self.get(key).await {
            Err(e) => return (0, Err(e)),
            Ok(None) => return (0, Ok(())),
            Ok(Some(v)) => v,
        };
        let (n, err) = strconv::parse_int(&v, "ParseInt");
        (n, err.map_err(Error::plain))
    }

    /// Iterate iterates all the key-value pairs from key to upperBound (string data keys).
    // go: TxStructure.Iterate L1
    pub async fn iterate(
        &self,
        key: &[u8],
        upper_bound: &[u8],
        mut f: impl FnMut(&[u8], &[u8]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let data_prefix = Key(self.encode_string_data_key(key));
        let upper_bound_data_prefix = Key(self.encode_string_data_key(upper_bound));
        let mut it = self
            .reader
            .iter(&data_prefix, &upper_bound_data_prefix)
            .await?;
        while it.valid() {
            let k = self.decode_string_data_key(it.key())?;
            f(&k, it.value())?;
            it.next().await?;
        }
        Ok(())
    }
}
