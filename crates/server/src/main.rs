//! Read-only TiDB-compatible SQL node (prototype). See docs/adr/0001.
//!
//! For now the MySQL protocol layer is wired to a stub session handler (`stub.rs`) that only
//! answers a few constant statements; the SQL layer replaces it in later tasks.

#![forbid(unsafe_code)]

mod stub;

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use tidb_mysql_protocol::constants::auth_plugin;
use tidb_mysql_protocol::tls::{self, TlsConfig};
use tidb_mysql_protocol::{Credential, Server, ServerConfig};
use tracing::{error, info};

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
    let factory = stub::StubFactory {
        user: args.user.clone(),
        credential: Credential::native_from_password(&args.password),
        server_version: config.server_version.clone(),
        tls_enabled: config.tls.is_some(),
    };

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
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let shutdown = async {
            if tokio::signal::ctrl_c().await.is_ok() {
                info!("received Ctrl-C");
            }
        };
        Server::new(config, factory).serve(listener, shutdown).await
    });
    if let Err(e) = result {
        error!(%addr, error = %e, "server failed");
        std::process::exit(1);
    }
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
