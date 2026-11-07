//! TCP listener: accepts connections and runs each on its own task.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::constants::{
    DEFAULT_MAX_ALLOWED_PACKET, DEFAULT_SERVER_VERSION, auth_plugin, capability,
};
use crate::handler::SessionFactory;
use crate::tls::TlsConfig;

/// Protocol-level server settings.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Version string in the handshake (and what `SELECT VERSION()` should report).
    pub server_version: String,
    /// Auth plugin advertised in the initial handshake (TiDB's `default_authentication_plugin`).
    /// Clients answering with another plugin are switched to `mysql_native_password`.
    pub default_auth_plugin: String,
    /// Capability flags offered to clients.
    pub capability: u32,
    /// Largest accepted command payload.
    pub max_allowed_packet: usize,
    /// TLS configuration; when set, `CLIENT_SSL` is advertised and clients may upgrade.
    pub tls: Option<TlsConfig>,
}

impl ServerConfig {
    /// Capability flags sent in the handshake: `capability`, plus `CLIENT_SSL` with TLS.
    pub fn advertised_capability(&self) -> u32 {
        if self.tls.is_some() {
            self.capability | capability::CLIENT_SSL
        } else {
            self.capability
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            server_version: DEFAULT_SERVER_VERSION.to_string(),
            default_auth_plugin: auth_plugin::MYSQL_NATIVE_PASSWORD.to_string(),
            capability: capability::SERVER_DEFAULT,
            max_allowed_packet: DEFAULT_MAX_ALLOWED_PACKET,
            tls: None,
        }
    }
}

/// A MySQL-protocol server serving sessions created by `F`.
pub struct Server<F> {
    config: Arc<ServerConfig>,
    factory: Arc<F>,
    next_connection_id: AtomicU64,
    /// N3 runtime: connections are tasks of this pool (connection task = statement task).
    /// `None`: `tokio::spawn` on the current runtime (tests).
    pool: Option<tidb_runtime::ExecPool>,
    /// N3 Q1: statement admission shared by every connection.
    admission: Option<tidb_runtime::Admission>,
}

impl<F: SessionFactory> Server<F> {
    pub fn new(config: ServerConfig, factory: F) -> Self {
        Server {
            config: Arc::new(config),
            factory: Arc::new(factory),
            next_connection_id: AtomicU64::new(1),
            pool: None,
            admission: None,
        }
    }

    /// Run connections on `pool` and admit commands through `admission` (N3 3.1, Q1).
    pub fn with_runtime(
        mut self,
        pool: tidb_runtime::ExecPool,
        admission: tidb_runtime::Admission,
    ) -> Self {
        self.pool = Some(pool);
        self.admission = Some(admission);
        self
    }

    /// Accepts connections until `shutdown` completes, then aborts the remaining connections.
    pub async fn serve(
        &self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()>,
    ) -> io::Result<()> {
        info!(addr = %listener.local_addr()?, "MySQL protocol server listening");
        let mut conns: Vec<JoinHandle<()>> = Vec::new();
        let started = std::time::Instant::now();
        tokio::pin!(shutdown);
        loop {
            // Reap finished connection tasks.
            conns.retain(|h| !h.is_finished());
            tokio::select! {
                () = &mut shutdown => break,
                accepted = listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(a) => a,
                        Err(e) => {
                            // E.g. EMFILE: back off instead of spinning.
                            warn!(error = %e, "accept failed");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                    if let Err(e) = stream.set_nodelay(true) {
                        warn!(error = %e, "set TCP_NODELAY failed");
                    }
                    let id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
                    let config = Arc::clone(&self.config);
                    let factory = Arc::clone(&self.factory);
                    let admission = self.admission.clone();
                    let fut = async move {
                        crate::conn::run(stream, peer.to_string(), id, started, &config, &*factory, admission).await;
                    };
                    conns.push(match &self.pool {
                        Some(pool) => pool.spawn_connection("connection", fut),
                        None => tokio::spawn(fut),
                    });
                }
            }
        }
        info!(connections = conns.len(), "shutting down");
        for h in &conns {
            h.abort();
        }
        for h in conns {
            let _ = h.await;
        }
        Ok(())
    }
}
