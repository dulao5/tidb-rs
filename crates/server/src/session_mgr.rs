//! go-file: pkg/server/server.go
//!
//! Server-wide connection registry, kill dispatch, and `SessionManager` implementation
//! (`tidb-server`, rank 16).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use arc_swap::ArcSwapOption;
use parking_lot::RwLock;
use tidb_base::errors;
use tidb_memory::sqlkiller::{
    MaxExecTimeExceeded, QueryInterrupted, RunawayQueryExceeded, SqlKiller,
};
use tidb_runtime::cancel::{ChildToken, KillCancel};
use tidb_sessionctx::session_mgr::{KillRemoteFuture, ProcessInfo, SessionManager};

use crate::conn_id::{ConnIdAllocator, GlobalAllocator, SimpleAllocator};

/// Hook invoked by [`ServerSessionManager::kill_remote_conn`] to forward a global `KILL`
/// over the status-port gRPC `Coprocessor` RPC to `target_server_id`.
pub type RemoteKillFn =
    Arc<dyn Fn(u64, u64, bool) -> KillRemoteFuture<'static> + Send + Sync + 'static>;

/// Per-connection runtime control shared between [`ServerSessionManager`], the MySQL
/// protocol loop, `PocSession`, and `tidb-session`.
pub struct ConnControl {
    pub conn_id: u64,
    pub killer: Arc<SqlKiller>,
    pub kill_cancel: KillCancel,
    /// Per-statement child token cancelled alongside [`Self::kill_cancel`] and
    /// `SqlKiller::finish_result_set` during a `KILL` call.
    pub finish_cancel: Arc<ArcSwapOption<ChildToken>>,
    /// Per-connection shutdown token cancelled on `KILL CONNECTION` or `KillAllConnections`.
    pub conn_shutdown: ChildToken,
    /// Marks whether `KILL CONNECTION` has requested loop exit after current command cleanup.
    pub wait_shutdown: AtomicBool,
    /// Live lock-free process list entry shared with sessions running on this connection.
    pub process_info: Arc<ArcSwapOption<ProcessInfo>>,
}

impl ConnControl {
    /// Creates a new [`ConnControl`] with a paired [`SqlKiller`] and [`KillCancel`].
    // glue: shared per-connection kill/process control
    pub fn new(conn_id: u64) -> Arc<Self> {
        let killer = SqlKiller::new();
        killer.conn_id.store(conn_id, Ordering::SeqCst);
        let kill_cancel = KillCancel::new(killer.signal_handle());
        {
            let notify = kill_cancel.clone();
            killer.set_on_kill(Some(Box::new(move || notify.notify())));
        }
        {
            let notify = kill_cancel.clone();
            killer.set_on_reset(Some(Box::new(move || notify.on_reset())));
        }
        Arc::new(Self {
            conn_id,
            killer,
            kill_cancel,
            finish_cancel: Arc::new(ArcSwapOption::empty()),
            conn_shutdown: ChildToken::new(),
            wait_shutdown: AtomicBool::new(false),
            process_info: Arc::new(ArcSwapOption::empty()),
        })
    }

    /// Creates a [`ConnControl`] bound to an existing `Session`'s `killer`, `kill_cancel`, and
    /// `process_info` slot (used when switching/sharing across engines or unit tests).
    // glue: shared control over an existing SqlKiller/KillCancel pair
    pub fn from_parts(
        conn_id: u64,
        killer: Arc<SqlKiller>,
        kill_cancel: KillCancel,
        process_info: Arc<ArcSwapOption<ProcessInfo>>,
    ) -> Arc<Self> {
        killer.conn_id.store(conn_id, Ordering::SeqCst);
        Arc::new(Self {
            conn_id,
            killer,
            kill_cancel,
            finish_cancel: Arc::new(ArcSwapOption::empty()),
            conn_shutdown: ChildToken::new(),
            wait_shutdown: AtomicBool::new(false),
            process_info,
        })
    }

