//! go-file: pkg/util/globalconn/globalconn.go
//! go-file: pkg/util/globalconn/pool.go
//!
//! Server connection ID allocators (`SimpleAllocator` and `GlobalAllocator`).

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use tidb_sessionctx::globalconn::{
    GCID, MAX_LOCAL_CONN_ID_32, MAX_LOCAL_CONN_ID_64, MAX_SERVER_ID_32, RESERVED_COUNT,
    parse_conn_id,
};

/// Allocates and recycles MySQL protocol connection IDs.
// go: globalconn.Allocator L1
pub trait ConnIdAllocator: Send + Sync {
    /// Returns the next unique connection ID.
    // glue: interface method Allocator.NextID
    fn next_id(&self) -> u64;
    /// Returns a released connection ID back to the allocator pool.
    // glue: interface method Allocator.Release
    fn release(&self, connection_id: u64);
    /// Returns a reserved internal connection ID.
    // glue: interface method Allocator.GetReservedConnID
    fn get_reserved_conn_id(&self, reserved_no: u64) -> u64;
}

/// Simple auto-incrementing connection ID allocator when global kill is disabled.
// go: SimpleAllocator L1
pub struct SimpleAllocator {
    next_id: AtomicU64,
}

impl SimpleAllocator {
    /// Creates a new `SimpleAllocator` whose first issued ID is `1`.
    // go: NewSimpleAllocator L1
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(0),
        }
    }
}

impl Default for SimpleAllocator {
    // glue: Default for SimpleAllocator
    fn default() -> Self {
        Self::new()
    }
}

impl ConnIdAllocator for SimpleAllocator {
    // go: SimpleAllocator.NextID L1
    fn next_id(&self) -> u64 {
        let cap = u64::MAX - RESERVED_COUNT;
        let raw = self.next_id.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        raw % cap
    }

    // go: SimpleAllocator.Release L1
    fn release(&self, _connection_id: u64) {}

    // go: SimpleAllocator.GetReservedConnID L1
    fn get_reserved_conn_id(&self, reserved_no: u64) -> u64 {
        assert!(
            reserved_no < RESERVED_COUNT,
            "invalid reservedNo exceed {RESERVED_COUNT}"
        );
        u64::MAX - reserved_no
    }
}

/// FIFO pool matching Go's `LockFreeCircularPool` semantics without pre-allocating 8 MiB upfront.
struct LocalPool32 {
    next_fresh: u64,
    max_id: u64,
    recycled: VecDeque<u64>,
    in_use: HashSet<u64>,
}

impl LocalPool32 {
    // glue: equivalent to LockFreeCircularPool.InitExt(1 << 20, math.MaxUint32)
    fn new(max_id: u64) -> Self {
        Self {
            next_fresh: 1,
            max_id,
            recycled: VecDeque::new(),
            in_use: HashSet::new(),
        }
    }

    fn get(&mut self) -> Option<u64> {
        let id = if self.next_fresh <= self.max_id {
            let id = self.next_fresh;
            self.next_fresh += 1;
            id
        } else {
            self.recycled.pop_front()?
        };
        self.in_use.insert(id);
        Some(id)
    }

    fn put(&mut self, val: u64) -> bool {
        if !self.in_use.remove(&val) {
            return false;
        }
        self.recycled.push_back(val);
        true
    }

    fn free_len(&self) -> usize {
        (self.max_id as usize).saturating_sub(self.in_use.len())
    }

    fn cap(&self) -> usize {
        self.max_id as usize
    }
}

struct AutoIncPool64 {
    last_id: u64,
    cap: u64,
    existed: HashSet<u64>,
}

impl AutoIncPool64 {
    fn new(cap: u64) -> Self {
        Self {
            last_id: 0,
            cap,
            existed: HashSet::new(),
        }
    }

    fn get(&mut self) -> Option<u64> {
        const TRIES: usize = 10;
        for _ in 0..TRIES {
            self.last_id = self.last_id.wrapping_add(1) % self.cap;
            if self.existed.contains(&self.last_id) {
                continue;
            }
            self.existed.insert(self.last_id);
            return Some(self.last_id);
        }
        None
    }

    fn put(&mut self, id: u64) {
        self.existed.remove(&id);
    }
}

