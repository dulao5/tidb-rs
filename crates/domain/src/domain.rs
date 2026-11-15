//! go-file: pkg/domain/domain.go
//!
//! Schema loading of the domain (D06.5 part 1, spec §2).
//!
//! Mapping:
//! - `Domain` keeps only what schema loading reads: the store, the `InfoCache` and the
//!   reload lock `m`. Go's `do` is also the builder's `autoid.Requirement` (its `Store()`);
//!   here that is [`StoreRequirement`], a holder of the same store.
//! - `Reload` takes the timestamp from the caller ([`Domain::reload_at`]): the PoC domain's
//!   sync loop reads one TSO per round and loads both schemas at it (VS1 plan §4.1).
//!   [`Domain::reload`] is Go's `Reload` with its own TSO.
//! - The schema validator (`SchemaValidator.Update` / `Reset`), the self-version report to
//!   etcd (`SchemaSyncer.UpdateSelfVersion`) and the lease are the PoC domain's until part 2
//!   (spec §4); `Reload` returns what they need ([`ReloadResult`]).
//! - Only infoschema v2 exists (D06.3 spec §1.2): `useV2` is always true and there is no
//!   v1 / v2 switch (`shouldUseV2` returns `(true, false)` after the first load).
//! - `schemaFilter` (cross-keyspace, next-gen) and `domainutil.RepairInfo` (repair mode, a
//!   Go TiDB startup option) are not used by tidb-rs: both are always off.
//! - metrics (`infoschema_metrics.LoadSchema*`, `metrics.LoadSchemaCounter`) and failpoints
//!   are not ported.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use tidb_base::Error;
use tidb_infoschema::autoid::Requirement;
use tidb_infoschema::builder::{
    convert_charset_collate_to_lower_case_if_need, convert_old_version_utf8_to_utf8mb4_if_need,
};
use tidb_infoschema::infoschema_v2::is_special_db;
use tidb_infoschema::meta::{self, Mutator};
use tidb_infoschema::{InfoCache, InfoSchemaV2, new_builder, new_cache, new_data};
use tidb_kv::kv::{Snapshot, Storage};
use tidb_kv::version::new_version;
use tidb_model::{
    ActionSetTiFlashReplica, ActionType, ActionUpdateTiFlashReplicaStatus, DBInfo, PolicyInfo,
    ResourceGroupInfo, SchemaDiff,
};
use tidb_sessionctx::variable::tidb_vars::{SchemaCacheSize, SchemaVersionCacheLimit};

/// LoadSchemaDiffVersionGapThreshold is the threshold for version gap to reload domain by
/// loading schema diffs
pub const LOAD_SCHEMA_DIFF_VERSION_GAP_THRESHOLD: i64 = 10000;

/// fetchSchemaConcurrency controls the goroutines to load schemas, but more goroutines
/// increase the memory usage when calling json.Unmarshal(), which would cause OOM,
/// so we decrease the concurrency.
const FETCH_SCHEMA_CONCURRENCY: usize = 1;

/// client-go `transaction.RelatedSchemaChange`: the physical table IDs a diff load changed
/// and the action of each.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelatedSchemaChange {
    pub phy_tbl_ids: Vec<i64>,
    pub action_types: Vec<u64>,
}

/// `autoid.Requirement` of the builder: Go passes the domain itself (`Domain.Store`).
// glue: the domain as autoid.Requirement
pub struct StoreRequirement(pub Arc<dyn Storage>);

impl Requirement for StoreRequirement {
    // glue: interface method Requirement.Store (Domain.Store)
    fn store(&self) -> Arc<dyn Storage> {
        Arc::clone(&self.0)
    }
}

/// What `loadInfoSchema` returns: the InfoSchema, whether it came from the cache, the
/// version before loading, the tables a diff load changed (`None`: full load).
pub type LoadResult = (Arc<InfoSchemaV2>, bool, i64, Option<RelatedSchemaChange>);

