//! go-file: pkg/kv/option.go
//!
//! Transaction option identifiers, `ReplicaReadType` and the request-source helpers.
//! The TiCDC / lossy-DDL write-source bits are the commit path (X, tracker scoping).

// Go identifiers for the option constants (M-GONAME).
#![allow(non_upper_case_globals)]

use crate::context::Context;

// Transaction options (Go: iota + 1).
/// BinlogInfo contains the binlog data and client.
pub const BinlogInfo: isize = 1;
/// SchemaChecker is used for checking schema-validity.
pub const SchemaChecker: isize = 2;
/// IsolationLevel sets isolation level for current transaction. The default level is SI.
pub const IsolationLevel: isize = 3;
/// Priority marks the priority of this transaction.
pub const Priority: isize = 4;
/// NotFillCache makes this request do not touch the LRU cache of the underlying storage.
pub const NotFillCache: isize = 5;
/// SyncLog is not used anymore.
pub const SyncLog: isize = 6;
/// KeyOnly retrieve only keys, it can be used in scan now.
pub const KeyOnly: isize = 7;
/// Pessimistic is defined for pessimistic lock
pub const Pessimistic: isize = 8;
/// SnapshotTS is defined to set snapshot ts.
pub const SnapshotTS: isize = 9;
/// ReplicaRead Set replica read
pub const ReplicaRead: isize = 10;
/// TaskID Set task ID
pub const TaskID: isize = 11;
/// InfoSchema is schema version used by txn startTS.
pub const InfoSchema: isize = 12;
/// CollectRuntimeStats is used to enable collect runtime stats.
pub const CollectRuntimeStats: isize = 13;
/// SchemaAmender is used to amend mutations for pessimistic transactions
pub const SchemaAmender: isize = 14;
/// SampleStep skips 'SampleStep - 1' number of keys after each returned key.
pub const SampleStep: isize = 15;
/// CommitHook is a callback function called right after the transaction gets committed
pub const CommitHook: isize = 16;
/// EnableAsyncCommit indicates whether async commit is enabled
pub const EnableAsyncCommit: isize = 17;
/// Enable1PC indicates whether one-phase commit is enabled
pub const Enable1PC: isize = 18;
/// GuaranteeLinearizability indicates whether to guarantee linearizability at the cost of
/// an extra tso request before prewrite
pub const GuaranteeLinearizability: isize = 19;
/// TxnScope indicates which @@txn_scope this transaction will work with.
pub const TxnScope: isize = 20;
/// ReadReplicaScope
pub const ReadReplicaScope: isize = 21;
/// IsStalenessReadOnly indicates whether the transaction is staleness read only transaction
pub const IsStalenessReadOnly: isize = 22;
/// MatchStoreLabels indicates the labels the store should be matched
pub const MatchStoreLabels: isize = 23;
/// ResourceGroupTag indicates the resource group tag of the kv request.
pub const ResourceGroupTag: isize = 24;
/// ResourceGroupTagger can be used to set the ResourceGroupTag dynamically according to
/// the request content. It will be used only when ResourceGroupTag is nil.
pub const ResourceGroupTagger: isize = 25;
/// KVFilter indicates the filter to ignore key-values in the transaction's memory buffer.
pub const KVFilter: isize = 26;
/// SnapInterceptor is used for setting the interceptor for snapshot
pub const SnapInterceptor: isize = 27;
/// CommitTSUpperBoundCheck is used by cached table
/// The commitTS must be greater than all the write lock lease of the visited cached table.
pub const CommitTSUpperBoundCheck: isize = 28;
/// RPCInterceptor is interceptor.RPCInterceptor on Transaction or Snapshot, used to
/// decorate additional logic before and after the underlying client-go RPC request.
pub const RPCInterceptor: isize = 29;
/// TableToColumnMaps is a map from tableID to a series of maps. The maps are needed when
/// checking data consistency. Save them here to reduce redundant computations.
pub const TableToColumnMaps: isize = 30;
/// AssertionLevel controls how strict the assertions on data during transactions should be.
pub const AssertionLevel: isize = 31;
/// RequestSourceInternal set request source scope of transaction.
pub const RequestSourceInternal: isize = 32;
/// RequestSourceType set request source type of the current statement.
pub const RequestSourceType: isize = 33;
/// ExplicitRequestSourceType is a complement of RequestSourceType, it may specified by the
/// client or users.
pub const ExplicitRequestSourceType: isize = 34;
/// ReplicaReadAdjuster set the adjust function of cop requests.
pub const ReplicaReadAdjuster: isize = 35;
/// ScanBatchSize set the iter scan batch size.
pub const ScanBatchSize: isize = 36;
/// TxnSource set the source of this transaction.
/// We use an uint64 to represent the source of a transaction.
/// The first 8 bits are reserved for TiCDC to implement BDR synchronization,
/// and the next 8 bits are reserved for Lossy DDL reorg Backfill job.
/// The remaining 48 bits are reserved for extendability.
pub const TxnSource: isize = 37;
/// ResourceGroupName set the bind resource group name.
pub const ResourceGroupName: isize = 38;
/// LoadBasedReplicaReadThreshold sets the TiKV wait duration threshold of enabling replica
/// read automatically.
pub const LoadBasedReplicaReadThreshold: isize = 39;
/// TiKVClientReadTimeout sets the timeout value for readonly kv request in milliseconds
pub const TiKVClientReadTimeout: isize = 40;
/// SizeLimits sets the size limits of membuf
pub const SizeLimits: isize = 41;
/// SessionID marks the connection id, for logging and tracing.
pub const SessionID: isize = 42;
/// BackgroundGoroutineLifecycleHooks is the hooks to track the start and end of background
/// goroutine
pub const BackgroundGoroutineLifecycleHooks: isize = 43;
/// PrewriteEncounterLockPolicy is the policy to handle lock conflict during prewrite
pub const PrewriteEncounterLockPolicy: isize = 44;

