//! go-file: pkg/planner/core/rule/logical_rules.go
//!
//! The flags of the logical optimization rules, which the plan builder sets in `optFlag`
//! (the constant block of the Go file). `setPredicatePushDownFlag` and the `init` that
//! hands it to `util` are the optimizer driver's (D09.13, VS1-24).

#![allow(non_upper_case_globals)]

/// Note: The order of flags is same as the order of optRule in the list.
/// Do not mess up the order.
pub const FlagGcSubstitute: u64 = 1 << 0;
pub const FlagPruneColumns: u64 = 1 << 1;
pub const FlagStabilizeResults: u64 = 1 << 2;
pub const FlagBuildKeyInfo: u64 = 1 << 3;
pub const FlagDecorrelate: u64 = 1 << 4;
pub const FlagSemiJoinRewrite: u64 = 1 << 5;
pub const FlagEliminateAgg: u64 = 1 << 6;
pub const FlagSkewDistinctAgg: u64 = 1 << 7;
pub const FlagEliminateProjection: u64 = 1 << 8;
pub const FlagMaxMinEliminate: u64 = 1 << 9;
pub const FlagConstantPropagation: u64 = 1 << 10;
pub const FlagConvertOuterToInnerJoin: u64 = 1 << 11;
pub const FlagPredicatePushDown: u64 = 1 << 12;
pub const FlagEliminateOuterJoin: u64 = 1 << 13;
pub const FlagPartitionProcessor: u64 = 1 << 14;
pub const FlagCollectPredicateColumnsPoint: u64 = 1 << 15;
pub const FlagPushDownAgg: u64 = 1 << 16;
pub const FlagDeriveTopNFromWindow: u64 = 1 << 17;
pub const FlagPredicateSimplification: u64 = 1 << 18;
pub const FlagPushDownTopN: u64 = 1 << 19;
pub const FlagSyncWaitStatsLoadPoint: u64 = 1 << 20;
pub const FlagJoinReOrder: u64 = 1 << 21;
pub const FlagCorrelate: u64 = 1 << 22;
pub const FlagPruneColumnsAgain: u64 = 1 << 23;
pub const FlagPushDownSequence: u64 = 1 << 24;
pub const FlagResolveExpand: u64 = 1 << 25;