/// What `Reload` hands to the schema validator and the schema syncer (the PoC domain's
/// until part 2, see the module doc).
#[derive(Clone)]
pub struct ReloadResult {
    /// The loaded (or cached) InfoSchema.
    pub is: Arc<InfoSchemaV2>,
    /// `hitCache`.
    pub hit_cache: bool,
    /// `oldSchemaVersion`.
    pub old_schema_version: i64,
    /// `changes` (`None`: a full load, which resets the validator).
    pub changes: Option<RelatedSchemaChange>,
    /// The timestamp the schema was loaded at (`SchemaValidator.Update`'s `leaseGrantTS`).
    pub version: u64,
}

/// Domain represents a storage space. Different domains can use the same database name.
/// Multiple domains can be used in parallel without synchronization.
pub struct Domain {
    store: Arc<dyn Storage>,
    requirement: Arc<dyn Requirement>,
    info_cache: InfoCache,
    /// `m`: Reload runs once at a time (held across the loads' awaits).
    m: tokio::sync::Mutex<()>,
}

/// `NewDomain`: the part schema loading needs (the store and the InfoCache).
// go: NewDomain L2 (only the store and the InfoCache: D06.5 part 1)
pub fn new_domain(store: Arc<dyn Storage>) -> Domain {
    let capacity = SchemaVersionCacheLimit.load(Ordering::SeqCst).max(0) as usize;
    let requirement: Arc<dyn Requirement> = Arc::new(StoreRequirement(Arc::clone(&store)));
    Domain {
        info_cache: new_cache(Some(Arc::clone(&requirement)), capacity),
        requirement,
        store,
        m: tokio::sync::Mutex::new(()),
    }
}

/// `mathutil.Divide2Batches`: split `total` into `batches` sizes differing by at most one.
// go-dep: util/mathutil.Divide2Batches L1
fn divide2_batches(mut total: usize, batches: usize) -> Vec<usize> {
    let mut result = Vec::with_capacity(batches);
    let quotient = total / batches;
    let mut remainder = total % batches;
    while total > 0 {
        let mut size = quotient;
        if remainder > 0 {
            size += 1;
            remainder -= 1;
        }
        result.push(size);
        total -= size;
    }
    result
}

/// shouldUseV2 decides whether to use infoschema v2.
/// When loading snapshot, infoschema should keep the same as before to avoid v1/v2 switch.
/// Otherwise, it is decided by enabledV2. Only v2 exists (module doc): `enableV2` and the
/// old schema's `IsV2` are always true.
// go: shouldUseV2 L1
pub fn should_use_v2(
    enable_v2: bool,
    old: Option<&InfoSchemaV2>,
    is_snapshot: bool,
) -> (bool, bool) {
    // case 1: no information about old
    let Some(old) = old else {
        return (enable_v2, false);
    };
    // case 2: snapshot load should keep the same as old
    let (old_is_v2, _) = tidb_infoschema::infoschema_v2::is_v2(old);
    if is_snapshot {
        return (old_is_v2, false);
    }
    // case 3: the most general case
    (enable_v2, old_is_v2 != enable_v2)
}

// go: canSkipSchemaCheckerDDL L1
pub fn can_skip_schema_checker_ddl(tp: ActionType) -> bool {
    tp == ActionUpdateTiFlashReplicaStatus || tp == ActionSetTiFlashReplica
}

impl Domain {
    // go: Domain.InfoCache L1
    pub fn info_cache(&self) -> &InfoCache {
        &self.info_cache
    }

    /// InfoSchema gets the latest information schema from domain.
    // go: Domain.InfoSchema L1
    pub fn info_schema(&self) -> Option<Arc<InfoSchemaV2>> {
        self.info_cache.get_latest()
    }

    /// Store gets KV store from domain.
    // go: Domain.Store L1
    pub fn store(&self) -> Arc<dyn Storage> {
        Arc::clone(&self.store)
    }

    /// GetSnapshotMeta gets a new snapshot meta at startTS.
    // go: Domain.GetSnapshotMeta L1
    pub fn get_snapshot_meta(&self, start_ts: u64) -> Mutator {
        let snapshot = self.store.get_snapshot(new_version(start_ts));
        meta::new_reader(snapshot)
    }

