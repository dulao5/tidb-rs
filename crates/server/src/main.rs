//! Read-only TiDB-compatible SQL node (prototype). See docs/adr/0001.
//!
//! The MySQL protocol layer is wired to the SQL sessions of `tidb-session`, which read from the
//! TiKV cluster behind `--pd` (see docs/adr/0004-prototype-sql-layer.md).

#![forbid(unsafe_code)]

mod status;

use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use tidb_distsql::DagContext;
use tidb_domain::SchemaSync;
use tidb_domain::ServerInfo;
use tidb_domain::SyncConfig;
use tidb_kv::KvClient;
use tidb_meta::SchemaCache;
use tidb_mysql_protocol::constants::auth_plugin;
use tidb_mysql_protocol::tls::{self, TlsConfig};
use tidb_mysql_protocol::{Credential, Server, ServerConfig};
use tidb_session::Factory;
use tidb_session::Shared;
use tracing::{error, info, warn};

#[derive(Debug, Parser)]
#[command(name = "tidb-server", about = "tidb-rs read-only SQL node (prototype)")]
struct Args {
    /// Address to listen on for MySQL clients.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Port to listen on (4001 avoids clashing with a Go TiDB on 4000).
    #[arg(short = 'P', long, default_value_t = 4001)]
    port: u16,
    /// The single account accepted until users are read from the cluster.
    #[arg(long, default_value = "root")]
    user: String,
    /// Password of `--user` (empty = no password).
    #[arg(long, default_value = "")]
    password: String,
    /// Auth plugin advertised in the handshake (TiDB's default_authentication_plugin).
    /// Clients answering with another plugin are switched to mysql_native_password.
    #[arg(long, default_value = auth_plugin::MYSQL_NATIVE_PASSWORD,
          value_parser = [auth_plugin::MYSQL_NATIVE_PASSWORD, auth_plugin::CACHING_SHA2_PASSWORD])]
    default_auth_plugin: String,
    /// Generate a self-signed certificate at startup when --ssl-cert/--ssl-key are not given
    /// (TiDB's security.auto-tls). `--auto-tls=false` disables TLS in that case.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    auto_tls: bool,
    /// PEM certificate (chain) for TLS connections (TiDB security.ssl-cert).
    #[arg(long, requires = "ssl_key")]
    ssl_cert: Option<PathBuf>,
    /// PEM private key for --ssl-cert (TiDB security.ssl-key).
    #[arg(long, requires = "ssl_cert")]
    ssl_key: Option<PathBuf>,
    /// PEM CA bundle; client certificates are verified when presented (TiDB security.ssl-ca).
    #[arg(long)]
    ssl_ca: Option<PathBuf>,
    /// PD endpoints of the cluster (`host:port`, comma-separated or repeated).
    #[arg(long, required = true, value_delimiter = ',')]
    pd: Vec<String>,
    /// gRPC connections to each TiKV store (TiDB: tikv-client.grpc-connection-count).
    #[arg(long, default_value_t = 4)]
    tikv_connections: usize,
    /// Log per-statement phase timings (average microseconds by statement shape) every SECS
    /// seconds; 0 (the default) disables collection.
    #[arg(long, value_name = "SECS", default_value_t = 0)]
    stmt_timing: u64,
    /// Join TiDB's schema sync (ADR 0005): register in PD's etcd as a TiDB server, keep the
    /// schema loaded in the background and follow the metadata-lock protocol, so statements
    /// use the in-memory schema and autocommit point gets read at MaxTS. The DDL owner then
    /// waits for this node. `--schema-sync=false` keeps the per-statement schema check of
    /// ADR 0003 and writes nothing to etcd.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    schema_sync: bool,
    /// IP address published in the server info (`ip`); together with --port it must be
    /// unique among all TiDB servers. Default: --host, or the address used to reach PD when
    /// --host is a wildcard.
    #[arg(long)]
    advertise_address: Option<IpAddr>,
    /// Address of the status HTTP server (`/metrics`, `/status`; TiDB's status.status-host).
    /// Default: --host.
    #[arg(long)]
    status_host: Option<String>,
    /// Port of the status HTTP server (TiDB's status.status-port; 10081 avoids clashing with a
    /// Go TiDB on 10080, as 4001 does for --port). 0 disables it.
    #[arg(long, default_value_t = 10081)]
    status_port: u16,
    /// `status_port` published in the server info. Default: the status server's port, or 0
    /// (nothing listens: Go TiDB's cluster-table fan-out to this node fails at once with a
    /// warning) with --status-port 0.
    #[arg(long)]
    advertise_status_port: Option<u16>,
    /// Synced mode needs a completed reload + MDL cycle within SECS, and the node may lag the
    /// cluster's schema version at most SECS; otherwise it deregisters (DDL stops waiting
    /// for it) and statements fall back until it is in sync again.
    #[arg(long, value_name = "SECS", default_value_t = 5.0)]
    schema_sync_max_staleness: f64,
    /// Test hook: while this file exists the schema sync loop does nothing, as if stuck.
    #[arg(long, hide = true)]
    schema_sync_stall_file: Option<PathBuf>,
    /// Log level: error, warn, info, debug or trace.
    #[arg(long, default_value = "info")]
    log_level: tracing::Level,
}

