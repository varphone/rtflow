use clap::Parser;
use rtflow::{MountConfig, RecordingVodSource, RtspServer, ServerConfig};
use rtflow_testkit::example::{advertised_url, init_tracing, mount_path};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

/// Serve a segmented recording directory with per-session NPT seek.
#[derive(Debug, Parser)]
struct Args {
    recording_directory: PathBuf,
    #[arg(long, default_value = "0.0.0.0:8554")]
    listen: SocketAddr,
    #[arg(long, default_value = "/archive")]
    mount: String,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    max_sample_size: usize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "logging")]
    rtflow::logging::init();
    init_tracing();
    let args = Args::parse();
    let source = Arc::new(RecordingVodSource::open(
        &args.recording_directory,
        args.max_sample_size,
    )?);
    let server = RtspServer::bind(ServerConfig::new(args.listen)).await?;
    server
        .register_mount(mount_path(&args.mount)?, MountConfig::Vod { source })
        .await?;
    tracing::info!(
        url = %advertised_url(server.local_addr(), &args.mount),
        directory = %args.recording_directory.display(),
        "recording VOD ready"
    );
    tokio::signal::ctrl_c().await?;
    server.shutdown().await?;
    Ok(())
}