    /// Replaces the per-statement `finish_cancel` child token and returns the installed token.
    pub fn install_finish_cancel(&self) -> Arc<ChildToken> {
        let token = Arc::new(ChildToken::new());
        self.finish_cancel.store(Some(Arc::clone(&token)));
        if self.killer.get_kill_signal() != 0 || self.is_wait_shutdown() {
            token.cancel();
        }
        token
    }

    /// Clears the active per-statement `finish_cancel` token.
    pub fn clear_finish_cancel(&self) {
        self.finish_cancel.store(None);
    }

    /// Returns `true` if `KILL CONNECTION` has requested closing this connection.
    pub fn is_wait_shutdown(&self) -> bool {
        self.wait_shutdown.load(Ordering::SeqCst) || self.conn_shutdown.is_cancelled()
    }
}

/// Server-wide [`SessionManager`] owning active [`ConnControl`] entries and `server_id` lease state.
pub struct ServerSessionManager {
    server_id: Arc<AtomicU64>,
    valid_server_id: Arc<AtomicBool>,
    global_kill_enabled: bool,
    allocator: Arc<dyn ConnIdAllocator>,
    clients: RwLock<HashMap<u64, Arc<ConnControl>>>,
    remote_kill: ArcSwapOption<RemoteKillFn>,
}

impl ServerSessionManager {
    /// Creates a standalone [`ServerSessionManager`] with `server_id = 1` (first conn ID `2097154`).
    // glue: standalone global kill server_id=1
    pub fn new_standalone() -> Arc<Self> {
        Self::new_with_config(1, true, true, true)
    }

    /// Creates a [`ServerSessionManager`] with global kill disabled (`SimpleAllocator`: `1, 2, 3, ...`).
    // glue: !enable-global-kill fallback matching Go domain.go:1487-1489
    pub fn new_simple() -> Arc<Self> {
        Arc::new(Self {
            server_id: Arc::new(AtomicU64::new(0)),
            valid_server_id: Arc::new(AtomicBool::new(false)),
            global_kill_enabled: false,
            allocator: Arc::new(SimpleAllocator::new()),
            clients: RwLock::new(HashMap::new()),
            remote_kill: ArcSwapOption::empty(),
        })
    }

    /// Creates a [`ServerSessionManager`] initialized with `server_id` and lease validity flag
    /// (with global kill & 32-bit connection IDs enabled).
    // glue: cluster / configured server_id initialization
    pub fn new_with_server_id(server_id: u64, valid: bool) -> Arc<Self> {
        Self::new_with_config(server_id, valid, true, true)
    }

    /// Creates a [`ServerSessionManager`] honoring both `enable_global_kill` and
    /// `enable_32bits_connection_id` (matching Go `domain.go:1469-1490`).
    // go-part: Domain.InitGlobalKill / NewServerSessionManager L1
    pub fn new_with_config(
        server_id: u64,
        valid: bool,
        enable_global_kill: bool,
        enable_32bits_conn_id: bool,
    ) -> Arc<Self> {
        if !enable_global_kill {
            return Self::new_simple();
        }
        let sid = Arc::new(AtomicU64::new(server_id));
        let sid_getter = Arc::clone(&sid);
        let allocator: Arc<dyn ConnIdAllocator> = Arc::new(GlobalAllocator::new(
            Arc::new(move || sid_getter.load(Ordering::SeqCst)),
            enable_32bits_conn_id,
        ));
        Arc::new(Self {
            server_id: sid,
            valid_server_id: Arc::new(AtomicBool::new(valid)),
            global_kill_enabled: true,
            allocator,
            clients: RwLock::new(HashMap::new()),
            remote_kill: ArcSwapOption::empty(),
        })
    }

