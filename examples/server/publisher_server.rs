use clap::Parser;
use rtflow::{MountConfig, RtspServer, ServerConfig};
use rtflow_testkit::example::{
    CodecArg, advertised_url, init_tracing, mount_path, publisher_description,
};
use std::{collections::HashSet, net::SocketAddr, time::Duration};

/// Accept ANNOUNCE/SETUP/RECORD from an external RTSP publisher.
#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8554")]
    listen: SocketAddr,
    #[arg(long, default_value = "/ingest")]
    mount: String,
    #[arg(long, value_delimiter = ',', default_values = ["h264", "aac"])]
    codecs: Vec<CodecArg>,
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    allow_replace: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let args = Args::parse();
    if args.codecs.is_empty() {
        return Err("at least one codec is required".into());
    }
    let server = RtspServer::bind(ServerConfig::new(args.listen)).await?;
    if args.codecs.iter().copied().collect::<HashSet<_>>().len() != args.codecs.len() {
        return Err("--codecs must not contain duplicates".into());
    }
    let mount = server
        .register_mount(
            mount_path(&args.mount)?,
            MountConfig::Publisher {
                description: publisher_description("external-publisher", &args.codecs),
                allow_replace_after_disconnect: args.allow_replace,
            },
        )
        .await?;
    let url = advertised_url(server.local_addr(), &args.mount);
    tracing::info!(%url, "publisher mount ready");
    println!("FFmpeg TCP publish: ffmpeg -re -i INPUT -c copy -rtsp_transport tcp -f rtsp {url}");
    println!("FFmpeg UDP publish: ffmpeg -re -i INPUT -c copy -rtsp_transport udp -f rtsp {url}");
    println!("Dynamic H.264/AAC publish: cargo run --example ffmpeg_live_publish -- {url}");
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
                    publisher = snapshot.publisher_connected,
                    subscribers = snapshot.subscriber_count,
                    accepted = snapshot.stats.accepted_frames,
                    "publisher status"
                );
            }
        }
    }
    server.shutdown().await?;
    Ok(())
}
