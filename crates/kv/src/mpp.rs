//! go-file: pkg/kv/mpp.go
//!
//! Only `GetNewestMppVersion` (`SessionVars.ChooseMppVersion`); the rest of the file is the
//! deferred MPP scope (DEF.MPP).

// Go identifiers for the constants (M-GONAME).
#![allow(non_upper_case_globals)]

/// MppVersion indicates the mpp-version used to build mpp plan
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MppVersion(pub i64);

/// MppVersionV0 supports TiFlash version [~, ~]
pub const MppVersionV0: MppVersion = MppVersion(0);
/// MppVersionV1 supports TiFlash version [v6.6.x, ~]
pub const MppVersionV1: MppVersion = MppVersion(1);
/// MppVersionV2 supports TiFlash version [v7.3, ~], support ReportMPPTaskStatus service
pub const MppVersionV2: MppVersion = MppVersion(2);
const mppVersionMax: MppVersion = MppVersion(3);
const newestMppVersion: MppVersion = MppVersion(mppVersionMax.0 - 1);
/// MppVersionUnspecified means the illegal or unspecified version, it only used in TiDB.
pub const MppVersionUnspecified: MppVersion = MppVersion(-1);
/// MppVersionUnspecifiedName denotes name of UNSPECIFIED mpp version
pub const MppVersionUnspecifiedName: &str = "UNSPECIFIED";

/// GetNewestMppVersion returns the mpp-version can be used in mpp plan
// go: GetNewestMppVersion L1
pub fn get_newest_mpp_version() -> MppVersion {
    newestMppVersion
}
