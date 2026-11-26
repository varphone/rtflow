//! SRT push destinations: mux a mount to MPEG-TS and send it as SRT caller.
use crate::config::{Config, SrtPush};
use broadcast_common::Package;
use rtflow::{
    AccessUnit, Codec, MediaDescription, MediaKind, MountHandle, MountRegistry,
    media::TrackDescription,
    srt::{SrtCallerConfig, SrtSender},
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use transmux::{
    CodecConfig, Media, Sample, StreamingTsMux, Track, TrackId, TrackSpec,
    avc_config::AVCConfigurationBox,
    hevc_config::HEVCConfigurationBox,
    mp4esds::{
        DecoderConfigDescriptor, DecoderSpecificInfo, ESDescriptor, EsdsBox, SLConfigDescriptor,
    },
};

/// Runtime status of one configured push destination.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SrtPushStatus {
    pub mount: String,
    pub remote: String,
    pub stream_id: String,
    pub connected: bool,
    pub encrypted: bool,
    pub bytes_sent: u64,
    pub rtt_us: u32,
    pub last_error: Option<String>,
}

/// Build the status table for the configured push destinations.
pub fn status_table(config: &Config) -> Arc<std::sync::Mutex<Vec<SrtPushStatus>>> {
    Arc::new(std::sync::Mutex::new(
        config
            .srt
            .push
            .iter()
            .map(|push| SrtPushStatus {
                mount: push.mount.clone(),
                remote: push.remote.clone(),
                stream_id: push
                    .stream_id
                    .clone()
                    .unwrap_or_else(|| push.mount.trim_start_matches('/').to_owned()),
                ..SrtPushStatus::default()
            })
            .collect(),
    ))
}

/// Spawn one task per configured push destination.
pub fn spawn(
    config: Arc<Config>,
    mounts: Arc<MountRegistry>,
    cancel: CancellationToken,
    status: Arc<std::sync::Mutex<Vec<SrtPushStatus>>>,
) -> Vec<tokio::task::JoinHandle<()>> {
    config
        .srt
        .push
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, push)| {
            let mounts = mounts.clone();
            let cancel = cancel.clone();
            let base = config.clone();
            let status = status.clone();
            tokio::spawn(async move { run_push(push, index, base, mounts, cancel, status).await })
        })
        .collect()
}

fn update_status(
    status: &Arc<std::sync::Mutex<Vec<SrtPushStatus>>>,
    index: usize,
    update: impl FnOnce(&mut SrtPushStatus),
) {
    if let Some(entry) = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(index)
    {
        update(entry);
    }
}

