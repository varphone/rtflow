use clap::Parser;
use rtflow_testkit::example::{
    TransportArg, init_tracing,
    live_tools::{GstreamerPlayConfig, ManagedChild, require_gstreamer},
};
use std::{path::PathBuf, time::Duration};
use url::Url;

/// Play or validate an H.264/AAC RTSP stream with GStreamer.
#[derive(Debug, Parser)]
struct Args {
    #[arg(default_value = "rtsp://127.0.0.1:8554/ingest")]
    url: Url,
    #[arg(long, value_enum, default_value_t = TransportArg::Tcp)]
    transport: TransportArg,
    #[arg(long, default_value_t = 100)]
    latency_ms: u32,
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    audio: bool,
    /// Depayload into fakesink instead of opening video and audio devices.
    #[arg(long)]
    headless: bool,
    /// Stop after this many seconds; zero runs until Ctrl-C.
    #[arg(long, default_value_t = 0)]
    duration: u64,
    #[arg(long, default_value = "gst-launch-1.0")]
    gst_launch_bin: PathBuf,
    #[arg(long, default_value = "gst-inspect-1.0")]
    gst_inspect_bin: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "logging")]
    rtflow::logging::init();
    init_tracing();
    let args = Args::parse();
    let config = GstreamerPlayConfig {
        executable: args.gst_launch_bin,
        inspect_executable: args.gst_inspect_bin,
        url: args.url,
        transport: args.transport,
        latency_ms: args.latency_ms,
        audio: args.audio,
        headless: args.headless,
    };
    require_gstreamer(&config)?;
    let mut command = config.command();
    tracing::info!(
        url = %config.url,
        transport = config.transport.as_rtsp_name(),
        latency_ms = config.latency_ms,
        audio = config.audio,
        headless = config.headless,
        "starting GStreamer RTSP player"
    );
    let mut child = ManagedChild::spawn("gst-launch-1.0", &mut command)?;
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
                Err(format!("GStreamer exited with {status}").into())
            }
        }
    };
    child.terminate().await;
    outcome
}
