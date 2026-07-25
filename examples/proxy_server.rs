mod common;

use clap::Parser;
use common::{TransportArg, advertised_url, credentials, init_tracing, mount_path};
use rtflow::{
    MountConfig, RtspServer, ServerConfig,
    proxy::{ReconnectPolicy, UpstreamConfig},
};
use std::{net::SocketAddr, time::Duration};

/// Expose an on-demand RTSP proxy mount for an upstream stream.
#[derive(Debug, Parser)]
struct Args {
    upstream: url::Url,
    #[arg(long, default_value = "0.0.0.0:8554")]
    listen: SocketAddr,
    #[arg(long, default_value = "/proxy")]
    mount: String,
    #[arg(long, value_enum, default_value_t = TransportArg::Tcp)]
    upstream_transport: TransportArg,
    #[arg(long)]
    username: Option<String>,
    #[arg(long)]
    password: Option<String>,
    #[arg(long, default_value_t = 250)]
    retry_initial_ms: u64,
    #[arg(long, default_value_t = 30_000)]
    retry_max_ms: u64,
    #[arg(long, default_value_t = 5_000)]
    idle_grace_ms: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let args = Args::parse();
    let mut upstream = UpstreamConfig::new(args.upstream, args.upstream_transport.into());
    upstream.credentials = credentials(args.username, args.password)?;
    upstream.reconnect = ReconnectPolicy {
        initial_delay: Duration::from_millis(args.retry_initial_ms),
        max_delay: Duration::from_millis(args.retry_max_ms),
        ..Default::default()
    };
    let mut server_config = ServerConfig::new(args.listen);
    server_config.no_subscriber_grace = Duration::from_millis(args.idle_grace_ms);
    let server = RtspServer::bind(server_config).await?;
    let mount = server
        .register_mount(
            mount_path(&args.mount)?,
            MountConfig::Proxy {
                upstream: Box::new(upstream),
            },
        )
        .await?;
    tracing::info!(
        url = %advertised_url(server.local_addr(), &args.mount),
        "proxy ready"
    );
    let mut status = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
                break;
            }
            _ = status.tick() => {
                let snapshot = mount.snapshot();
                tracing::info!(
                    state = ?snapshot.proxy_state,
                    subscribers = snapshot.subscriber_count,
                    packets = snapshot.stats.rtp_packets,
                    "proxy status"
                );
            }
        }
    }
    server.shutdown().await?;
    Ok(())
}
