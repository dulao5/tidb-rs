//! go-file: pkg/kv/iter.go

use tidb_base::Error;

use crate::kv::FnKeyCmp;
use crate::kv::Iterator;

/// NextUntil applies FnKeyCmp to each entry of the iterator until meets some condition.
/// It will stop when fn returns true, or iterator is invalid or an error occurs.
// go: NextUntil L1
pub async fn next_until(it: &mut (dyn Iterator + '_), f: FnKeyCmp<'_>) -> Result<(), Error> {
    while it.valid() && !f(it.key()) {
        it.next().await?;
    }
    Ok(())
}
