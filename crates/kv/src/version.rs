//! go-file: pkg/kv/version.go
//!
//! `Version.Cmp` has no caller on the read path (X).

// Go identifiers for the statics (M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::Error;

/// VersionProvider provides increasing IDs.
pub trait VersionProvider {
    // glue: Go interface method VersionProvider.CurrentVersion
    fn current_version(&self) -> Result<Version, Error>;
}

/// Version is the wrapper of KV's version.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub ver: u64,
}

/// MaxVersion is the maximum version, notice that it's not a valid version.
pub const MaxVersion: Version = Version { ver: u64::MAX };
/// MinVersion is the minimum version, it's not a valid version, too.
pub const MinVersion: Version = Version { ver: 0 };

/// NewVersion creates a new Version struct.
// go: NewVersion L1
pub fn new_version(v: u64) -> Version {
    Version { ver: v }
}