fn main() {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_max_level(args.log_level)
        .with_target(false)
        .init();

    let addr: SocketAddr = match format!("{}:{}", args.host, args.port).parse() {
        Ok(a) => a,
        Err(e) => {
            error!(host = %args.host, port = args.port, error = %e, "invalid listen address");
            std::process::exit(2);
        }
    };
    let tls = match tls_config(&args) {
        Ok(tls) => tls,
        Err(e) => {
            // Like TiDB: invalid configured certificates halt startup rather than silently
            // downgrading connections.
            error!(error = %e, "failed to set up TLS");
            std::process::exit(2);
        }
    };
    let config = ServerConfig {
        default_auth_plugin: args.default_auth_plugin.clone(),
        tls,
        ..ServerConfig::default()
    };
    tidb_metrics::init(&tidb_metrics::ServerFacts {
        version: tidb_domain::serverinfo::version_string()
            .split_once("-TiDB-")
            .map(|(_, v)| v.to_owned())
            .unwrap_or_default(),
        git_hash: tidb_domain::serverinfo::GIT_HASH.to_owned(),
        maxprocs: worker_threads(),
    });
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!(error = %e, "failed to start the async runtime");
            std::process::exit(1);
        }
    };
    let result = runtime.block_on(async move {
        // Bind the status port first: its real port is what the server info advertises.
        let status_listener = if args.status_port == 0 {
            None
        } else {
            let host = args.status_host.as_deref().unwrap_or(&args.host);
            let listener = tokio::net::TcpListener::bind((host, args.status_port))
                .await
                .map_err(|e| {
                    std::io::Error::other(format!(
                        "listen on status address {host}:{}: {e}",
                        args.status_port
                    ))
                })?;
            Some(listener)
        };
        let advertised_status_port = match (args.advertise_status_port, &status_listener) {
            (Some(p), _) => p,
            (None, Some(l)) => l.local_addr()?.port(),
            (None, None) => 0,
        };
        let kv = KvClient::connect_with(args.pd.clone(), args.tikv_connections)
            .await
            .map_err(|e| std::io::Error::other(format!("connect to PD {:?}: {e}", args.pd)))?;
        let schema = SchemaCache::new(kv.clone());
        let new_collation = tidb_session::detect_new_collation(&kv, &schema)
            .await
            .map_err(|e| std::io::Error::other(format!("read mysql.tidb: {e}")))?;
        info!(pd = ?args.pd, new_collation, "connected to the cluster");
        let dag = DagContext {
            new_collation_enabled: new_collation,
            ..DagContext::select_utc()
        };
        let sync = if args.schema_sync {
            let ip = advertise_ip(&args);
            let info = ServerInfo::new(
                ip.to_string(),
                u32::from(args.port),
                u32::from(advertised_status_port),
            );
            let mut cfg = SyncConfig::new(args.pd.clone(), info, dag.clone());
            cfg.limits.max_staleness = Duration::from_secs_f64(args.schema_sync_max_staleness);
            cfg.stall_file = args.schema_sync_stall_file.clone();
            let sync = SchemaSync::start(kv.clone(), cfg)
                .await
                .map_err(|e| std::io::Error::other(format!("start schema sync: {e}")))?;
            Some(sync)
        } else {
            info!("schema sync is off: every statement checks the schema version (ADR 0003)");
            None
        };
        let factory = Factory::new(Shared {
            kv,
            schema,
            dag,
            sync: sync.clone(),
            user: args.user.clone(),
            credential: Credential::native_from_password(&args.password),
            server_version: config.server_version.clone(),
            tls_enabled: config.tls.is_some(),
        });
        if args.stmt_timing > 0 {
            tidb_session::timing::set_enabled(true);
            let every = std::time::Duration::from_secs(args.stmt_timing);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(every);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    info!("statement timing\n{}", tidb_session::timing::report());
                }
            });
        }
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let (stop_status, status_stopped) = tokio::sync::oneshot::channel::<()>();
        if let Some(status_listener) = status_listener {
            let info = status::StatusInfo {
                version: config.server_version.clone(),
                git_hash: tidb_domain::serverinfo::GIT_HASH.to_owned(),
            };
            tokio::spawn(status::serve(status_listener, info, async move {
                let _ = status_stopped.await;
            }));
        }
        let shutdown = async {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            tokio::select! {
                r = tokio::signal::ctrl_c() => r?,
                _ = term.recv() => {}
            }
            info!("received SIGINT/SIGTERM");
            Ok::<(), std::io::Error>(())
        };
        let shutdown = async {
            if let Err(e) = shutdown.await {
                warn!(error = %e, "signal handling failed; shutting down");
            }
            // Leave the cluster first, so DDL stops waiting for this node at once.
            if let Some(sync) = &sync {
                sync.shutdown().await;
            }
            let _ = stop_status.send(());
        };
        Server::new(config, factory).serve(listener, shutdown).await
    });
    if let Err(e) = result {
        error!(%addr, error = %e, "server failed");
        std::process::exit(1);
    }
}