async fn run_push(
    push: SrtPush,
    index: usize,
    config: Arc<Config>,
    mounts: Arc<MountRegistry>,
    cancel: CancellationToken,
    status: Arc<std::sync::Mutex<Vec<SrtPushStatus>>>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let passphrase = push
            .passphrase
            .clone()
            .or_else(|| config.srt.passphrase.clone());
        let pbkeylen = if push.pbkeylen == 0 {
            config.srt.pbkeylen
        } else {
            push.pbkeylen
        };
        let stream_id = push
            .stream_id
            .clone()
            .unwrap_or_else(|| push.mount.trim_start_matches('/').to_owned());
        let caller_config = SrtCallerConfig {
            remote: match push.remote.parse() {
                Ok(remote) => remote,
                Err(_) => {
                    tracing::error!(remote = %push.remote, "SRT push: invalid remote");
                    return;
                }
            },
            stream_id: Some(stream_id.clone()),
            latency_ms: push.latency_ms.unwrap_or(config.srt.latency_ms),
            passphrase,
            pbkeylen,
            connect_timeout: Duration::from_secs(5),
            peer_idle_timeout: config.srt.peer_idle_timeout,
            mtu: config.srt.mtu,
            tlpktdrop: config.srt.tlpktdrop,
            ..SrtCallerConfig::default()
        };

        // Wait for the source mount to exist with a usable description.
        let mut handle = None;
        while !cancel.is_cancelled() {
            if let Some(candidate) = mounts.get_handle(&push.mount) {
                let description = candidate.description();
                if !description.tracks.is_empty() {
                    handle = Some(candidate);
                    break;
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        }
        let Some(handle) = handle else {
            return;
        };

        match SrtSender::connect(caller_config).await {
            Ok(mut sender) => {
                backoff = Duration::from_secs(1);
                tracing::info!(
                    mount = %push.mount,
                    remote = %push.remote,
                    stream_id = %stream_id,
                    "SRT push: connected"
                );
                update_status(&status, index, |entry| {
                    entry.connected = true;
                    entry.encrypted = sender.encrypted();
                    entry.last_error = None;
                });
                if let Err(error) = pump(&mut sender, &handle, cancel.clone(), &status, index).await
                {
                    tracing::warn!(%error, mount = %push.mount, "SRT push: stream ended");
                    update_status(&status, index, |entry| {
                        entry.last_error = Some(error.to_string());
                    });
                }
                let stats = sender.stats();
                update_status(&status, index, |entry| {
                    entry.connected = false;
                    entry.bytes_sent = stats.bytes_sent;
                    entry.rtt_us = stats.rtt_us;
                });
                let _ = sender.close().await;
            }
            Err(error) => {
                let message = error.to_string();
                update_status(&status, index, |entry| {
                    entry.connected = false;
                    entry.last_error = Some(message.clone());
                });
                tracing::warn!(remote = %push.remote, %error, "SRT push: connect failed");
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn pump(
    sender: &mut SrtSender,
    mount: &MountHandle,
    cancel: CancellationToken,
    status: &Arc<std::sync::Mutex<Vec<SrtPushStatus>>>,
    index: usize,
) -> rtflow::Result<()> {
    let description = mount.description();
    let mut muxer = TsMuxer::new(&description).ok_or_else(|| {
        rtflow::Error::Media(format!(
            "SRT push: {} has no TS-carriable tracks",
            mount.path()
        ))
    })?;
    let (mut receiver, startup) = mount.subscribe_with_startup();
    let has_video = description
        .tracks
        .iter()
        .any(|track| track.kind == MediaKind::Video);
    let mut started = !has_video;

    // Startup snapshot first (it may already contain a keyframe).
    for unit in startup {
        if !started {
            if !unit.is_keyframe {
                continue;
            }
            started = true;
        }
        let bytes = muxer.push(&unit)?;
        if !bytes.is_empty() {
            sender.send(&bytes).await?;
        }
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let unit = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = ticker.tick() => {
                sender.tick().await?;
                continue;
            }
            result = receiver.recv() => match result {
                Ok(unit) => unit,
                Err(rtflow::FrameRecvError::Lagged(_)) => continue,
                Err(rtflow::FrameRecvError::Closed) => return Ok(()),
            },
        };
        if !started {
            if !unit.is_keyframe || !matches!(unit.codec, Codec::H264 | Codec::H265) {
                continue;
            }
            started = true;
        }
        let bytes = muxer.push(&unit)?;
        if !bytes.is_empty() {
            sender.send(&bytes).await?;
            let stats = sender.stats();
            update_status(status, index, |entry| {
                entry.bytes_sent = stats.bytes_sent;
                entry.rtt_us = stats.rtt_us;
            });
        }
        // Process ACK/NAK without blocking the media path.
        sender.tick().await?;
    }
}

/// Minimal MPEG-TS muxer for live mounts (H.264/H.265/AAC).
struct TsMuxer {
    mux: StreamingTsMux,
    specs: Vec<TrackSpec>,
    /// Media track id → index in `specs`.
    mapping: HashMap<u8, usize>,
    timescales: HashMap<u8, u32>,
    durations: HashMap<u8, u32>,
}

impl TsMuxer {
    fn new(description: &MediaDescription) -> Option<Self> {
        let mut specs = Vec::new();
        let mut mapping = HashMap::new();
        let mut timescales = HashMap::new();
        let mut durations = HashMap::new();
        for track in &description.tracks {
            let Some(spec) = track_spec(track) else {
                continue;
            };
            mapping.insert(track.id, specs.len());
            timescales.insert(track.id, track.clock_rate);
            let duration = match track.kind {
                MediaKind::Video => track
                    .frame_rate
                    .map(|(num, den)| {
                        (u64::from(track.clock_rate) * u64::from(den) / u64::from(num).max(1))
                            as u32
                    })
                    .unwrap_or(3_000),
                MediaKind::Audio => 1024,
            };
            durations.insert(track.id, duration.max(1));
            specs.push(spec);
        }
        if specs.is_empty() {
            return None;
        }
        Some(Self {
            mux: StreamingTsMux::new(),
            specs,
            mapping,
            timescales,
            durations,
        })
    }

    fn push(&mut self, unit: &AccessUnit) -> rtflow::Result<Vec<u8>> {
        let Some(&index) = self.mapping.get(&unit.track_id) else {
            return Ok(Vec::new());
        };
        let timescale = u64::from(
            self.timescales
                .get(&unit.track_id)
                .copied()
                .unwrap_or(90_000),
        );
        let ticks = |duration: Duration| -> i64 {
            (duration.as_secs() * timescale
                + u64::from(duration.subsec_nanos()) * timescale / 1_000_000_000) as i64
        };
        let duration = self.durations.get(&unit.track_id).copied().unwrap_or(3_000);
        let sample = if unit.codec == Codec::Aac {
            Sample::from_raw(
                unit.data.clone(),
                Some(ticks(unit.dts.unwrap_or(unit.pts))),
                Some(ticks(unit.pts)),
                Some(duration),
            )
        } else {
            Sample::from_annexb(
                &unit.data,
                Some(ticks(unit.dts.unwrap_or(unit.pts))),
                Some(ticks(unit.pts)),
                Some(duration),
                unit.is_keyframe,
            )
        };
        let Ok(sample) = sample else {
            return Ok(Vec::new());
        };

        let mut tracks = Vec::with_capacity(self.specs.len());
        for (track_index, spec) in self.specs.iter().enumerate() {
            let samples = if track_index == index {
                vec![sample.clone()]
            } else {
                Vec::new()
            };
            tracks.push(Track::new(spec.clone(), samples));
        }
        let media = Media::new(tracks, broadcast_common::timescale!(90_000));
        self.mux
            .package(&media)
            .map_err(|error| rtflow::Error::Media(format!("SRT push mux: {error}")))
    }
}

fn aac_esds(sample_rate: u32, channels: u16) -> EsdsBox {
    let index = match sample_rate {
        96_000 => 0u8,
        88_200 => 1,
        64_000 => 2,
        48_000 => 3,
        44_100 => 4,
        32_000 => 5,
        24_000 => 6,
        22_050 => 7,
        16_000 => 8,
        12_000 => 9,
        11_025 => 10,
        8_000 => 11,
        _ => 4,
    };
    let asc = vec![
        (2u8 << 3) | (index >> 1),
        ((index & 1) << 7) | ((channels as u8 & 0x0f) << 3),
    ];
    EsdsBox::new(ESDescriptor::new(
        1,
        0,
        Some(DecoderConfigDescriptor::new(
            0x40,
            0x05,
            false,
            0,
            0,
            0,
            Some(DecoderSpecificInfo::new(asc)),
        )),
        Some(SLConfigDescriptor::predefined_two()),
    ))
}

fn track_spec(track: &TrackDescription) -> Option<TrackSpec> {
    match track.codec {
        Codec::H264 => {
            let config = track.fmtp.as_deref().and_then(|fmtp| {
                let value = fmtp
                    .split(';')
                    .find(|part| part.trim().starts_with("sprop-parameter-sets"))?
                    .split_once('=')?
                    .1;
                let mut nals = value.split(',').filter_map(|value| {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD
                        .decode(value.trim())
                        .ok()
                });
                let sps = nals.next()?;
                let pps = nals.next()?;
                let mut config = Vec::with_capacity(sps.len() + pps.len() + 16);
                config.extend_from_slice(&[1, sps[1], sps[2], sps[3], 0xff, 0xe1]);
                config.extend_from_slice(&(sps.len() as u16).to_be_bytes());
                config.extend_from_slice(&sps);
                config.push(1);
                config.extend_from_slice(&(pps.len() as u16).to_be_bytes());
                config.extend_from_slice(&pps);
                Some(config)
            })?;
            let avcc = AVCConfigurationBox::parse_body(&config).ok()?;
            Some(TrackSpec::new(
                TrackId::new(u32::from(track.id) + 1),
                broadcast_common::timescale::Timescale::try_new(track.clock_rate).ok()?,
                CodecConfig::Avc {
                    config: avcc,
                    width: track.width.unwrap_or(0) as u16,
                    height: track.height.unwrap_or(0) as u16,
                },
            ))
        }
        Codec::H265 => {
            let fmtp = track.fmtp.as_deref()?;
            let value = |key: &str| {
                fmtp.split(';')
                    .find(|part| part.trim().starts_with(key))
                    .and_then(|part| part.split_once('='))
                    .and_then(|(_, value)| {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD
                            .decode(value.trim())
                            .ok()
                    })
            };
            let config = rtflow::media::bitstream::build_hevc_config_record(
                &value("sprop-vps")?,
                &value("sprop-sps")?,
                &value("sprop-pps")?,
            )?;
            let hvcc = HEVCConfigurationBox::parse_body(&config).ok()?;
            Some(TrackSpec::new(
                TrackId::new(u32::from(track.id) + 1),
                broadcast_common::timescale::Timescale::try_new(track.clock_rate).ok()?,
                CodecConfig::Hevc {
                    config: hvcc,
                    width: track.width.unwrap_or(0) as u16,
                    height: track.height.unwrap_or(0) as u16,
                },
            ))
        }
        Codec::Aac => Some(TrackSpec::new(
            TrackId::new(u32::from(track.id) + 1),
            broadcast_common::timescale::Timescale::try_new(track.clock_rate).ok()?,
            CodecConfig::Aac {
                esds: aac_esds(track.clock_rate, track.channels.unwrap_or(2) as u16),
                channel_count: track.channels.unwrap_or(2) as u16,
                sample_rate: track.clock_rate,
                sample_size: 16,
            },
        )),
        _ => None,
    }
}