    /// Creates a [`ServerSessionManager`] with a custom [`ConnIdAllocator`].
    pub fn with_allocator(
        server_id: u64,
        valid: bool,
        allocator: Arc<dyn ConnIdAllocator>,
    ) -> Arc<Self> {
        Arc::new(Self {
            server_id: Arc::new(AtomicU64::new(server_id)),
            valid_server_id: Arc::new(AtomicBool::new(valid)),
            global_kill_enabled: true,
            allocator,
            clients: RwLock::new(HashMap::new()),
            remote_kill: ArcSwapOption::empty(),
        })
    }

    /// Updates `server_id` and whether the node currently holds a valid `/tidb/server_id` lease.
    pub fn set_server_id(&self, id: u64, valid: bool) {
        if !self.global_kill_enabled {
            return;
        }
        self.server_id.store(id, Ordering::SeqCst);
        self.valid_server_id.store(valid, Ordering::SeqCst);
    }

    /// Returns `true` when global kill is enabled and this node lacks a valid `/tidb/server_id`
    /// lease (Go `Domain.IsLostConnectionToPD` + `Server.Run` reject-connection check).
    pub fn is_lost_connection_to_pd(&self) -> bool {
        self.global_kill_enabled && !self.valid_server_id.load(Ordering::SeqCst)
    }

    /// Installs the cross-node `KILL` RPC dispatcher (`killRemoteConn`).
    pub fn set_remote_kill_hook(&self, hook: RemoteKillFn) {
        self.remote_kill.store(Some(Arc::new(hook)));
    }

    /// Allocates the next connection ID.
    pub fn next_conn_id(&self) -> u64 {
        self.allocator.next_id()
    }

    /// Releases a connection ID back to the allocator.
    pub fn release_conn_id(&self, conn_id: u64) {
        self.allocator.release(conn_id);
    }

    /// Allocates a new connection ID, creates its [`ConnControl`], and registers it.
    pub fn register_new_conn(&self) -> Arc<ConnControl> {
        let conn_id = self.next_conn_id();
        let ctrl = ConnControl::new(conn_id);
        self.register_conn(Arc::clone(&ctrl));
        ctrl
    }

    /// Registers a [`ConnControl`] under its `conn_id`.
    pub fn register_conn(&self, ctrl: Arc<ConnControl>) {
        self.clients.write().insert(ctrl.conn_id, ctrl);
    }

    /// Removes `conn_id` from the registry and releases it back to the ID pool.
    pub fn unregister_conn(&self, conn_id: u64) {
        self.clients.write().remove(&conn_id);
        self.release_conn_id(conn_id);
    }

    /// Looks up [`ConnControl`] by `conn_id`.
    pub fn get_conn(&self, conn_id: u64) -> Option<Arc<ConnControl>> {
        self.clients.read().get(&conn_id).cloned()
    }

    /// Returns the number of active registered connections.
    pub fn client_count(&self) -> usize {
        self.clients.read().len()
    }
}

/// Signals `SqlKiller`, cancels any statement/connection tokens, and invokes `FinishResultSet`.
// go: server.killQuery L2 (also cancels FinishCancel/KillCancel tokens for async wakeups)
fn kill_query(conn: &ConnControl, max_execution_time: bool, runaway: bool) {
    if runaway {
        conn.killer.send_kill_signal(RunawayQueryExceeded);
    } else if max_execution_time {
        conn.killer.send_kill_signal(MaxExecTimeExceeded);
    } else {
        conn.killer.send_kill_signal(QueryInterrupted);
    }
    conn.kill_cancel.notify();
    if let Some(fc) = conn.finish_cancel.load().as_ref() {
        fc.cancel();
    }
    conn.killer.finish_result_set();
}

impl SessionManager for ServerSessionManager {
    // go: Server.ShowProcessList L1
    fn show_process_list(&self) -> HashMap<u64, Arc<ProcessInfo>> {
        let clients = self.clients.read();
        let mut rs = HashMap::with_capacity(clients.len());
        for (&id, conn) in clients.iter() {
            if let Some(pi) = conn.process_info.load_full() {
                rs.insert(id, pi);
            }
        }
        rs
    }

