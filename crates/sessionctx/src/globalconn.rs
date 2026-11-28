//! go-file: pkg/util/globalconn/globalconn.go
//!
//! Global Connection ID layout (`GCID`) and `ParseConnID` (`D00.3` file placed in
//! `tidb-sessionctx` rank 7 so `tidb-executor` (`SimpleExec`), `tidb-server`
//! (`ConnIdAllocator` / `ServerSessionManager`), and `poc-session` share it without
//! layering cycles).

use tidb_base::{Error, errors};

/// Number of bits of `serverID` for a 32-bit global connection ID.
pub const SERVER_ID_BITS_32: u32 = 11;
/// Maximum `serverID` for a 32-bit global connection ID (`2047`).
pub const MAX_SERVER_ID_32: u64 = (1 << SERVER_ID_BITS_32) - 1;
/// Number of bits of `localConnID` for a 32-bit global connection ID.
pub const LOCAL_CONN_ID_BITS_32: u32 = 20;
/// Maximum `localConnID` for a 32-bit global connection ID.
pub const MAX_LOCAL_CONN_ID_32: u64 = (1 << LOCAL_CONN_ID_BITS_32) - 1;

/// Maximum `serverID` for a 64-bit global connection ID.
pub const MAX_SERVER_ID_64: u64 = (1 << 22) - 1;
/// Number of bits of `localConnID` for a 64-bit global connection ID.
pub const LOCAL_CONN_ID_BITS_64: u32 = 40;
/// Maximum `localConnID` for a 64-bit global connection ID.
pub const MAX_LOCAL_CONN_ID_64: u64 = (1 << LOCAL_CONN_ID_BITS_64) - 1;

/// Count of reserved connection IDs for internal processes.
pub const RESERVED_COUNT: u64 = 200;

/// Global Connection ID providing unique connection IDs across the TiDB cluster.
///
/// 32-bit layout: `[serverID (11b) | localConnID (20b) | 0]`.
/// 64-bit layout: `[0 (1b) | serverID (22b) | localConnID (40b) | 1]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GCID {
    pub server_id: u64,
    pub local_conn_id: u64,
    pub is_64bits: bool,
}

impl GCID {
    /// Converts this `GCID` to a 64-bit MySQL connection ID.
    // go: GCID.ToConnID L1
    pub fn to_conn_id(self) -> u64 {
        let mut id = 0u64;
        if self.is_64bits {
            assert!(
                self.local_conn_id <= MAX_LOCAL_CONN_ID_64,
                "unexpected localConnID {} exceeds {MAX_LOCAL_CONN_ID_64}",
                self.local_conn_id
            );
            assert!(
                self.server_id <= MAX_SERVER_ID_64,
                "unexpected serverID {} exceeds {MAX_SERVER_ID_64}",
                self.server_id
            );
            id |= 0x1;
            id |= self.local_conn_id << 1;
            id |= self.server_id << 41;
        } else {
            assert!(
                self.local_conn_id <= MAX_LOCAL_CONN_ID_32,
                "unexpected localConnID {} exceeds {MAX_LOCAL_CONN_ID_32}",
                self.local_conn_id
            );
            assert!(
                self.server_id <= MAX_SERVER_ID_32,
                "unexpected serverID {} exceeds {MAX_SERVER_ID_32}",
                self.server_id
            );
            id |= self.local_conn_id << 1;
            id |= self.server_id << 21;
        }
        id
    }
}

/// Parses a `u64` connection ID into `(gcid, is_truncated)`.
///
/// `is_truncated = true` indicates that an older client truncated a 64-bit `GCID` to 32 bits.
// go: ParseConnID L1
pub fn parse_conn_id(id: u64) -> Result<(GCID, bool), Error> {
    if id & 0x8000_0000_0000_0000 > 0 {
        return Err(errors::new("unexpected connectionID exceeds int64"));
    }
    if id & 0x1 > 0 {
        // 64bits
        if id & 0xffff_ffff_0000_0000 == 0 {
            return Ok((GCID::default(), true));
        }
        return Ok((
            GCID {
                server_id: (id >> 41) & MAX_SERVER_ID_64,
                local_conn_id: (id >> 1) & MAX_LOCAL_CONN_ID_64,
                is_64bits: true,
            },
            false,
        ));
    }

    // 32bits
    if id & 0xffff_ffff_0000_0000 > 0 {
        return Err(errors::new("unexpected connectionID exceeds uint32"));
    }
    Ok((
        GCID {
            server_id: (id >> 21) & MAX_SERVER_ID_32,
            local_conn_id: (id >> 1) & MAX_LOCAL_CONN_ID_32,
            is_64bits: false,
        },
        false,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_truncation_match_go() {
        let g32 = GCID {
            server_id: 1,
            local_conn_id: 1,
            is_64bits: false,
        };
        assert_eq!(g32.to_conn_id(), (1 << 21) | (1 << 1));
        assert_eq!(g32.to_conn_id(), 2_097_154);
        assert_eq!(parse_conn_id(2_097_154).unwrap(), (g32, false));

        let g64 = GCID {
            server_id: 2048,
            local_conn_id: 42,
            is_64bits: true,
        };
        let cid64 = g64.to_conn_id();
        assert_eq!(cid64 & 1, 1);
        assert_eq!(parse_conn_id(cid64).unwrap(), (g64, false));

        // 32-bit truncated odd ID -> is_truncated = true
        assert!(parse_conn_id(99_999_999).unwrap().1);
        // Overflow int64 / uint32
        assert!(parse_conn_id(1u64 << 63).is_err());
        assert!(parse_conn_id(1u64 << 32).is_err());
    }
}
