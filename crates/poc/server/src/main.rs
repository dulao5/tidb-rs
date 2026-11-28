//! Read-only TiDB-compatible SQL node (prototype). See docs/adr/0001.
//!
//! The MySQL protocol layer is wired to the SQL sessions of `tidb-session`, which read from the
//! TiKV cluster behind `--pd` (see docs/adr/0004-prototype-sql-layer.md).

#![forbid(unsafe_code)]

mod rpc;
mod status;

use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;
use clap::parser::ValueSource;
use poc_domain::SchemaSync;
use poc_domain::ServerInfo;
use poc_domain::SyncConfig;
use poc_kv::KvClient;
use poc_session::Factory;
use poc_session::Shared;
use tidb_distsql::DagContext;
use tidb_meta::SchemaCache;
use tidb_mysql_protocol::constants::auth_plugin;
use tidb_mysql_protocol::tls::{self, TlsConfig};
use tidb_mysql_protocol::{Credential, Server, ServerConfig};

// Process-wide allocator: jemalloc (A1 / P0 decision D7), the same one the benchmarks use.
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Debug, Parser)]
#[command(
    name = "tidb-server",
    about = "tidb-rs read-only SQL node (prototype)",
    disable_version_flag = true
)]
struct Args {
    /// Print TiDB version information and exit (`-V` / `--version`).
    #[arg(short = 'V', long = "version")]
    show_version: bool,
    /// Path to TiDB TOML configuration file (`--config`).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Check config file validity and exit (`--config-check`).
    #[arg(long)]
    config_check: bool,
    /// Enforce strict config file checking (`--config-strict`).
    #[arg(long)]
    config_strict: bool,
    /// Keyspace name (`--keyspace-name`).
    #[arg(long, default_value = "")]
    keyspace_name: String,
    /// Address to listen on for MySQL clients.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Port to listen on (4001 avoids clashing with a Go TiDB on 4000).
    #[arg(short = 'P', long, default_value_t = 4001)]
    port: u16,
    /// Legacy single-account username flag (kept for CLI compatibility; authentication reads
    /// `mysql.user` from the cluster).
    #[arg(long, default_value = "root")]
    user: String,
    /// Legacy password flag for `--user` (kept for CLI compatibility; authentication reads
    /// `mysql.user` from the cluster).
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
    #[arg(long, value_delimiter = ',')]
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
    /// Schema lease (Go's `--lease`, a Go duration such as `45s`): synced statements use the
    /// cached schema while the last successful reload is at most one lease old (measured in
    /// TSO, Go's `SchemaValidator`); later reads fall back to the per-statement schema check.
    #[arg(long, value_name = "DURATION", default_value = "45s", value_parser = parse_go_duration)]
    lease: Duration,
    /// The node deregisters (DDL stops waiting for it) when it has completed no reload + MDL
    /// cycle for SECS, or has lagged the cluster's schema version for SECS; statements then
    /// fall back until it is in sync again. Default and minimum: the schema lease, so the
    /// node stops admitting statements before it deregisters.
    #[arg(long, value_name = "SECS")]
    schema_sync_max_staleness: Option<f64>,
    /// Test hook: while this file exists the schema sync loop does nothing, as if stuck.
    #[arg(long, hide = true)]
    schema_sync_stall_file: Option<PathBuf>,
    /// Concurrently executing commands (TiDB's token-limit, `server.concurrentLimiter`): a
    /// command waits for a token, it is never rejected. 0 means 1000 and the value is capped
    /// at 1048576, as Go's `Config.Valid` does.
    #[arg(long, default_value_t = 1000)]
    token_limit: usize,
    /// Thread topology of the runtime (N3 3.1): `steal` (work-stealing, default) or
    /// `no-steal` (one current-thread runtime per worker; a connection stays on its thread).
    #[arg(long, default_value = "steal", value_parser = ["steal", "no-steal"])]
    runtime_topology: String,
    /// Which statements run on the ported stack (specs/VS1-vertical-slice/plan.md §4.3): `off`
    /// runs every statement on the prototype (the default); `fallback` sends the SELECTs the
    /// ported stack supports to it and falls back to the prototype when it reaches code that
    /// is not ported yet; `strict` returns the
    /// stub's 1235 error instead (for the weekly build). The environment variable
    /// TIDB_RS_NEW_STACK, when set, overrides this flag.
    #[arg(long, default_value = "off", value_parser = ["off", "fallback", "strict"])]
    new_stack: String,
    /// Record which engine ran each connection's last statement and serve it on the status
    /// port as `GET /debug/route?conn=<id>` (the test harness's coverage census, #260; off by
    /// default). TIDB_RS_ROUTE_TRACE=1 turns it on too.
    #[arg(long)]
    route_trace: bool,
    /// Log level: debug, info, warn, error, dpanic, panic or fatal (`-L` / `--log-level`).
    #[arg(short = 'L', long, default_value = "info")]
    log_level: String,
    /// Log file path (empty writes to stdout; `--log-file`).
    #[arg(long, default_value = "")]
    log_file: String,
    /// Log format: text or json (`--log-format`).
    #[arg(long, default_value = "text")]
    log_format: String,
    /// General log file path (`--log-general`).
    #[arg(long, default_value = "")]
    log_general: String,
    /// Slow query log file path (`--log-slow-query`).
    #[arg(long, default_value = "")]
    log_slow_query: String,
}

