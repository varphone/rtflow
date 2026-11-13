use clap::Parser;
use rtflow::{
    MountConfig, RtspServer, ServerConfig,
    source::{SyntheticSource, SyntheticSourceConfig},
};
use rtflow_testkit::example::{CodecArg, advertised_url, description, init_tracing, mount_path};
use std::{collections::HashSet, net::SocketAddr};

/// Publish mock hardware-encoder access units into a local RTSP mount.
#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8554")]
    listen: SocketAddr,
    #[arg(long, default_value = "/camera")]
    mount: String,
    #[arg(long, value_delimiter = ',', default_values = ["h264", "pcma"])]
    codecs: Vec<CodecArg>,
    #[arg(long, default_value_t = 25)]
    fps: u32,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let args = Args::parse();
    if args.fps == 0 || args.fps > 1_000 || args.codecs.is_empty() {
        return Err("--fps must be in 1..=1000 and at least one codec is required".into());
    }
    if args.codecs.iter().copied().collect::<HashSet<_>>().len() != args.codecs.len() {
        return Err("--codecs must not contain duplicates".into());
    }
    let mount_path = mount_path(&args.mount)?;
    let media = description("mock-device", &args.codecs);
    let server = RtspServer::bind(ServerConfig::new(args.listen)).await?;
    let mount = server
        .register_mount(
            mount_path,
            MountConfig::Local {
                description: media.clone(),
            },
        )
        .await?;
    let publisher = mount.publisher()?;
    tracing::info!(url = %advertised_url(server.local_addr(), &args.mount), "local source ready");

    let handle = SyntheticSource::spawn(
        publisher,
        media,
        SyntheticSourceConfig {
            fps: args.fps,
            keyframe_interval: args.fps.max(1),
        },
    )?;
    tokio::signal::ctrl_c().await?;
    handle.abort();
    let _ = handle.await;

    let snapshot = mount.snapshot();
    tracing::info!(
        accepted = snapshot.stats.accepted_frames,
        dropped = snapshot.stats.dropped_frames,
        "stopping local source"
    );
    mount.publisher()?.close().await?;
    server.shutdown().await?;
    Ok(())
}
