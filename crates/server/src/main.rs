//! Read-only TiDB-compatible SQL node (prototype). See docs/adr/0001.
//!
//! For now the MySQL protocol layer is wired to a stub session handler (`stub.rs`) that only
//! answers a few constant statements; the SQL layer replaces it in later tasks.

#![forbid(unsafe_code)]

mod stub;

use std::net::SocketAddr;

use clap::Parser;
use tidb_mysql_protocol::constants::auth_plugin;
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
    let config = ServerConfig {
        default_auth_plugin: args.default_auth_plugin.clone(),
        ..ServerConfig::default()
    };
    let factory = stub::StubFactory {
        user: args.user.clone(),
        credential: Credential::native_from_password(&args.password),
        server_version: config.server_version.clone(),
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
