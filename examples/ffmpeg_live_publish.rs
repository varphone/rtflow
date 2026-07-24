mod common;

use clap::Parser;
use common::{
    TransportArg, init_tracing,
    live_tools::{FfmpegLiveConfig, ManagedChild, VideoSize, require_ffmpeg},
};
use std::{path::PathBuf, time::Duration};
use url::Url;

/// Generate a live H.264/AAC test stream with FFmpeg and publish it over RTSP.
#[derive(Debug, Parser)]
struct Args {
    #[arg(default_value = "rtsp://127.0.0.1:8554/ingest")]
    url: Url,
    #[arg(long, value_enum, default_value_t = TransportArg::Tcp)]
    transport: TransportArg,
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
    #[arg(long, default_value_t = 1_000)]
    tone_hz: u32,
    /// Stop after this many seconds; zero runs until Ctrl-C.
    #[arg(long, default_value_t = 0)]
    duration: u64,
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg_bin: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let args = Args::parse();
    require_ffmpeg(&args.ffmpeg_bin, args.audio)?;
    let config = FfmpegLiveConfig {
        executable: args.ffmpeg_bin,
        url: args.url,
        transport: args.transport,
        size: args.size,
        fps: args.fps,
        gop: args.gop,
        video_bitrate_kbps: args.video_bitrate_kbps,
        audio: args.audio,
        tone_hz: args.tone_hz,
    };
    let mut command = config.command()?;
    tracing::info!(
        url = %config.url,
        transport = config.transport.as_rtsp_name(),
        size = %config.size,
        fps = config.fps,
        audio = config.audio,
        "starting FFmpeg live publisher"
    );
    let mut child = ManagedChild::spawn("ffmpeg", &mut command)?;
    let timer = async {
        if args.duration == 0 {
            std::future::pending::<()>().await;
        } else {
            tokio::time::sleep(Duration::from_secs(args.duration)).await;
        }
    };
    tokio::pin!(timer);

    let outcome = tokio::select! {
        result = tokio::signal::ctrl_c() => {
            result?;
            Ok(())
        }
        _ = &mut timer => Ok(()),
        status = child.wait() => {
            let status = status?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("FFmpeg exited with {status}").into())
            }
        }
    };
    child.terminate().await;
    outcome
}
