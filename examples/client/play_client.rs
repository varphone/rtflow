use clap::Parser;
use rtflow::{
    AccessUnit, Codec,
    client::{ClientOptions, ClientReconnectPolicy, RtspClient},
};
use rtflow_testkit::example::{TransportArg, credentials, init_tracing};
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
    /// Reconnect automatically after the media session drops.
    #[arg(long)]
    reconnect: bool,
}

/// Shared per-frame bookkeeping for both plain and reconnecting sessions.
struct Consumer {
    args: Args,
    counts: BTreeMap<u8, u64>,
    outputs: BTreeMap<u8, tokio::fs::File>,
    total: u64,
    started: tokio::time::Instant,
}

impl Consumer {
    fn new(args: Args) -> Self {
        Self {
            args,
            counts: BTreeMap::new(),
            outputs: BTreeMap::new(),
            total: 0,
            started: tokio::time::Instant::now(),
        }
    }

    fn deadline_reached(&self) -> bool {
        if self.args.max_frames > 0 && self.total >= self.args.max_frames {
            return true;
        }
        self.args.duration > 0 && self.started.elapsed() >= Duration::from_secs(self.args.duration)
    }

    async fn handle(&mut self, unit: AccessUnit) -> Result<(), Box<dyn std::error::Error>> {
        self.total += 1;
        *self.counts.entry(unit.track_id).or_default() += 1;
        tracing::debug!(
            track = unit.track_id,
            codec = ?unit.codec,
            bytes = unit.data.len(),
            pts = ?unit.pts,
            keyframe = unit.is_keyframe,
            "access unit"
        );
        if let Some(directory) = &self.args.output_dir {
            if let Entry::Vacant(entry) = self.outputs.entry(unit.track_id) {
                let extension = match unit.codec {
                    Codec::H264 => "h264",
                    Codec::H265 => "h265",
                    Codec::Aac => "aac",
                    Codec::Pcma => "pcma",
                    Codec::Pcmu => "pcmu",
                    Codec::Opus => "opus",
                    Codec::Vp8 => "vp8",
                    Codec::Vp9 => "vp9",
                    Codec::Av1 => "av1",
                    Codec::Jpeg => "mjpg",
                    Codec::H263 => "h263",
                    Codec::Mpeg4 => "m4v",
                    _ => "bin",
                };
                let path = directory.join(format!("track-{}.{}", unit.track_id, extension));
                let file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(path)
                    .await?;
                entry.insert(file);
            }
            self.outputs
                .get_mut(&unit.track_id)
                .unwrap()
                .write_all(&unit.data)
                .await?;
        }
        Ok(())
    }

    fn finish(self) {
        tracing::info!(
            frames = self.total,
            tracks = ?self.counts,
            elapsed = ?self.started.elapsed(),
            "client complete"
        );
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let args = Args::parse();
    let mut builder = ClientOptions::builder().transport(args.transport.into());
    if let Some(creds) = credentials(args.username.clone(), args.password.clone())? {
        builder = builder.credentials(creds);
    }
    let options = builder.build()?;
    if let Some(directory) = &args.output_dir {
        tokio::fs::create_dir_all(directory).await?;
    }

    if args.reconnect {
        let mut session = RtspClient::connect_reconnecting(
            args.url.clone(),
            options,
            ClientReconnectPolicy::default(),
        )
        .await?;
        for track in &session.description.tracks {
            tracing::info!(
                track = track.id,
                codec = ?track.codec,
                clock_rate = track.clock_rate,
                "track discovered"
            );
        }
        let mut consumer = Consumer::new(args);
        while !consumer.deadline_reached() {
            tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    result?;
                    break;
                }
                next = session.next() => {
                    let Some(unit) = next else { break };
                    consumer.handle(unit?).await?;
                }
            }
        }
        consumer.finish();
        session.teardown().await;
        return Ok(());
    }

    let mut session = RtspClient::connect(args.url.clone(), options).await?;
    for track in &session.description.tracks {
        tracing::info!(
            track = track.id,
            codec = ?track.codec,
            clock_rate = track.clock_rate,
            "track discovered"
        );
    }
    let mut consumer = Consumer::new(args);
    let mut quality_tick = tokio::time::interval(Duration::from_secs(5));
    quality_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if consumer.deadline_reached() {
            break;
        }
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
                break;
            }
            _ = quality_tick.tick() => {
                let snapshot = session.quality_snapshot().await;
                for (track, reception) in &snapshot.tracks {
                    tracing::info!(
                        track = *track,
                        packets = reception.packets_received,
                        lost = reception.packets_lost,
                        "quality snapshot"
                    );
                }
            }
            next = session.next() => {
                let Some(unit) = next else { break };
                consumer.handle(unit?).await?;
            }
        }
    }
    consumer.finish();
    let _ = session.teardown().await;
    Ok(())
}