    /// GetSnapshotInfoSchema gets a snapshot information schema.
    // go: Domain.GetSnapshotInfoSchema L1
    pub async fn get_snapshot_info_schema(
        &self,
        snapshot_ts: u64,
    ) -> Result<Arc<InfoSchemaV2>, Error> {
        // if the snapshotTS is new enough, we can get infoschema directly through snapshotTS.
        if let Some(is) = self.info_cache.get_by_snapshot_ts(snapshot_ts) {
            return Ok(is);
        }
        let (is, _, _, _) = self.load_info_schema(snapshot_ts, true).await?;
        Ok(is)
    }

    /// loadInfoSchema loads infoschema at startTS.
    /// It returns:
    /// 1. the needed infoschema
    /// 2. cache hit indicator
    /// 3. currentSchemaVersion(before loading)
    /// 4. the changed table IDs if it is not full load
    /// 5. an error if any
    // go: Domain.loadInfoSchema L2 (only v2: no v1/v2 switch; schemaFilter / repair mode are off, module doc)
    pub async fn load_info_schema(
        &self,
        start_ts: u64,
        is_snapshot: bool,
    ) -> Result<LoadResult, Error> {
        let snapshot: Arc<dyn Snapshot> = self.store.get_snapshot(new_version(start_ts));
        // Using the KV timeout read feature to address the issue of potential DDL lease expiration when
        // the meta region leader is slow.
        snapshot.set_option(
            tidb_kv::option::TiKVClientReadTimeout,
            Box::new(3000u64), // 3000ms.
        );
        let m = meta::new_reader(snapshot);
        let needed_schema_version = m.get_schema_version_with_non_empty_diff().await?;
        // fetch the commit timestamp of the schema diff
        let schema_ts = match self
            .get_timestamp_for_schema_version_with_non_empty_diff(
                &m,
                needed_schema_version,
                start_ts,
            )
            .await
        {
            Ok(ts) => ts,
            Err(e) => {
                log::warn!(
                    "failed to get schema version, error: {e}, version: {needed_schema_version}"
                );
                0
            }
        };

        // `variable.SchemaCacheSize.Load() > 0`: only v2 exists, 0 is an unbounded v2 cache
        // (D06.3 spec §5.4).
        let enable_v2 = true;
        let mut current_schema_version = 0;
        let old_info_schema = self.info_cache.get_latest();
        if let Some(old) = &old_info_schema {
            current_schema_version = old.schema_meta_version();
        }
        let (_use_v2, is_v1_v2_switch) =
            should_use_v2(enable_v2, old_info_schema.as_deref(), is_snapshot);
        if let Some(is) = self.info_cache.get_by_version(needed_schema_version) {
            // Copy the infoschema V2 instance and update its ts.
            // For example, the DDL run 30 minutes ago, GC happened 10 minutes ago. If we use
            // that infoschema it would get error "GC life time is shorter than transaction
            // duration" when visiting TiKV.
            // So we keep updating the ts of the infoschema v2.
            let is = Arc::new(is.clone_and_update_ts(start_ts));

            // try to insert here as well to correct the schemaTs if previous is wrong
            // the insert method check if schemaTs is zero
            self.info_cache.insert(Arc::clone(&is), schema_ts);

            if !is_v1_v2_switch {
                return Ok((is, true, 0, None));
            }
        }

        // tryLoadSchemaDiffs is only used to diff upgrading the current latest infoschema, if:
        // 1. Not first time bootstrap loading, which needs a full load.
        // 2. It is newer than the current one, so it will be "the current one" after this function call.
        // 3. There are less 100 diffs.
        // 4. No regenerated schema diff.
        let start_time = Instant::now();
        if !is_v1_v2_switch
            && current_schema_version != 0
            && needed_schema_version > current_schema_version
            && needed_schema_version - current_schema_version
                < LOAD_SCHEMA_DIFF_VERSION_GAP_THRESHOLD
        {
            match self
                .try_load_schema_diffs(&m, current_schema_version, needed_schema_version, start_ts)
                .await
            {
                Ok((is, related_changes, diff_types)) => {
                    let is = Arc::new(is);
                    self.info_cache.insert(Arc::clone(&is), schema_ts);
                    log::info!(
                        "diff load InfoSchema success, isV2: true, currentSchemaVersion: {current_schema_version}, neededSchemaVersion: {needed_schema_version}, elapsed time: {:?}, gotSchemaVersion: {}, phyTblIDs: {:?}, actionTypes: {:?}, diffTypes: {diff_types:?}",
                        start_time.elapsed(),
                        is.schema_meta_version(),
                        related_changes.phy_tbl_ids,
                        related_changes.action_types,
                    );
                    return Ok((is, false, current_schema_version, Some(related_changes)));
                }
                // We can fall back to full load, don't need to return the error.
                Err(e) => log::error!("failed to load schema diff, error: {e}"),
            }
        }

        // full load.
        let schemas = self.fetch_all_schemas_with_tables(&m).await?;
        let policies = self.fetch_policies(&m).await?;
        let resource_groups = self.fetch_resource_groups(&m).await?;

        let data = if is_snapshot {
            // Use a NewData() to avoid adding the snapshot schema to the infoschema history.
            // Not adding snapshot schema to history can avoid such cases (see Go).
            new_data()
        } else {
            Arc::clone(&self.info_cache.data)
        };
        // sysFacHack (the DDL's session factory): tidb-rs runs no DDL.
        let mut builder = new_builder(Some(Arc::clone(&self.requirement)), None, data);
        builder
            .init_with_db_infos(schemas, policies, resource_groups, needed_schema_version)
            .await?;
        let is = Arc::new(builder.build(start_ts).await);
        log::info!(
            "full load InfoSchema success, isV2: true, currentSchemaVersion: {current_schema_version}, neededSchemaVersion: {needed_schema_version}, elapsed time: {:?}",
            start_time.elapsed()
        );

        // isV1V2Switch is never true (module doc): Go's Upsert branch is not reached.
        self.info_cache.insert(Arc::clone(&is), schema_ts);
        Ok((is, false, current_schema_version, None))
    }