/// `--lease`: Go's `time.ParseDuration`; the lease must be positive.
// glue: clap value parser
fn parse_go_duration(s: &str) -> Result<Duration, String> {
    let ns = tidb_base::gostd::time::parse_duration(s)?;
    if ns <= 0 {
        return Err(format!("the schema lease must be positive, got {s:?}"));
    }
    Ok(Duration::from_nanos(ns as u64))
}

/// `--new-stack`, overridden by `TIDB_RS_NEW_STACK` (vertical-slice plan §4.3).
fn new_stack_mode(flag: &str) -> Result<poc_session::router::NewStackMode, String> {
    match std::env::var("TIDB_RS_NEW_STACK") {
        Ok(v) if !v.is_empty() => v.parse(),
        _ => flag.parse(),
    }
}

fn main() {
    let matches = Args::command().get_matches();
    let args = Args::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    let is_cli = |id: &str| matches.value_source(id) == Some(ValueSource::CommandLine);

    let conf_path = args
        .config
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let has_config_file = args.config.is_some();
    if let Err(tidb_base::config::InitializeExit::Exit(code)) = tidb_base::config::initialize_config(
        &conf_path,
        args.config_check,
        args.config_strict,
        |cfg| {
            // Match Go cmd/tidb-server/main.go:485-585 (`actualFlags`): CLI flags only override
            // values from `--config` when explicitly given on the command line. When no config
            // file is provided, keep tidb-rs's default listen ports (127.0.0.1:4001, status 10081).
            if is_cli("host") || !has_config_file {
                cfg.host.clone_from(&args.host);
            }
            if is_cli("port") || !has_config_file {
                cfg.port = usize::from(args.port);
            }
            if is_cli("advertise_address")
                && let Some(ip) = args.advertise_address
            {
                cfg.advertise_address = ip.to_string();
            }
            if cfg.advertise_address.is_empty() {
                cfg.advertise_address = advertise_ip(&args, &cfg.host).to_string();
            }
            if is_cli("status_host")
                && let Some(sh) = &args.status_host
            {
                cfg.status.status_host.clone_from(sh);
            } else if !has_config_file {
                cfg.status.status_host =
                    args.status_host.clone().unwrap_or_else(|| cfg.host.clone());
            }
            if is_cli("status_port") || !has_config_file {
                cfg.status.status_port = usize::from(args.status_port);
            }
            if is_cli("token_limit") {
                cfg.token_limit = args.token_limit;
            }
            if !args.pd.is_empty() {
                cfg.store = "tikv".to_owned();
                cfg.path = args.pd.join(",");
            }
            if is_cli("log_level") {
                cfg.log.level.clone_from(&args.log_level);
            }
            if is_cli("log_file") && !args.log_file.is_empty() {
                cfg.log
                    .file
                    .file_log_config
                    .filename
                    .clone_from(&args.log_file);
            }
            if is_cli("log_format") && !args.log_format.is_empty() {
                cfg.log.format.clone_from(&args.log_format);
            }
            if is_cli("log_general") && !args.log_general.is_empty() {
                cfg.log.general_log_file.clone_from(&args.log_general);
            }
            if is_cli("log_slow_query") && !args.log_slow_query.is_empty() {
                cfg.log.slow_query_file.clone_from(&args.log_slow_query);
            }
            if is_cli("keyspace_name") && !args.keyspace_name.is_empty() {
                cfg.keyspace_name.clone_from(&args.keyspace_name);
            }
            // Match Go cmd/tidb-server/main.go:774 & :284 (`setGlobalVars` / `UpdateTempStoragePath`)
            // so `loaded config` carries resolved socket and tmp-storage-path values.
            cfg.socket = cfg.socket.replace("{Port}", &cfg.port.to_string());
            cfg.update_temp_storage_path();
        },
    ) {
        std::process::exit(code);
    }

    let tidb_cfg = tidb_base::config::get_global_config_arc();
    if args.show_version {
        // log-skip: -V CLI version output parity with Go cmd/tidb-server/main.go
        println!("{}", tidb_log::get_tidb_info(&tidb_cfg));
        std::process::exit(0);
    }

    if args.pd.is_empty() {
        // log-skip: pre-logger CLI argument error
        eprintln!("error: the following required arguments were not provided:\n  --pd <PD>");
        std::process::exit(2);
    }

    tidb_log::init_version(&tidb_cfg);
    let server_ver = tidb_log::server_version(&tidb_cfg);
    tidb_sessionctx::variable::set_sys_var("version", &server_ver);
    if let Some(comment) = tidb_log::version_comment(&tidb_cfg) {
        tidb_sessionctx::variable::set_sys_var("version_comment", &comment);
    }
    if tidb_cfg.log.query_log_max_len > 0 {
        tidb_sessionctx::variable::tidb_vars::QueryLogMaxLen.store(
            tidb_cfg.log.query_log_max_len as i32,
            std::sync::atomic::Ordering::SeqCst,
        );
    }
    if let Err(e) = tidb_log::init_logger(&tidb_cfg.log.to_log_config(), &tidb_cfg.keyspace_name) {
        // log-skip: pre-logger init failure
        eprintln!("init logger failed: {e}");
        std::process::exit(1);
    }
    log_wrappers::set_redact_provider(|| match tidb_base::errors::redact_log_enabled_load() {
        tidb_base::errors::RedactLogEnable => log_wrappers::RedactMode::On,
        tidb_base::errors::RedactLogMarker => log_wrappers::RedactMode::Marker,
        _ => log_wrappers::RedactMode::Off,
    });
    tidb_log::print_tidb_info(&tidb_cfg);

    let effective_host = tidb_cfg.host.clone();
    let effective_port = u16::try_from(tidb_cfg.port).unwrap_or(args.port);
    let effective_status_host = if tidb_cfg.status.status_host.is_empty() {
        effective_host.clone()
    } else {
        tidb_cfg.status.status_host.clone()
    };
    let effective_status_port =
        u16::try_from(tidb_cfg.status.status_port).unwrap_or(args.status_port);
    let addr: SocketAddr = match format!("{effective_host}:{effective_port}").parse() {
        Ok(a) => a,
        Err(e) => {
            let err_str = e.to_string();
            tidb_base::log_error!(
                "invalid listen address",
                "host" => effective_host.as_str(),
                "port" => u64::from(effective_port),
                "error" => err_str.as_str(),
            );
            let _ = tidb_log::sync();
            std::process::exit(2);
        }
    };
    let new_stack = match new_stack_mode(&args.new_stack) {
        Ok(m) => m,
        Err(e) => {
            tidb_base::log_error!("invalid TIDB_RS_NEW_STACK", "error" => e.as_str());
            let _ = tidb_log::sync();
            std::process::exit(2);
        }
    };
    if let Err(e) = poc_kv::point_get_store_driver_from_env() {
        tidb_base::log_error!("invalid TIDB_RS_POINT_GET", "error" => e.as_str());
        let _ = tidb_log::sync();
        std::process::exit(2);
    }
    let tls = match tls_config(&args) {
        Ok(tls) => tls,
        Err(e) => {
            // Like TiDB: invalid configured certificates halt startup rather than silently
            // downgrading connections.
            let err_str = e.to_string();
            tidb_base::log_error!("failed to set up TLS", "error" => err_str.as_str());
            let _ = tidb_log::sync();
            std::process::exit(2);
        }
    };
    let config = ServerConfig {
        server_version: server_ver.clone(),
        default_auth_plugin: args.default_auth_plugin.clone(),
        tls,
        ..ServerConfig::default()
    };
    tidb_metrics::init(&tidb_metrics::ServerFacts {
        version: tidb_log::release_version(&tidb_cfg),
        git_hash: tidb_log::TIDB_GIT_HASH.to_owned(),
        maxprocs: worker_threads(),
    });
    let runtime = match tidb_runtime::Runtime::new(tidb_runtime::RuntimeConfig {
        topology: if args.runtime_topology == "no-steal" {
            tidb_runtime::Topology::NoSteal
        } else {
            tidb_runtime::Topology::Steal
        },
        worker_threads: worker_threads(),
        thread_name: "tidb-rs-worker".to_owned(),
        ..tidb_runtime::RuntimeConfig::default()
    }) {
        Ok(rt) => rt,
        Err(e) => {
            let err_str = e.to_string();
            tidb_base::log_error!(
                "failed to start the async runtime",
                "error" => err_str.as_str(),
            );
            let _ = tidb_log::sync();
            std::process::exit(1);
        }
    };
    tidb_runtime::metrics::init();
    // Go `Config.Valid`: token-limit 0 -> 1000, at most MaxTokenLimit (1 << 20).
    let token_limit = match args.token_limit {
        0 => 1000,
        n => n.min(1 << 20),
    };
    let admission = match tidb_runtime::Admission::new(&tidb_runtime::AdmissionConfig {
        token_limit,
        ..tidb_runtime::AdmissionConfig::default()
    }) {
        Ok(a) => a,
        Err(e) => {
            let err_str = e.to_string();
            tidb_base::log_error!(
                "invalid admission configuration",
                "error" => err_str.as_str(),
            );
            let _ = tidb_log::sync();
            std::process::exit(2);
        }
    };
    let pool = runtime.pool().clone();
    poc_kv::register_client_metrics();
    let result = runtime.block_on(async move {
        // Bind the status port first: its real port is what the server info advertises.
        let status_listener = if effective_status_port == 0 {
            None
        } else {
            let host = effective_status_host.as_str();
            let listener = tokio::net::TcpListener::bind((host, effective_status_port))
                .await
                .map_err(|e| {
                    std::io::Error::other(format!(
                        "listen on status address {host}:{effective_status_port}: {e}",
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
            .map_err(|e| std::io::Error::other(format!("connect to PD {:?}: {e}", args.pd)))?
            // The coprocessor client's workers run on the ExecPool (D07.2, N3 M-GO).
            .with_exec_pool(pool.clone())
            .map_err(|e| std::io::Error::other(format!("coprocessor client: {e}")))?;
        let schema = SchemaCache::new(kv.clone());
        let new_collation = poc_session::detect_new_collation(&kv, &schema)
            .await
            .map_err(|e| std::io::Error::other(format!("read mysql.tidb: {e}")))?;
        let pd_refs: Vec<&str> = args.pd.iter().map(String::as_str).collect();
        tidb_base::log_info!(
            "connected to the cluster",
            "pd" => pd_refs.as_slice(),
            "new_collation" => new_collation,
        );
        let dag = DagContext {
            new_collation_enabled: new_collation,
            ..DagContext::select_utc()
        };
        let sync = if args.schema_sync {
            let ip = advertise_ip(&args, &effective_host);
            let info = ServerInfo::new(
                ip.to_string(),
                u32::from(effective_port),
                u32::from(advertised_status_port),
            )
            .with_version(config.server_version.clone());
            let mut cfg = SyncConfig::new(args.pd.clone(), info, dag.clone());
            cfg.limits.schema_lease = args.lease;
            cfg.limits.max_staleness = args
                .schema_sync_max_staleness
                .map_or(args.lease, Duration::from_secs_f64);
            cfg.stall_file = args.schema_sync_stall_file.clone();
            // The new stack's InfoSchema (the ported domain) only when the new stack is on.
            cfg.ported_domain = new_stack.uses_ported_domain();
            let sync = SchemaSync::start(kv.clone(), cfg)
                .await
                .map_err(|e| std::io::Error::other(format!("start schema sync: {e}")))?;
            Some(sync)
        } else {
            tidb_base::log_info!(
                "schema sync is off: every statement checks the schema version (ADR 0003)",
            );
            None
        };
        let priv_sync = poc_domain::PrivilegeSync::start(kv.clone(), args.pd.clone())
            .await
            .map_err(|e| std::io::Error::other(format!("load mysql.* privilege tables: {e}")))?;
        let factory = Factory::new(Shared {
            kv,
            schema,
            dag,
            sync: sync.clone(),
            privileges: Some(poc_session::privilege::PrivilegeSource::from_sync(
                priv_sync.clone(),
            )),
            user: args.user.clone(),
            credential: Credential::native_from_password(&args.password),
            server_version: config.server_version.clone(),
            tls_enabled: config.tls.is_some(),
        })
        .with_new_stack(new_stack)
        .map_err(std::io::Error::other)?;
        if new_stack != poc_session::router::NewStackMode::Off {
            let mode_str = format!("{new_stack:?}");
            tidb_base::log_info!(
                "VS1 router on: VS1-shaped SELECTs go to the new stack",
                "mode" => mode_str.as_str(),
            );
        }
        if args.route_trace || std::env::var("TIDB_RS_ROUTE_TRACE").is_ok_and(|v| v == "1") {
            poc_session::router::trace::enable();
            tidb_base::log_info!("route trace on: GET /debug/route?conn=<id> on the status port",);
        }
        if args.stmt_timing > 0 {
            poc_session::timing::set_enabled(true);
            let every = std::time::Duration::from_secs(args.stmt_timing);
            pool.spawn("stmt-timing", async move {
                let mut tick = tokio::time::interval(every);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    let report = poc_session::timing::report();
                    tidb_base::log_info!("statement timing", "report" => &report);
                }
            });
        }
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let (stop_status, status_stopped) = tokio::sync::oneshot::channel::<()>();
        let (status_done_tx, status_done_rx) = tokio::sync::oneshot::channel::<()>();
        let has_status_server = status_listener.is_some();
        if let Some(status_listener) = status_listener {
            let info = status::StatusInfo {
                version: config.server_version.clone(),
                git_hash: tidb_log::TIDB_GIT_HASH.to_owned(),
                listen_host: effective_status_host.clone(),
            };
            pool.spawn("status-server", async move {
                status::serve(status_listener, info, async move {
                    let _ = status_stopped.await;
                })
                .await;
                let _ = status_done_tx.send(());
            });
        }
        let shutdown = async {
            let mut sig_term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            let mut sig_hup =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
            let mut sig_quit =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit())?;
            let sig_name = tokio::select! {
                r = tokio::signal::ctrl_c() => {
                    r?;
                    "interrupt"
                }
                _ = sig_term.recv() => "terminated",
                _ = sig_hup.recv() => "hangup",
                _ = sig_quit.recv() => "quit",
            };
            tidb_base::log_info!("got signal to exit", "signal" => sig_name);
            Ok::<(), std::io::Error>(())
        };
        let shutdown = async {
            if let Err(e) = shutdown.await {
                let err_str = e.to_string();
                tidb_base::log_warn!(
                    "signal handling failed; shutting down",
                    "error" => err_str.as_str(),
                );
            }
            tidb_base::log_info!("setting tidb-server to report unhealthy (shutting-down)");
            // Stop the status/RPC server and wait for its shutdown WARNs before draining
            // client connections (matches Go shutdown ordering, spec §4.2).
            let _ = stop_status.send(());
            if has_status_server {
                let _ = tokio::time::timeout(Duration::from_secs(5), status_done_rx).await;
            }
            // Leave the cluster before draining clients, so DDL stops waiting for this node at once.
            priv_sync.shutdown();
            if let Some(sync) = &sync {
                sync.shutdown().await;
            }
        };
        let serve_res = Server::new(config, factory)
            .with_runtime(pool, admission)
            .serve(listener, shutdown)
            .await;
        // TODO(audit-plugin #448): Go calls plugin.Shutdown(context.Background()) here (cmd/tidb-server/main.go:926).
        serve_res
    });
    let _ = tidb_log::sync();
    if let Err(e) = result {
        let addr_str = addr.to_string();
        let err_str = e.to_string();
        tidb_base::log_error!(
            "server failed",
            "addr" => addr_str.as_str(),
            "error" => err_str.as_str(),
        );
        let _ = tidb_log::sync();
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

/// The IP published in the server info: --advertise-address, else `host` unless it is a
/// wildcard, else the local address of a route to the first PD endpoint.
fn advertise_ip(args: &Args, host: &str) -> IpAddr {
    if let Some(ip) = args.advertise_address {
        return ip;
    }
    if let Ok(ip) = host.parse::<IpAddr>()
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
        tidb_base::log_info!(
            "mysql protocol server secure connection is enabled",
            "client verification enabled" => args.ssl_ca.is_some(),
        );
        return Ok(Some(config));
    }
    if !args.auto_tls {
        tidb_base::log_info!("TLS is disabled (no --ssl-cert/--ssl-key and --auto-tls=false)",);
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
    tidb_base::log_info!(
        "mysql protocol server secure connection is enabled",
        "client verification enabled" => false,
    );
    Ok(Some(config))
}
