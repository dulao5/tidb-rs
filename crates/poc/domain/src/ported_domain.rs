//! The ported domain (`tidb-domain`, block D06.5 part 1) driven by this crate's sync loop
//! (VS1 plan §4.1): every reload of the PoC schema also reloads the ported domain's
//! InfoSchema at the same timestamp, so both see the same schema version. The new stack's
//! sessions plan with the ported InfoSchema.
//!
//! The ported store driver has no `kv.Storage` and no snapshot iterators yet (D07.1 part 2),
//! so [`PocStorage`] is a `kv.Storage` over `poc-kv`: point gets, batch gets and forward
//! iterators (a buffered scan, enough for the meta hashes) at a timestamp. It is glue that
//! goes away with D07.1.

use std::collections::HashMap;
use std::sync::Arc;

use poc_kv::{KeyRange, KvClient, KvPair};
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_kv::Context;
use tidb_kv::error::ErrNotExist;
use tidb_kv::key::Key;
use tidb_kv::kv::{
    BatchGetOption, BatchGetter, Client, GetOption, Getter, Iterator as KvIterator, KvFuture,
    OptionValue, Retriever, Snapshot, Storage, Transaction, TxnOption, ValueEntry, new_value_entry,
};
use tidb_kv::version::{Version, new_version};

fn kv_err(e: poc_kv::Error) -> Error {
    tidb_base::errors::new(e.to_string())
}

/// `kv.Storage` over `poc-kv` (module doc).
pub struct PocStorage {
    kv: KvClient,
}

impl PocStorage {
    pub fn new(kv: KvClient) -> PocStorage {
        PocStorage { kv }
    }
}

impl Storage for PocStorage {
    fn begin<'a>(&'a self, _: &'a [TxnOption]) -> KvFuture<'a, Box<dyn Transaction>> {
        Box::pin(async {
            Err(blocked(
                "D07.1",
                "transactions of the ported domain's store",
            ))
        })
    }
    fn get_snapshot(&self, ver: Version) -> Arc<dyn Snapshot> {
        Arc::new(PocSnapshot {
            kv: self.kv.clone(),
            ts: ver.ver,
        })
    }
    /// Schema loading sends no coprocessor requests; a client that answers every request
    /// with an error, so a caller that does reach it fails instead of panicking.
    fn get_client(&self) -> Arc<dyn Client> {
        Arc::new(NoCoprClient)
    }
    fn close(&self) -> Result<(), Error> {
        Ok(())
    }
    fn uuid(&self) -> String {
        "poc-kv".to_owned()
    }
    fn current_version<'a>(&'a self, _: &'a str) -> KvFuture<'a, Version> {
        Box::pin(async move { Ok(new_version(self.kv.current_ts().await.map_err(kv_err)?)) })
    }
    fn name(&self) -> String {
        "poc-kv".to_owned()
    }
    fn describe(&self) -> String {
        "the PoC's TiKV client as kv.Storage (until the store driver's)".to_owned()
    }
    fn get_min_safe_ts(&self, _: &str) -> u64 {
        0
    }
}

/// The coprocessor client of [`PocStorage`]: every request fails.
struct NoCoprClient;

impl Client for NoCoprClient {
    fn send(
        &self,
        _: &Context,
        _: &tidb_kv::kv::Request,
        _: Option<OptionValue>,
        _: &tidb_kv::kv::ClientSendOption,
    ) -> Box<dyn tidb_kv::kv::Response> {
        Box::new(FailedResponse)
    }
    fn is_request_type_supported(&self, _: i64, _: i64) -> bool {
        false
    }
}

/// The response of [`NoCoprClient`]: its first `Next` is the error.
struct FailedResponse;

impl tidb_kv::kv::Response for FailedResponse {
    fn next<'a>(
        &'a mut self,
        _: &'a Context,
    ) -> KvFuture<'a, Option<Box<dyn tidb_kv::kv::ResultSubset>>> {
        Box::pin(async {
            Err(blocked(
                "D07.1",
                "coprocessor requests through the ported domain's store",
            ))
        })
    }
    fn close(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// A snapshot at `ts`: each read is a `poc-kv` read at that timestamp.
struct PocSnapshot {
    kv: KvClient,
    ts: u64,
}

impl Getter for PocSnapshot {
    fn get<'a>(
        &'a self,
        _: &'a Context,
        k: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        Box::pin(async move {
            match self.kv.snapshot(self.ts).get(k.0.clone()).await {
                Ok(Some(v)) => Ok(new_value_entry(v, 0)),
                Ok(None) => Err(ErrNotExist.clone()),
                Err(e) => Err(kv_err(e)),
            }
        })
    }
}

