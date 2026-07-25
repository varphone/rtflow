mod common;

use clap::Parser;
use common::{TransportArg, credentials, init_tracing};
use rtflow::{
    Codec,
    client::{ClientOptions, RtspClient},
};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    path::PathBuf,
    time::Duration,
};
use tokio::io::AsyncWriteExt;

/// Receive encoded access units from an RTSP URL.
#[derive(Debug, Parser)]
struct Args {
    url: url::Url,
    #[arg(long, value_enum, default_value_t = TransportArg::Udp)]
    transport: TransportArg,
    #[arg(long)]
    username: Option<String>,
    #[arg(long)]
    password: Option<String>,
    #[arg(long, default_value_t = 0)]
    max_frames: u64,
    #[arg(long, default_value_t = 0)]
    duration: u64,
    #[arg(long)]
    output_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let args = Args::parse();
    let options = ClientOptions {
        transport: args.transport.into(),
        credentials: credentials(args.username, args.password)?,
        ..Default::default()
    };
    let mut session = RtspClient::connect(args.url, options).await?;
    for track in &session.description.tracks {
        tracing::info!(
            track = track.id,
            codec = ?track.codec,
            clock_rate = track.clock_rate,
            "track discovered"
        );
    }
    if let Some(directory) = &args.output_dir {
        tokio::fs::create_dir_all(directory).await?;
    }

    let started = tokio::time::Instant::now();
    let mut counts = BTreeMap::<u8, u64>::new();
    let mut outputs = BTreeMap::<u8, tokio::fs::File>::new();
    let mut total = 0u64;
    loop {
        let remaining = if args.duration == 0 {
            Duration::from_secs(365 * 24 * 60 * 60)
        } else {
            Duration::from_secs(args.duration).saturating_sub(started.elapsed())
        };
        if remaining.is_zero() || (args.max_frames > 0 && total >= args.max_frames) {
            break;
        }
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
                break;
            }
            _ = tokio::time::sleep(remaining) => break,
            next = session.next() => {
                let Some(unit) = next else { break };
                let unit = unit?;
                total += 1;
                *counts.entry(unit.track_id).or_default() += 1;
                tracing::debug!(
                    track = unit.track_id,
                    codec = ?unit.codec,
                    bytes = unit.data.len(),
                    pts = ?unit.pts,
                    keyframe = unit.is_keyframe,
                    "access unit"
                );
                if let Some(directory) = &args.output_dir {
                    if let Entry::Vacant(entry) = outputs.entry(unit.track_id) {
                        let extension = match unit.codec {
                            Codec::H264 => "h264",
                            Codec::H265 => "h265",
                            Codec::Aac => "aac",
                            Codec::Pcma => "pcma",
                            Codec::Pcmu => "pcmu",
                        };
                        let path =
                            directory.join(format!("track-{}.{}", unit.track_id, extension));
                        let file = tokio::fs::OpenOptions::new()
                            .create(true)
                            .truncate(true)
                            .write(true)
                            .open(path)
                            .await?;
                        entry.insert(file);
                    }
                    outputs
                        .get_mut(&unit.track_id)
                        .unwrap()
                        .write_all(&unit.data)
                        .await?;
                }
            }
        }
    }
    tracing::info!(
        frames = total,
        tracks = ?counts,
        elapsed = ?started.elapsed(),
        "client complete"
    );
    let _ = session.teardown().await;
    Ok(())
}
