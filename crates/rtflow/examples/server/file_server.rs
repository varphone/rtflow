use clap::Parser;
use rtflow::{FileSource, FileSourceOptions, LoopMode, MountConfig, RtspServer, ServerConfig};
use rtflow_testkit::example::{advertised_url, init_tracing, mount_path};
use std::{net::SocketAddr, path::PathBuf, time::Duration};

/// Stream a supported media file through a shared RTSP timeline.
#[derive(Debug, Parser)]
struct Args {
    media_file: PathBuf,
    #[arg(long, default_value = "0.0.0.0:8554")]
    listen: SocketAddr,
    #[arg(long, default_value = "/movie")]
    mount: String,
    /// Total plays; zero means loop forever.
    #[arg(long, default_value_t = 0)]
    plays: u32,
    #[arg(long, default_value_t = 1.0)]
    rate: f64,
    #[arg(long, default_value_t = 0.0)]
    start_seconds: f64,
    #[arg(long, default_value_t = 8)]
    prefetch: usize,
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pause_when_idle: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "logging")]
    rtflow::logging::init();
    init_tracing();
    let args = Args::parse();
    let mount_path = mount_path(&args.mount)?;
    if !args.start_seconds.is_finite() || args.start_seconds < 0.0 {
        return Err("--start-seconds must be a finite non-negative value".into());
    }
    let loop_mode = match args.plays {
        0 => LoopMode::Forever,
        1 => LoopMode::Once,
        count => LoopMode::Repeat(count - 1),
    };
    let source = FileSource::open_async(
        &args.media_file,
        FileSourceOptions {
            loop_mode,
            playback_rate: args.rate,
            start_position: Duration::from_secs_f64(args.start_seconds),
            pause_when_idle: args.pause_when_idle,
            prefetch: args.prefetch,
            ..Default::default()
        },
    )
    .await?;
    let server = RtspServer::bind(ServerConfig::new(args.listen)).await?;
    let mount = server
        .register_mount(
            mount_path,
            MountConfig::Local {
                description: source.description().clone(),
            },
        )
        .await?;
    tracing::info!(
        url = %advertised_url(server.local_addr(), &args.mount),
        file = %args.media_file.display(),
        duration = ?source.duration(),
        "file source ready"
    );
    let playback = source.play(mount.publisher()?);
    tokio::select! {
        result = playback.wait() => {
            let stats = result?;
            tracing::info!(?stats, "file playback completed");
        }
        result = tokio::signal::ctrl_c() => {
            result?;
            tracing::info!("shutdown requested");
        }
    }
    server.shutdown().await?;
    Ok(())
}