    /// Returns the timestamp of a schema version, which is the commit timestamp of the
    /// schema diff. Go reads it with `store/helper`'s `GetMvccByEncodedKeyWithTS` (TiKV's
    /// MVCC debug API, D07.1, not ported); without it Go returns "cannot get store from
    /// domain" (a store that is not a `helper.Storage`), and the caller uses 0.
    // glue: stub of Domain.getTimestampForSchemaVersionWithNonEmptyDiff, blocked-by:D07.1 (store/helper GetMvccByEncodedKeyWithTS: the commit ts of the schema diff)
    async fn get_timestamp_for_schema_version_with_non_empty_diff(
        &self,
        m: &Mutator,
        version: i64,
        _start_ts: u64,
    ) -> Result<u64, Error> {
        let _ = m.encode_schema_diff_key(version);
        Err(tidb_base::errors::new("cannot get store from domain"))
    }

    // go: Domain.fetchPolicies L1
    async fn fetch_policies(&self, m: &Mutator) -> Result<Vec<PolicyInfo>, Error> {
        m.list_policies().await
    }

    // go: Domain.fetchResourceGroups L1
    async fn fetch_resource_groups(&self, m: &Mutator) -> Result<Vec<ResourceGroupInfo>, Error> {
        m.list_resource_groups().await
    }

    /// Go runs the groups on an errgroup limited to `min(groups, 128)` goroutines; here they
    /// are futures polled together (the meta reads are the I/O).
    // go: Domain.fetchAllSchemasWithTables L2 (schemaFilter is off; the groups are joined futures)
    async fn fetch_all_schemas_with_tables(&self, m: &Mutator) -> Result<Vec<DBInfo>, Error> {
        let all_schemas = m.list_databases().await?;
        if all_schemas.is_empty() {
            return Ok(Vec::new());
        }

        let splitted_schemas = self.split_for_concurrent_fetch(all_schemas);
        let fetched = futures::future::try_join_all(
            splitted_schemas
                .into_iter()
                .map(|schemas| self.fetch_schemas_with_tables(schemas, m)),
        )
        .await?;
        Ok(fetched.into_iter().flatten().collect())
    }