    // go: Server.GetProcessInfo L1
    fn get_process_info(&self, id: u64) -> Option<Arc<ProcessInfo>> {
        let conn = self.clients.read().get(&id).cloned()?;
        conn.process_info.load_full()
    }

    // go: Server.Kill L2 (omits SysProcTracker until background sys processes land)
    fn kill(&self, connection_id: u64, query: bool, max_execution_time: bool, runaway: bool) {
        tidb_base::log_info!(
            "kill",
            "conn" => connection_id,
            "query" => query,
            "maxExecutionTime" => max_execution_time,
            "runawayExceed" => runaway,
        );
        tidb_metrics::server_event_kill();

        let conn = self.clients.read().get(&connection_id).cloned();
        let Some(conn) = conn else {
            return;
        };

        if !query {
            conn.wait_shutdown.store(true, Ordering::SeqCst);
            conn.conn_shutdown.cancel();
            // log-skip: Server.Kill:943 async conn_shutdown token cancels socket writes without SetWriteDeadline
            // log-skip: Server.Kill:946 async conn_shutdown token cancels socket reads without SetReadDeadline
        }
        kill_query(&conn, max_execution_time, runaway);
    }

    // go: Server.KillAllConnections L1
    fn kill_all_connections(&self) {
        tidb_base::log_info!("kill all connections.", "category" => "server");
        let conns: Vec<Arc<ConnControl>> = {
            let guard = self.clients.read();
            guard.values().cloned().collect()
        };
        for conn in conns {
            conn.wait_shutdown.store(true, Ordering::SeqCst);
            conn.conn_shutdown.cancel();
            // log-skip: Server.KillAllConnections:1001 async conn_shutdown token cancels socket reads without SetReadDeadline
            kill_query(&conn, false, false);
        }
    }

    // go: Server.ServerID L1
    fn server_id(&self) -> u64 {
        self.server_id.load(Ordering::SeqCst)
    }

    // glue: cluster server_id lease validity guard
    fn has_valid_server_id(&self) -> bool {
        self.valid_server_id.load(Ordering::SeqCst)
    }

    // glue: Server.Run reject connection guard when PD connectivity is lost
    fn is_lost_connection_to_pd(&self) -> bool {
        ServerSessionManager::is_lost_connection_to_pd(self)
    }

