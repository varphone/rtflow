mod common;

use clap::Parser;
use common::{
    CodecArg, TransportArg, advertised_url, init_tracing,
    live_tools::{
        FfmpegLiveConfig, GstreamerPlayConfig, ManagedChild, VideoSize, require_ffmpeg,
        require_gstreamer,
    },
    mount_path, publisher_description,
};
use rtflow::{MountConfig, RtspServer, ServerConfig};
use std::{net::SocketAddr, num::NonZeroU64, path::PathBuf, time::Duration};
use url::Url;

/// Run an FFmpeg -> Rust RTSP server -> GStreamer live streaming demo.
#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,
    #[arg(long, default_value = "/live")]
    mount: String,
    #[arg(long, value_enum, default_value_t = TransportArg::Tcp)]
    publish_transport: TransportArg,
    #[arg(long, value_enum, default_value_t = TransportArg::Tcp)]
    play_transport: TransportArg,
    #[arg(long, default_value = "1280x720")]
    size: VideoSize,
    #[arg(long, default_value_t = 25)]
    fps: u32,
    #[arg(long, default_value_t = 25)]
    gop: u32,
    #[arg(long, default_value_t = 2_000)]
    video_bitrate_kbps: u32,
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    audio: bool,
    #[arg(long)]
    play_audio: bool,
    #[arg(long, default_value_t = 1_000)]
    tone_hz: u32,
    #[arg(long, default_value_t = 100)]
    latency_ms: u32,
    #[arg(long)]
    headless: bool,
    /// Stop after this many seconds; zero runs until Ctrl-C.
    #[arg(long, default_value_t = 0)]
    duration: u64,
    /// RTSP control-session lease advertised to players that do not send keep-alives.
    #[arg(long, default_value = "86400")]
    session_timeout_secs: NonZeroU64,
    #[arg(long, default_value_t = 10)]
    startup_timeout: u64,
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg_bin: PathBuf,
    #[arg(long, default_value = "gst-launch-1.0")]
    gst_launch_bin: PathBuf,
    #[arg(long, default_value = "gst-inspect-1.0")]
    gst_inspect_bin: PathBuf,
}

async fn wait_for_state(
    child: &mut ManagedChild,
    timeout: Duration,
    mut ready: impl FnMut() -> bool,
    state: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if ready() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!("{} exited with {status} before {state}", child.name()).into());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("timed out waiting for {state}").into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let args = Args::parse();
    if args.play_audio && !args.audio {
        return Err("--play-audio requires --audio true".into());
    }
    if args.startup_timeout == 0 {
        return Err("--startup-timeout must be greater than zero".into());
    }
    let mount_path = mount_path(&args.mount)?;
    let session_timeout = Duration::from_secs(args.session_timeout_secs.get());
    let server = RtspServer::bind(
        ServerConfig::builder(args.listen)
            .session_timeout(session_timeout)
            .build()?,
    )
    .await?;
    let codecs = if args.audio {
        vec![CodecArg::H264, CodecArg::Aac]
    } else {
        vec![CodecArg::H264]
    };
    let mount = server
        .register_mount(
            mount_path,
            MountConfig::Publisher {
                description: publisher_description("live-tool-demo", &codecs),
                allow_replace_after_disconnect: true,
            },
        )
        .await?;
    let url = Url::parse(&advertised_url(server.local_addr(), &args.mount))?;
    let ffmpeg_config = FfmpegLiveConfig {
        executable: args.ffmpeg_bin,
        url: url.clone(),
        transport: args.publish_transport,
        size: args.size,
        fps: args.fps,
        gop: args.gop,
        video_bitrate_kbps: args.video_bitrate_kbps,
        audio: args.audio,
        tone_hz: args.tone_hz,
    };
    let gstreamer_config = GstreamerPlayConfig {
        executable: args.gst_launch_bin,
        inspect_executable: args.gst_inspect_bin,
        url: url.clone(),
        transport: args.play_transport,
        latency_ms: args.latency_ms,
        audio: args.play_audio,
        headless: args.headless,
    };
    require_ffmpeg(&ffmpeg_config.executable, args.audio)?;
    require_gstreamer(&gstreamer_config)?;

    tracing::info!(
        %url,
        session_timeout_secs = args.session_timeout_secs.get(),
        "RTSP publisher mount ready"
    );
    let mut ffmpeg_command = ffmpeg_config.command()?;
    let mut ffmpeg = ManagedChild::spawn("ffmpeg", &mut ffmpeg_command)?;
    let timeout = Duration::from_secs(args.startup_timeout);
    if let Err(error) = wait_for_state(
        &mut ffmpeg,
        timeout,
        || mount.snapshot().publisher_connected,
        "RTSP publisher connection",
    )
    .await
    {
        ffmpeg.terminate().await;
        server.shutdown().await?;
        return Err(error);
    }
    tracing::info!("FFmpeg publisher connected");

    let mut gstreamer_command = gstreamer_config.command();
    let mut gstreamer = match ManagedChild::spawn("gst-launch-1.0", &mut gstreamer_command) {
        Ok(child) => child,
        Err(error) => {
            ffmpeg.terminate().await;
            server.shutdown().await?;
            return Err(error.into());
        }
    };
    if let Err(error) = wait_for_state(
        &mut gstreamer,
        timeout,
        || mount.snapshot().subscriber_count > 0,
        "GStreamer subscriber connection",
    )
    .await
    {
        gstreamer.terminate().await;
        ffmpeg.terminate().await;
        server.shutdown().await?;
        return Err(error);
    }
    tracing::info!("GStreamer subscriber connected");

    let timer = async {
        if args.duration == 0 {
            std::future::pending::<()>().await;
        } else {
            tokio::time::sleep(Duration::from_secs(args.duration)).await;
        }
    };
    tokio::pin!(timer);
    let mut status = tokio::time::interval(Duration::from_secs(2));
    let outcome = loop {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
                break Ok(());
            }
            _ = &mut timer => break Ok(()),
            child_status = ffmpeg.wait() => {
                break Err(format!("FFmpeg exited unexpectedly with {}", child_status?).into());
            }
            child_status = gstreamer.wait() => {
                break Err(format!("GStreamer exited unexpectedly with {}", child_status?).into());
            }
            _ = status.tick() => {
                let snapshot = mount.snapshot();
                tracing::info!(
                    publisher = snapshot.publisher_connected,
                    subscribers = snapshot.subscriber_count,
                    accepted = snapshot.stats.accepted_frames,
                    dropped = snapshot.stats.dropped_frames,
                    "live stream status"
                );
            }
        }
    };

    gstreamer.terminate().await;
    ffmpeg.terminate().await;
    server.shutdown().await?;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_timeout_defaults_to_one_day() {
        let args = Args::try_parse_from(["live_stream_demo"]).unwrap();
        assert_eq!(args.session_timeout_secs.get(), 86_400);
    }

    #[test]
    fn session_timeout_rejects_zero() {
        assert!(Args::try_parse_from(["live_stream_demo", "--session-timeout-secs", "0"]).is_err());
    }
}