/// Worker threads of the multi-threaded runtime (tokio: `TOKIO_WORKER_THREADS`, else the
/// available parallelism), reported as `tidb_server_maxprocs` (Go: `GOMAXPROCS`).
fn worker_threads() -> usize {
    std::env::var("TOKIO_WORKER_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

/// The IP published in the server info: --advertise-address, else --host unless it is a
/// wildcard, else the local address of a route to the first PD endpoint.
fn advertise_ip(args: &Args) -> IpAddr {
    if let Some(ip) = args.advertise_address {
        return ip;
    }
    if let Ok(ip) = args.host.parse::<IpAddr>()
        && !ip.is_unspecified()
    {
        return ip;
    }
    let pd = args
        .pd
        .first()
        .map(String::as_str)
        .unwrap_or("127.0.0.1:2379");
    let pd = pd
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect(pd)?;
            s.local_addr()
        })
        .map(|a| a.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]))
}

/// TLS configuration from the flags, mirroring TiDB's `LoadTLSCertificates`: the given
/// certificate and key, else an auto-generated self-signed certificate if `--auto-tls`.
fn tls_config(args: &Args) -> std::io::Result<Option<TlsConfig>> {
    if let (Some(cert), Some(key)) = (&args.ssl_cert, &args.ssl_key) {
        let config = tls::load_tls_config(cert, key, args.ssl_ca.as_deref())?;
        info!(cert = %cert.display(), client_verification = args.ssl_ca.is_some(), "secure connection is enabled");
        return Ok(Some(config));
    }
    if !args.auto_tls {
        info!("TLS is disabled (no --ssl-cert/--ssl-key and --auto-tls=false)");
        return Ok(None);
    }
    let mut names = vec!["localhost"];
    if !args.host.is_empty()
        && args.host != "0.0.0.0"
        && args.host != "::"
        && args.host != "localhost"
    {
        names.push(&args.host);
    }
    let config = tls::auto_tls_config(&names)?;
    info!(
        ?names,
        "secure connection is enabled with an auto-generated self-signed certificate"
    );
    Ok(Some(config))
}