    // go: SimpleExec.killRemoteConn L2 (hooked into status-port RPC client)
    fn kill_remote_conn(
        &self,
        target_server_id: u64,
        conn_id: u64,
        query: bool,
    ) -> KillRemoteFuture<'_> {
        if let Some(hook) = self.remote_kill.load_full() {
            return hook(target_server_id, conn_id, query);
        }
        Box::pin(async { Err(errors::new("remote kill RPC is unavailable".to_owned())) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static KILL_COUNTER_LOCK: parking_lot::Mutex<()> = parking_lot::const_mutex(());

    #[tokio::test]
    async fn kill_query_vs_kill_connection_signals_and_tokens() {
        let _counter_guard = KILL_COUNTER_LOCK.lock();
        let mgr = ServerSessionManager::new_standalone();
        assert_eq!(mgr.server_id(), 1);
        assert!(mgr.has_valid_server_id());

        let ctrl = mgr.register_new_conn();
        assert_eq!(ctrl.conn_id, 2_097_154);
        let finish_tok = ctrl.install_finish_cancel();
        let before = tidb_metrics::server_event_kill_total();

        // KILL QUERY must cancel KillCancel + FinishCancel and send QueryInterrupted,
        // WITHOUT cancelling conn_shutdown.
        mgr.kill(ctrl.conn_id, true, false, false);
        assert_eq!(ctrl.killer.get_kill_signal(), QueryInterrupted);
        assert!(finish_tok.is_cancelled());
        assert!(!ctrl.conn_shutdown.is_cancelled());
        assert!(!ctrl.is_wait_shutdown());
        assert_eq!(tidb_metrics::server_event_kill_total(), before + 1);

        // Reset killer for next statement.
        ctrl.killer.reset();
        ctrl.clear_finish_cancel();

        // KILL CONNECTION sets conn_shutdown + wait_shutdown too.
        mgr.kill(ctrl.conn_id, false, false, false);
        assert!(ctrl.conn_shutdown.is_cancelled());
        assert!(ctrl.is_wait_shutdown());
        assert_eq!(ctrl.killer.get_kill_signal(), QueryInterrupted);

        mgr.unregister_conn(ctrl.conn_id);
        assert_eq!(mgr.client_count(), 0);
    }

    #[test]
    fn kill_all_connections_cancels_all_registered_clients() {
        let mgr = ServerSessionManager::new_standalone();
        let c1 = mgr.register_new_conn();
        let c2 = mgr.register_new_conn();
        mgr.kill_all_connections();
        assert!(c1.is_wait_shutdown());
        assert!(c2.is_wait_shutdown());
        assert_eq!(c1.killer.get_kill_signal(), QueryInterrupted);
        assert_eq!(c2.killer.get_kill_signal(), QueryInterrupted);
    }

    #[test]
    fn max_exec_time_and_runaway_map_to_expected_signals() {
        let _counter_guard = KILL_COUNTER_LOCK.lock();
        let mgr = ServerSessionManager::new_standalone();
        let c1 = mgr.register_new_conn();
        mgr.kill(c1.conn_id, true, true, false);
        assert_eq!(c1.killer.get_kill_signal(), MaxExecTimeExceeded);

        c1.killer.reset();
        mgr.kill(c1.conn_id, true, false, true);
        assert_eq!(c1.killer.get_kill_signal(), RunawayQueryExceeded);
    }

    #[test]
    fn allocator_honors_global_kill_and_32bits_config_switches() {
        // 1. enable_global_kill = false -> SimpleAllocator (1, 2, 3), never lost to PD.
        let simple = ServerSessionManager::new_with_config(1, true, false, true);
        assert_eq!(simple.server_id(), 0);
        assert!(!simple.is_lost_connection_to_pd());
        simple.set_server_id(99, true); // no-op when global kill is off
        assert_eq!(simple.server_id(), 0);
        assert_eq!(simple.next_conn_id(), 1);
        assert_eq!(simple.next_conn_id(), 2);
        assert_eq!(simple.next_conn_id(), 3);

        // 2. enable_global_kill = true, enable_32bits = true -> 32-bit 2097154, 2097156.
        let g32 = ServerSessionManager::new_with_config(1, true, true, true);
        assert_eq!(g32.server_id(), 1);
        assert!(!g32.is_lost_connection_to_pd());
        assert_eq!(g32.next_conn_id(), 2_097_154);
        assert_eq!(g32.next_conn_id(), 2_097_156);

        // 3. enable_global_kill = true, enable_32bits = false -> 64-bit odd IDs immediately.
        let g64 = ServerSessionManager::new_with_config(1, true, true, false);
        for expected_local in 1u64..=3 {
            let id = g64.next_conn_id();
            assert_eq!(id & 0x1, 1, "expected 64-bit odd ID, got {id}");
            let (gcid, truncated) = tidb_sessionctx::globalconn::parse_conn_id(id).unwrap();
            assert!(!truncated);
            assert!(gcid.is_64bits);
            assert_eq!(gcid.server_id, 1);
            assert_eq!(gcid.local_conn_id, expected_local);
        }

        // Lost PD lease transition toggles is_lost_connection_to_pd().
        let cluster = ServerSessionManager::new_with_config(0, false, true, true);
        assert!(cluster.is_lost_connection_to_pd());
        cluster.set_server_id(7, true);
        assert!(!cluster.is_lost_connection_to_pd());
        cluster.set_server_id(7, false);
        assert!(cluster.is_lost_connection_to_pd());
    }
}