    // go: Domain.splitForConcurrentFetch L1
    fn split_for_concurrent_fetch(&self, schemas: Vec<DBInfo>) -> Vec<Vec<DBInfo>> {
        let mut group_cnt = FETCH_SCHEMA_CONCURRENCY;
        let schema_cnt = schemas.len();
        if SchemaCacheSize.load(Ordering::SeqCst) > 0 && schema_cnt > 1000 {
            // TODO: Temporary solution to speed up when too many databases, will refactor it later.
            group_cnt = 8;
        }

        let mut splitted = Vec::with_capacity(group_cnt);
        let mut rest = schemas.into_iter();
        for group_size in divide2_batches(schema_cnt, group_cnt) {
            splitted.push(rest.by_ref().take(group_size).collect());
        }
        splitted
    }

    /// Go fills the `DBInfo`s in place; here the group is returned filled. Go reads
    /// `variable.SchemaCacheSize` to choose between the v2 (names and must-load tables) and
    /// the v1 (all tables) fetch: v2 is always used (D06.3 spec §5.4), except for the
    /// memory databases as in Go.
    // go: Domain.fetchSchemasWithTables L2 (always the v2 fetch; repair mode is off)
    async fn fetch_schemas_with_tables(
        &self,
        mut schemas: Vec<DBInfo>,
        m: &Mutator,
    ) -> Result<Vec<DBInfo>, Error> {
        let treat_old_utf8 =
            tidb_base::config::get_global_config().treat_old_version_utf8_as_utf8mb4;
        for di in &mut schemas {
            let mut tables = if !is_special_db(&di.name.l) {
                let (name2_id, special_table_infos, err) = m
                    .get_all_name_to_id_and_the_must_loaded_table_info(di.id)
                    .await;
                err?;
                di.table_name2_id = Some(
                    name2_id
                        .into_iter()
                        .map(|(k, v)| (String::from_utf8_lossy(&k).into_owned(), v))
                        .collect(),
                );
                special_table_infos
            } else {
                m.list_tables(di.id).await?
            };
            // If TreatOldVersionUTF8AsUTF8MB4 was enable, need to convert the old version schema UTF8 charset to UTF8MB4.
            if treat_old_utf8 {
                for tb_info in &mut tables {
                    convert_old_version_utf8_to_utf8mb4_if_need(tb_info);
                }
            }
            let mut di_tables = Vec::with_capacity(tables.len());
            for mut tbl in tables {
                convert_charset_collate_to_lower_case_if_need(&mut tbl);
                di_tables.push(Arc::new(tbl));
            }
            di.deprecated.tables = Some(di_tables);
        }
        Ok(schemas)
    }

    /// tryLoadSchemaDiffs tries to only load latest schema changes.
    /// Return true if the schema is loaded successfully.
    /// Return false if the schema can not be loaded by schema diff, then we need to do full load.
    /// The second returned value is the delta updated table and partition IDs.
    // go: Domain.tryLoadSchemaDiffs L2 (schemaFilter is off)
    async fn try_load_schema_diffs(
        &self,
        m: &Mutator,
        mut used_version: i64,
        new_version: i64,
        start_ts: u64,
    ) -> Result<(InfoSchemaV2, RelatedSchemaChange, Vec<String>), Error> {
        let mut diffs: Vec<SchemaDiff> = Vec::new();
        while used_version < new_version {
            used_version += 1;
            let (diff, err) = m.get_schema_diff(used_version).await;
            err?;
            let Some(diff) = diff else {
                // Empty diff means the txn of generating schema version is committed, but the txn of `runDDLJob` is not or fail.
                // It is safe to skip the empty diff because the infoschema is new enough and consistent.
                log::info!("diff load InfoSchema get empty schema diff, version: {used_version}");
                self.info_cache.insert_empty_schema_version(used_version);
                continue;
            };
            diffs.push(diff);
        }

        let mut builder = new_builder(
            Some(Arc::clone(&self.requirement)),
            None,
            Arc::clone(&self.info_cache.data),
        );
        let latest = self
            .info_cache
            .get_latest()
            .ok_or_else(|| tidb_base::errors::new("no InfoSchema to apply the schema diffs to"))?;
        builder.init_with_old_info_schema(&latest)?;

        builder = builder.with_store(Arc::clone(&self.store));
        builder.set_delta_update_bundles();
        let mut phy_tbl_ids = Vec::with_capacity(diffs.len());
        let mut actions = Vec::with_capacity(diffs.len());
        let mut diff_types = Vec::with_capacity(diffs.len());
        for diff in &diffs {
            if diff.regenerate_schema_map {
                return Err(tidb_base::errors::new(
                    "Meets a schema diff with RegenerateSchemaMap flag",
                ));
            }
            let ids = builder.apply_diff(m, diff).await?;
            if can_skip_schema_checker_ddl(diff.type_) {
                continue;
            }
            diff_types.push(diff.type_.to_string().to_owned());
            actions.extend(std::iter::repeat_n(diff.type_.0 as u64, ids.len()));
            phy_tbl_ids.extend(ids);
        }

        let is = builder.build(start_ts).await;
        Ok((
            is,
            RelatedSchemaChange {
                phy_tbl_ids,
                action_types: actions,
            },
            diff_types,
        ))
    }