/// Cluster-unique connection ID allocator embedding `server_id`.
///
/// Standalone `server_id = 1` produces first connection ID `2097154` (`(1 << 21) | (1 << 1)`).
// go: GlobalAllocator L2 (lazy 32-bit pool avoids 8 MiB upfront allocation while matching Go ID sequence)
pub struct GlobalAllocator {
    is_64bits: AtomicBool,
    server_id_getter: Arc<dyn Fn() -> u64 + Send + Sync>,
    local32: Mutex<LocalPool32>,
    local64: Mutex<AutoIncPool64>,
}

impl GlobalAllocator {
    /// Creates a new `GlobalAllocator` reading the current server ID from `server_id_getter`.
    // go: NewGlobalAllocator L1
    pub fn new(server_id_getter: Arc<dyn Fn() -> u64 + Send + Sync>, enable_32bits: bool) -> Self {
        Self::with_32bit_limit(server_id_getter, enable_32bits, MAX_LOCAL_CONN_ID_32)
    }

    /// Creates a `GlobalAllocator` with an explicit 32-bit local connection ID ceiling (for tests).
    // glue: test constructor matching Go's ldflagLocalConnIDBits32
    pub fn with_32bit_limit(
        server_id_getter: Arc<dyn Fn() -> u64 + Send + Sync>,
        enable_32bits: bool,
        max_local_32: u64,
    ) -> Self {
        assert!(
            max_local_32 <= MAX_LOCAL_CONN_ID_32,
            "32-bit local connection limit {max_local_32} exceeds {MAX_LOCAL_CONN_ID_32}"
        );
        Self {
            is_64bits: AtomicBool::new(!enable_32bits),
            server_id_getter,
            local32: Mutex::new(LocalPool32::new(max_local_32)),
            local64: Mutex::new(AutoIncPool64::new(
                MAX_LOCAL_CONN_ID_64 + 1 - RESERVED_COUNT,
            )),
        }
    }

    /// Allocates the next structured [`GCID`].
    // go: GlobalAllocator.Allocate L1
    pub fn allocate(&self) -> GCID {
        let server_id = (self.server_id_getter)();

        if !self.is_64bits.load(Ordering::SeqCst) && server_id <= MAX_SERVER_ID_32 {
            if let Some(local_conn_id) = self.local32.lock().get() {
                return GCID {
                    server_id,
                    local_conn_id,
                    is_64bits: false,
                };
            }
            if !self.is_64bits.swap(true, Ordering::SeqCst) {
                tidb_base::log_info!("GlobalAllocator upgrade to 64 bits");
            }
        }

        let local_conn_id = self
            .local64
            .lock()
            .get()
            .expect("Failed to allocate 64bits local connID after max tries");
        GCID {
            server_id,
            local_conn_id,
            is_64bits: true,
        }
    }
}

impl ConnIdAllocator for GlobalAllocator {
    // go: GlobalAllocator.NextID L1
    fn next_id(&self) -> u64 {
        self.allocate().to_conn_id()
    }

    // go: GlobalAllocator.Release L1
    fn release(&self, connection_id: u64) {
        let (gcid, is_truncated) = match parse_conn_id(connection_id) {
            Ok(res) => res,
            Err(err) => {
                tidb_base::log_error!(
                    "failed to ParseGlobalConnID",
                    "error" => &err,
                    "connectionID" => connection_id,
                    "isTruncated" => false,
                );
                return;
            }
        };
        if is_truncated {
            tidb_base::log_error!(
                "failed to ParseGlobalConnID",
                "error" => "",
                "connectionID" => connection_id,
                "isTruncated" => true,
            );
            return;
        }
        if gcid.is_64bits {
            self.local64.lock().put(gcid.local_conn_id);
        } else {
            let mut pool = self.local32.lock();
            if pool.put(gcid.local_conn_id) {
                // Match Go `globalconn.go:310`: `g.local32.Len() < g.local32.Cap()/2`, where
                // `LockFreeCircularPool.Len()` is the count of free (available) 32-bit slots.
                if pool.free_len() < pool.cap() / 2 && self.is_64bits.swap(false, Ordering::SeqCst)
                {
                    tidb_base::log_info!("GlobalAllocator downgrade to 32 bits");
                }
            } else {
                tidb_base::log_error!(
                    "failed to release 32bits connection ID",
                    "connectionID" => connection_id,
                    "localConnID" => gcid.local_conn_id,
                );
            }
        }
    }

