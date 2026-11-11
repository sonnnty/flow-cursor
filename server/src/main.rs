use anyhow::Result;
use clap::Parser;
use flow_server::{start, HubConfig};
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing::{info, warn};
use tracing_subscriber::{prelude::*, reload, EnvFilter};

/// FlowCursor hub: run it on the home server (Docker, host networking).
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// State directory (identity, nodes, layout, settings).
    #[arg(long, env = "FLOW_DATA", default_value = "/data")]
    data: PathBuf,
    /// QUIC (UDP) listen address for agents.
    #[arg(long, env = "FLOW_QUIC_ADDR", default_value = "0.0.0.0:7878")]
    quic: SocketAddr,
    /// Dashboard (HTTP) listen address.
    #[arg(long, env = "FLOW_HTTP_ADDR", default_value = "0.0.0.0:7880")]
    http: SocketAddr,
    /// Dashboard token. If unset a random token is generated, stored in the
    /// data directory and printed at startup.
    #[arg(long, env = "FLOW_ADMIN_TOKEN", hide_env_values = true)]
    admin_token: Option<String>,
    /// Disable mDNS advertisement.
    #[arg(long, env = "FLOW_NO_MDNS", value_parser = clap::builder::BoolishValueParser::new())]
    no_mdns: bool,
    /// Hub name (mDNS instance name).
    #[arg(long, env = "FLOW_NAME")]
    name: Option<String>,
    /// Print the join token and exit.
    #[arg(long)]
    print_token: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let initial = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
    let (filter, handle) = reload::Layer::new(EnvFilter::try_new(&initial).unwrap_or_else(|_| EnvFilter::new("info")));
    tracing_subscriber::registry().with(filter).with(tracing_subscriber::fmt::layer()).init();

    let name = args.name.clone().unwrap_or_else(|| {
        hostname::get().ok().and_then(|h| h.into_string().ok()).unwrap_or_else(|| "flowcursor".into())
    });
    let name = format!("FlowCursor hub on {name}");
    if args.print_token {
        println!("{}", flow_server::join_token_from_dir(&args.data, &name)?);
        return Ok(());
    }
    let cfg = HubConfig {
        data_dir: args.data.clone(),
        quic_addr: args.quic,
        http_addr: Some(args.http),
        admin_token: args.admin_token.clone(),
        mdns: !args.no_mdns,
        name,
    };
    let hub = start(cfg).await?;
    {
        let level = hub.hub.lock().p.settings.log_level.clone();
        if std::env::var("RUST_LOG").is_err() {
            let _ = handle.modify(|f| *f = EnvFilter::try_new(&level).unwrap_or_else(|_| EnvFilter::new("info")));
        }
        let h = handle.clone();
        *hub.hub.on_log_level.lock().expect("poisoned") =
            Some(Box::new(move |lvl: &str| match EnvFilter::try_new(lvl) {
                Ok(f) => {
                    let _ = h.modify(|old| *old = f);
                }
                Err(e) => warn!("invalid log level {lvl:?}: {e}"),
            }));
    }
    info!("hub fingerprint: {}", hub.fingerprint());
    info!("join token (give this to `flow-agent join`): {}", hub.join_token());
    if args.admin_token.is_none() {
        info!("dashboard token: {}", hub.hub.admin_token);
    }
    tokio::signal::ctrl_c().await.ok();
    info!("shutting down");
    hub.shutdown().await;
    Ok(())
}