/// TxnSizeLimits is the argument type for `SizeLimits` option
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxnSizeLimits {
    pub entry: u64,
    pub total: u64,
}

/// ReplicaReadType is the type of replica to read data from
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ReplicaReadType(pub u8);

/// ReplicaReadLeader stands for 'read from leader'.
pub const ReplicaReadLeader: ReplicaReadType = ReplicaReadType(0);
/// ReplicaReadFollower stands for 'read from follower'.
pub const ReplicaReadFollower: ReplicaReadType = ReplicaReadType(1);
/// ReplicaReadMixed stands for 'read from leader and follower'.
pub const ReplicaReadMixed: ReplicaReadType = ReplicaReadType(2);
/// ReplicaReadClosest stands for 'read from leader and follower which locates with the
/// same zone'
pub const ReplicaReadClosest: ReplicaReadType = ReplicaReadType(3);
/// ReplicaReadClosestAdaptive stands for 'read from follower which locates in the same
/// zone if the response size exceeds certain threshold'
pub const ReplicaReadClosestAdaptive: ReplicaReadType = ReplicaReadType(4);
/// ReplicaReadLearner stands for 'read from learner'.
pub const ReplicaReadLearner: ReplicaReadType = ReplicaReadType(5);
/// ReplicaReadPreferLeader stands for 'read from leader and auto-turn to followers if
/// leader is abnormal'.
pub const ReplicaReadPreferLeader: ReplicaReadType = ReplicaReadType(6);

impl ReplicaReadType {
    /// IsFollowerRead checks if follower is going to be used to read data.
    // go: ReplicaReadType.IsFollowerRead L1
    pub fn is_follower_read(self) -> bool {
        self != ReplicaReadLeader
    }

    /// IsClosestRead checks whether is going to request closet store to read
    // go: ReplicaReadType.IsClosestRead L1
    pub fn is_closest_read(self) -> bool {
        self == ReplicaReadClosest
    }
}

/// GetInternalSourceType get internal source
// go: GetInternalSourceType L1
pub fn get_internal_source_type(ctx: &Context) -> String {
    match ctx.request_source() {
        None => String::new(),
        Some(v) => v.request_source_type.clone(),
    }
}

/// InternalTxnOthers is the type of requests that consume low resources.
/// This reduces the size of metrics.
pub const InternalTxnOthers: &str = "others";
/// InternalTxnGC is the type of GC txn.
pub const InternalTxnGC: &str = "gc";
/// InternalTxnBootstrap is the type of TiDB bootstrap txns.
pub const InternalTxnBootstrap: &str = InternalTxnOthers;
/// InternalTxnMeta is the type of the miscellaneous meta usage.
pub const InternalTxnMeta: &str = InternalTxnOthers;
/// InternalTxnDDL is the type of inner txns in ddl module.
pub const InternalTxnDDL: &str = "ddl";
/// InternalTxnBackfillDDLPrefix is the prefix of the types of DDL operations needs
/// backfilling.
pub const InternalTxnBackfillDDLPrefix: &str = "ddl_";
/// InternalTxnCacheTable is the type of cache table usage.
pub const InternalTxnCacheTable: &str = InternalTxnOthers;
/// InternalTxnStats is the type of statistics txn.
pub const InternalTxnStats: &str = "stats";
/// InternalTxnBindInfo is the type of bind info txn.
pub const InternalTxnBindInfo: &str = InternalTxnOthers;
/// InternalTxnSysVar is the type of sys var txn.
pub const InternalTxnSysVar: &str = InternalTxnOthers;
/// InternalTxnTelemetry is the type of telemetry.
pub const InternalTxnTelemetry: &str = InternalTxnOthers;
/// InternalTxnAdmin is the type of admin operations.
pub const InternalTxnAdmin: &str = "admin";
/// InternalTxnPrivilege is the type of privilege txn.
pub const InternalTxnPrivilege: &str = InternalTxnOthers;
/// InternalTxnTools is the type of tools usage of TiDB.
/// Do not classify different tools by now.
pub const InternalTxnTools: &str = "tools";
/// InternalTxnBR is the type of BR usage.
pub const InternalTxnBR: &str = "br";
/// InternalTxnLightning is the type of Lightning usage.
pub const InternalTxnLightning: &str = "lightning";
/// InternalTxnTrace handles the trace statement.
pub const InternalTxnTrace: &str = "Trace";
/// InternalTxnTTL is the type of TTL usage
pub const InternalTxnTTL: &str = "TTL";
/// InternalLoadData is the type of LOAD DATA usage
pub const InternalLoadData: &str = "LoadData";
/// InternalImportInto is the type of IMPORT INTO usage
pub const InternalImportInto: &str = "ImportInto";
/// InternalDistTask is the type of distributed task.
pub const InternalDistTask: &str = "DistTask";
/// InternalTimer is the type of internal timer
pub const InternalTimer: &str = "Timer";
/// InternalDDLNotifier is the type of DDL notifier
pub const InternalDDLNotifier: &str = "DDLNotifier";

/// LossyDDLColumnReorgSource: TiCDC uses 1-255 to indicate the change from a lossy DDL
/// reorg Backfill job. For now, we only use 1 for column reorg backfill job.
pub const LossyDDLColumnReorgSource: u64 = 1;
/// LightningPhysicalImportTxnSource the 17th bit is set as the txn source for Lightning
/// physical import.
pub const LightningPhysicalImportTxnSource: u64 = 1 << 16;