    /// Reload reloads InfoSchema at a fresh TSO.
    // go: Domain.Reload L2 (the validator / self-version report are the caller's, module doc)
    pub async fn reload(&self) -> Result<ReloadResult, Error> {
        let ver = self
            .store
            .current_version(tidb_kv::txn_scope_var::GlobalTxnScope)
            .await?;
        self.reload_at(ver.ver).await
    }

    /// `Reload` after `do.store.CurrentVersion`: the PoC domain's sync loop passes the
    /// timestamp of its own reload (module doc).
    // glue: the body of Domain.Reload after CurrentVersion (the PoC domain drives it, VS1 plan §4.1)
    pub async fn reload_at(&self, mut version: u64) -> Result<ReloadResult, Error> {
        // Lock here for only once at the same time.
        let _guard = self.m.lock().await;
        let start_time = Instant::now();

        let mut res = self.load_info_schema(version, false).await;
        if let Err(e) = &res {
            let v = get_flashback_start_ts_from_error_msg(e);
            if v != 0 {
                // use the latest available version to create domain
                version = v - 1;
                res = self.load_info_schema(version, false).await;
            }
        }
        let (is, hit_cache, old_schema_version, changes) = res?;

        // Updating the self version in etcd (SchemaSyncer.UpdateSelfVersion), resetting the
        // validator on a full load and renewing the lease (SchemaValidator.Update) are the
        // caller's (module doc).
        log::debug!(
            "reload InfoSchema, version {}, hit cache {hit_cache}, took {:?}",
            is.schema_meta_version(),
            start_time.elapsed()
        );
        Ok(ReloadResult {
            is,
            hit_cache,
            old_schema_version,
            changes,
            version,
        })
    }
}

// go: getFlashbackStartTSFromErrorMsg L1
fn get_flashback_start_ts_from_error_msg(err: &Error) -> u64 {
    let msg = err.to_string();
    let slices: Vec<&str> = msg
        .split("is in flashback progress, FlashbackStartTS is ")
        .collect();
    if slices.len() != 2 {
        return 0;
    }
    slices[1].parse::<u64>().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divide_into_batches() {
        assert_eq!(divide2_batches(0, 1), Vec::<usize>::new());
        assert_eq!(divide2_batches(5, 1), vec![5]);
        assert_eq!(divide2_batches(10, 8), vec![2, 2, 1, 1, 1, 1, 1, 1]);
        assert_eq!(divide2_batches(3, 8), vec![1, 1, 1]);
    }

    #[test]
    fn flashback_start_ts() {
        let e = tidb_base::errors::new(
            "schema version 5 is in flashback progress, FlashbackStartTS is 1234",
        );
        assert_eq!(get_flashback_start_ts_from_error_msg(&e), 1234);
        assert_eq!(
            get_flashback_start_ts_from_error_msg(&tidb_base::errors::new("x")),
            0
        );
    }

    #[test]
    fn skip_schema_checker_ddl() {
        assert!(can_skip_schema_checker_ddl(ActionSetTiFlashReplica));
        assert!(can_skip_schema_checker_ddl(
            ActionUpdateTiFlashReplicaStatus
        ));
        assert!(!can_skip_schema_checker_ddl(tidb_model::ActionCreateTable));
    }
}