    // go: GlobalAllocator.GetReservedConnID L1
    fn get_reserved_conn_id(&self, reserved_no: u64) -> u64 {
        assert!(
            reserved_no < RESERVED_COUNT,
            "invalid reservedNo exceed {RESERVED_COUNT}"
        );
        GCID {
            server_id: (self.server_id_getter)(),
            local_conn_id: MAX_LOCAL_CONN_ID_64 - reserved_no,
            is_64bits: true,
        }
        .to_conn_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_server_id_one_produces_2097154_first() {
        let alloc = GlobalAllocator::new(Arc::new(|| 1), true);
        let id1 = alloc.next_id();
        let id2 = alloc.next_id();
        assert_eq!(id1, 2_097_154); // (1 << 21) | (1 << 1)
        assert_eq!(id2, 2_097_156); // (1 << 21) | (2 << 1)
        let (parsed, truncated) = parse_conn_id(id1).unwrap();
        assert!(!truncated);
        assert_eq!(parsed.server_id, 1);
        assert_eq!(parsed.local_conn_id, 1);
        assert!(!parsed.is_64bits);
    }

    #[test]
    fn overflow_to_64bit_odd_and_server_id_over_2047() {
        let alloc = GlobalAllocator::with_32bit_limit(Arc::new(|| 1), true, 2);
        assert_eq!(alloc.next_id(), 2_097_154);
        assert_eq!(alloc.next_id(), 2_097_156);
        let id64 = alloc.next_id();
        assert_eq!(id64 & 0x1, 1, "64-bit GCID has odd markup bit");
        let (parsed, truncated) = parse_conn_id(id64).unwrap();
        assert!(!truncated);
        assert!(parsed.is_64bits);
        assert_eq!(parsed.server_id, 1);
        assert_eq!(parsed.local_conn_id, 1);

        // ServerID > MAX_SERVER_ID_32 automatically allocates 64-bit GCIDs.
        let alloc_high = GlobalAllocator::new(Arc::new(|| MAX_SERVER_ID_32 + 5), true);
        let id_high = alloc_high.next_id();
        let (parsed_high, truncated_high) = parse_conn_id(id_high).unwrap();
        assert!(!truncated_high);
        assert!(parsed_high.is_64bits);
        assert_eq!(parsed_high.server_id, MAX_SERVER_ID_32 + 5);
    }

    #[test]
    fn truncation_detection_and_recycle() {
        let alloc = GlobalAllocator::new(Arc::new(|| 42), false);
        let id64 = alloc.next_id();
        assert_ne!(id64 & 0xffff_ffff_0000_0000, 0);
        let truncated_val = id64 & 0xffff_ffff;
        let (_, truncated) = parse_conn_id(truncated_val).unwrap();
        assert!(truncated);
        alloc.release(id64);
    }

    #[test]
    fn upgrade_and_downgrade_match_go_free_slots_condition() {
        // cap = 4: releasing 1 ID after exhaustion leaves free = 1 < 4/2 (2) -> downgrades to 32-bit.
        let alloc = GlobalAllocator::with_32bit_limit(Arc::new(|| 1), true, 4);
        let ids32: Vec<u64> = (0..4).map(|_| alloc.next_id()).collect();
        for &id in &ids32 {
            assert!(!parse_conn_id(id).unwrap().0.is_64bits);
        }
        let id64 = alloc.next_id();
        assert!(parse_conn_id(id64).unwrap().0.is_64bits);
        assert!(alloc.is_64bits.load(Ordering::SeqCst));

        alloc.release(ids32[0]); // free = 1 < cap / 2 (2) => downgrade to 32-bit
        assert!(!alloc.is_64bits.load(Ordering::SeqCst));
        let next = alloc.next_id();
        assert_eq!(next, ids32[0]);
        assert!(!parse_conn_id(next).unwrap().0.is_64bits);

        // Exhaust again -> upgrades back to 64-bit. Release 2 IDs where pool starts at cap = 2:
        // releasing 1st makes free = 1 == cap / 2 (1), NOT < 1 => stays 64-bit.
        let alloc2 = GlobalAllocator::with_32bit_limit(Arc::new(|| 1), true, 2);
        let first = alloc2.next_id();
        let _second = alloc2.next_id();
        let extra64 = alloc2.next_id();
        assert!(parse_conn_id(extra64).unwrap().0.is_64bits);
        alloc2.release(first); // free = 1, cap / 2 = 1 => not < 1 => remains 64-bit
        assert!(alloc2.is_64bits.load(Ordering::SeqCst));
        assert!(parse_conn_id(alloc2.next_id()).unwrap().0.is_64bits);
    }
}