impl BatchGetter for PocSnapshot {
    fn batch_get<'a>(
        &'a self,
        _: &'a Context,
        keys: &'a [Key],
        _: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
        Box::pin(async move {
            let keys = keys.iter().map(|k| k.0.clone()).collect();
            let pairs = self
                .kv
                .snapshot(self.ts)
                .batch_get(keys)
                .await
                .map_err(kv_err)?;
            Ok(pairs
                .into_iter()
                .map(|(k, v)| (k, new_value_entry(v, 0)))
                .collect())
        })
    }
}

/// A forward iterator over buffered pairs.
struct BufIter {
    pairs: Vec<KvPair>,
    i: usize,
    key: Key,
}

impl BufIter {
    fn load(&mut self) {
        self.key = Key(self
            .pairs
            .get(self.i)
            .map(|p| p.0.clone())
            .unwrap_or_default());
    }
}

impl KvIterator for BufIter {
    fn valid(&self) -> bool {
        self.i < self.pairs.len()
    }
    fn key(&self) -> &Key {
        &self.key
    }
    fn value(&self) -> &[u8] {
        self.pairs.get(self.i).map_or(&[], |p| p.1.as_slice())
    }
    fn next(&mut self) -> KvFuture<'_, ()> {
        self.i += 1;
        self.load();
        Box::pin(async { Ok(()) })
    }
    fn close(&mut self) {}
}

impl Retriever for PocSnapshot {
    fn iter<'a: 'f, 'f>(
        &'a self,
        k: &'f Key,
        upper: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
        Box::pin(async move {
            let pairs = self
                .kv
                .snapshot(self.ts)
                .scan_all(KeyRange::new(k.0.clone(), upper.0.clone()))
                .await
                .map_err(kv_err)?;
            let mut it = BufIter {
                pairs,
                i: 0,
                key: Key(vec![]),
            };
            it.load();
            Ok(Box::new(it) as Box<dyn KvIterator + 'a>)
        })
    }
    fn iter_reverse<'a: 'f, 'f>(
        &'a self,
        _: &'f Key,
        _: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
        Box::pin(async {
            Err(blocked(
                "D07.1",
                "reverse iterators of the ported domain's store",
            ))
        })
    }
}

impl Snapshot for PocSnapshot {
    /// The read timeout (`kv.TiKVClientReadTimeout`) is not an option of `poc-kv`.
    fn set_option(&self, _: isize, _: OptionValue) {}
}

/// The ported domain over the PoC's TiKV client.
pub fn new_ported_domain(kv: KvClient) -> Arc<tidb_domain::Domain> {
    Arc::new(tidb_domain::new_domain(Arc::new(PocStorage::new(kv))))
}

/// The ported domain if the new stack is on (`SyncConfig::ported_domain`), else none: with
/// `--new-stack=off` the sync loop does exactly what it did before the new stack existed.
pub(crate) fn ported_domain_if(
    enabled: bool,
    make: impl FnOnce() -> Arc<tidb_domain::Domain>,
) -> Option<Arc<tidb_domain::Domain>> {
    enabled.then(make)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidb_kv::kv::Response as _;

    /// `--new-stack=off` (the default): no ported domain is built, so the PoC reload never
    /// waits on it.
    #[test]
    fn off_mode_creates_no_ported_domain() {
        let cfg = crate::SyncConfig::new(
            vec![],
            crate::ServerInfo::new("127.0.0.1".into(), 4000, 10080),
            Default::default(),
        );
        assert!(!cfg.ported_domain, "off by default");
        assert!(ported_domain_if(cfg.ported_domain, || unreachable!("not built")).is_none());
        let store: Arc<dyn Storage> = Arc::new(StubStore);
        assert!(ported_domain_if(true, || Arc::new(tidb_domain::new_domain(store))).is_some());
    }

    /// The store's coprocessor client fails a request instead of panicking.
    #[test]
    fn coprocessor_client_errors() {
        let c = NoCoprClient;
        assert!(!c.is_request_type_supported(0, 0));
        let mut r = FailedResponse;
        let ctx = Context::background();
        assert!(futures::executor::block_on(r.next(&ctx)).is_err());
    }

    struct StubStore;

    impl Storage for StubStore {
        fn begin<'a>(&'a self, _: &'a [TxnOption]) -> KvFuture<'a, Box<dyn Transaction>> {
            unimplemented!()
        }
        fn get_snapshot(&self, _: Version) -> Arc<dyn Snapshot> {
            unimplemented!()
        }
        fn get_client(&self) -> Arc<dyn Client> {
            Arc::new(NoCoprClient)
        }
        fn close(&self) -> Result<(), Error> {
            Ok(())
        }
        fn uuid(&self) -> String {
            String::new()
        }
        fn current_version<'a>(&'a self, _: &'a str) -> KvFuture<'a, Version> {
            unimplemented!()
        }
        fn name(&self) -> String {
            String::new()
        }
        fn describe(&self) -> String {
            String::new()
        }
        fn get_min_safe_ts(&self, _: &str) -> u64 {
            0
        }
    }
}
