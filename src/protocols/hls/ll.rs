//! Low-Latency HLS (LL-HLS) live manager.
//!
//! This is a separate endpoint from the classic MPEG-TS HLS live path so the
//! stable 1 s TS pipeline is untouched. It uses `transmux::LlHlsSegmenter` to
//! emit CMAF parts (`#EXT-X-PART`) as they close, plus the completed segments
//! and a preload hint. Part/segment bytes are served from memory.

use super::segmenter::ts::track_spec;
use crate::{
    http::Handler,
    media::{Codec, MediaKind},
    mount::MountRegistry,
};
use bytes::Bytes;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};
use transmux::ll_hls::LlHlsSegmenter;
use transmux::{Sample, TrackId};

#[derive(Clone, Debug)]
pub struct LlHlsConfig {
    pub segment_duration: f64,
    pub part_target_ms: u32,
}

impl Default for LlHlsConfig {
    fn default() -> Self {
        Self {
            segment_duration: 2.0,
            part_target_ms: 200,
        }
    }
}

struct Part {
    bytes: Bytes,
    duration: f64,
    independent: bool,
}

struct State {
    init: Bytes,
    parts: BTreeMap<(u32, u32), Part>,
    segments: BTreeMap<u32, (Bytes, f64)>,
}

struct Channel {
    state: Mutex<State>,
    active: Mutex<bool>,
}

pub struct LlHlsLiveManager {
    registry: Arc<MountRegistry>,
    config: LlHlsConfig,
    channels: Mutex<HashMap<String, Arc<Channel>>>,
}

impl LlHlsLiveManager {
    pub fn new(registry: Arc<MountRegistry>, config: LlHlsConfig) -> Self {
        Self {
            registry,
            config,
            channels: Mutex::new(HashMap::new()),
        }
    }

    fn channel(&self, path: &str) -> Arc<Channel> {
        self.channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(path.to_owned())
            .or_insert_with(|| {
                Arc::new(Channel {
                    state: Mutex::new(State {
                        init: Bytes::new(),
                        parts: BTreeMap::new(),
                        segments: BTreeMap::new(),
                    }),
                    active: Mutex::new(false),
                })
            })
            .clone()
    }

    fn start(&self, mount: crate::mount::MountHandle, channel: Arc<Channel>) {
        let mut active = channel
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *active {
            return;
        }
        let description = mount.description();
        if description.tracks.is_empty() {
            return;
        }
        let mut specs = Vec::new();
        let mut timescales = HashMap::new();
        let mut durations = HashMap::new();
        for track in &description.tracks {
            if let Some(spec) = track_spec(track) {
                timescales.insert(track.id, track.clock_rate);
                let duration = match track.kind {
                    MediaKind::Video => track
                        .frame_rate
                        .map(|(n, d)| {
                            (u64::from(track.clock_rate) * u64::from(d) / u64::from(n).max(1))
                                as u32
                        })
                        .unwrap_or(3_000),
                    MediaKind::Audio => 1024,
                };
                durations.insert(track.id, duration.max(1));
                specs.push(spec);
            }
        }
        if specs.is_empty() {
            return;
        }
        let Ok(mut segmenter) = LlHlsSegmenter::with_part_target(
            specs,
            90_000,
            self.config.segment_duration,
            self.config.part_target_ms,
        ) else {
            return;
        };
        let init = segmenter.init_segment().unwrap_or_default();
        if let Ok(mut state) = channel.state.lock() {
            state.init = Bytes::from(init);
        }
        let cancel = mount.cancellation_token();
        *active = true;
        drop(active);
        tokio::spawn(async move {
            let (mut receiver, startup) = mount.subscribe_with_startup();
            let mut startup = std::collections::VecDeque::from(startup);
            let mut awaiting_keyframe = true;
            loop {
                let unit = if let Some(unit) = startup.pop_front() {
                    unit
                } else {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        result = receiver.recv() => match result {
                            Ok(unit) => unit,
                            Err(crate::mount::FrameRecvError::Lagged(_)) => { awaiting_keyframe = true; continue; }
                            Err(_) => break,
                        },
                    }
                };
                if awaiting_keyframe {
                    if !matches!(
                        unit.codec,
                        Codec::H264 | Codec::H265 | Codec::Vp9 | Codec::Av1
                    ) || !unit.is_keyframe
                    {
                        continue;
                    }
                    awaiting_keyframe = false;
                }
                let Some(&track_id) = timescales.get(&unit.track_id).map(|_| &unit.track_id) else {
                    continue;
                };
                let Some(timescale) = timescales.get(&track_id).copied() else {
                    continue;
                };
                let ticks = |d: std::time::Duration| {
                    (d.as_secs() * u64::from(timescale)
                        + u64::from(d.subsec_nanos()) * u64::from(timescale) / 1_000_000_000)
                        as i64
                };
                let sample = if matches!(unit.codec, Codec::Aac | Codec::Pcma | Codec::Pcmu) {
                    Sample::from_raw(
                        unit.data.clone(),
                        Some(ticks(unit.dts.unwrap_or(unit.pts))),
                        Some(ticks(unit.pts)),
                        None,
                    )
                } else {
                    Sample::from_annexb(
                        &unit.data,
                        Some(ticks(unit.dts.unwrap_or(unit.pts))),
                        Some(ticks(unit.pts)),
                        None,
                        unit.is_keyframe,
                    )
                };
                let mut sample = sample;
                sample.duration = durations.get(&track_id).copied();
                if segmenter
                    .push(TrackId::new(u32::from(track_id) + 1), sample)
                    .is_err()
                {
                    continue;
                }
                let mut state = channel
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let parts_ready = segmenter.take_ready_parts();
                for part in parts_ready {
                    state.parts.insert(
                        (part.segment_seq, part.part_index),
                        Part {
                            bytes: Bytes::from(part.bytes),
                            duration: part.duration,
                            independent: part.independent,
                        },
                    );
                }
                for segment in segmenter.take_ready_segments() {
                    state.segments.insert(
                        segment.segment_seq,
                        (Bytes::from(segment.bytes), segment.duration),
                    );
                    state
                        .parts
                        .retain(|(msn, _), _| *msn >= segment.segment_seq);
                }
                drop(state);
            }
            let mut state = channel
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = segmenter.flush();
            for part in segmenter.take_ready_parts() {
                state.parts.insert(
                    (part.segment_seq, part.part_index),
                    Part {
                        bytes: Bytes::from(part.bytes),
                        duration: part.duration,
                        independent: part.independent,
                    },
                );
            }
            for segment in segmenter.take_ready_segments() {
                state.segments.insert(
                    segment.segment_seq,
                    (Bytes::from(segment.bytes), segment.duration),
                );
            }
        });
    }

    fn playlist(&self, channel: &Channel, query: &str) -> String {
        let state = channel
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:9\n#EXT-X-INDEPENDENT-SEGMENTS\n");
        out.push_str(&format!(
            "#EXT-X-TARGETDURATION:{}\n",
            self.config.segment_duration.ceil().max(1.0) as u64
        ));
        out.push_str(&format!(
            "#EXT-X-PART-INF:PART-TARGET={:.3}\n",
            f64::from(self.config.part_target_ms) / 1000.0
        ));
        out.push_str(&format!(
            "#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK={:.3}\n",
            f64::from(self.config.part_target_ms) * 3.0 / 1000.0
        ));
        let first_sequence = state
            .segments
            .keys()
            .next()
            .copied()
            .or_else(|| state.parts.keys().next().map(|(msn, _)| *msn))
            .unwrap_or(0);
        out.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{first_sequence}\n"));
        out.push_str(&format!(
            "#EXT-X-MAP:URI=\"init.m4s{}\"\n",
            query_suffix(query)
        ));
        let last_segment = state.segments.keys().next_back().copied().unwrap_or(0);
        for (&seq, (_, duration)) in &state.segments {
            out.push_str(&format!(
                "#EXTINF:{duration:.3},\n{seq}.m4s{}\n",
                query_suffix(query)
            ));
        }
        for (&(msn, idx), part) in state.parts.range((last_segment, 0)..) {
            out.push_str(&format!(
                "#EXT-X-PART:DURATION={:.3},URI=\"part-{msn}-{idx}.m4s{}\",INDEPENDENT={}\n",
                part.duration,
                query_suffix(query),
                if part.independent { "YES" } else { "NO" }
            ));
        }
        if let Some(&(msn, _)) = state.parts.keys().next_back() {
            out.push_str(&format!(
                "#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part-{}-0.m4s{}\"\n",
                msn + 1,
                query_suffix(query)
            ));
        }
        out
    }
}

fn query_suffix(query: &str) -> String {
    if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    }
}

fn mount_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/llhls/")?;
    if let Some(mount) = rest.strip_suffix("/index.m3u8") {
        return Some((mount, "playlist"));
    }
    if let Some(mount) = rest.strip_suffix("/init.m4s") {
        return Some((mount, "init"));
    }
    let (mount, file) = rest.rsplit_once('/')?;
    if file.starts_with("part-") && file.ends_with(".m4s") {
        return Some((mount, "part"));
    }
    if file.ends_with(".m4s") {
        return Some((mount, "segment"));
    }
    None
}

pub fn create_ll_hls_handler(manager: Arc<LlHlsLiveManager>) -> Handler {
    Arc::new(move |req, mut writer| {
        let manager = manager.clone();
        Box::pin(async move {
            let Some((mount_path, kind)) = mount_path(&req.path).map(|(m, k)| (m.to_owned(), k))
            else {
                writer.write_body(404, "text/plain", b"Not Found").await?;
                return Ok(writer);
            };
            let registry_path = format!("/{mount_path}");
            let Some(mount) = manager.registry.get_handle(&registry_path) else {
                writer
                    .write_body(404, "text/plain", b"Mount not found")
                    .await?;
                return Ok(writer);
            };
            let channel = manager.channel(&registry_path);
            manager.start(mount, channel.clone());
            match kind {
                "playlist" => {
                    let query = req
                        .query
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join("&");
                    let body = manager.playlist(&channel, &query);
                    writer
                        .write_headers(
                            200,
                            &[
                                ("Content-Type", "application/vnd.apple.mpegurl"),
                                ("Content-Length", &body.len().to_string()),
                                ("Cache-Control", "no-cache"),
                                ("Access-Control-Allow-Origin", "*"),
                                ("Connection", "keep-alive"),
                            ],
                        )
                        .await?;
                    writer.write_raw(body.as_bytes()).await?;
                }
                "init" => {
                    let init = {
                        channel
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .init
                            .clone()
                    };
                    if init.is_empty() {
                        writer.write_body(404, "text/plain", b"No init").await?;
                        return Ok(writer);
                    }
                    writer
                        .write_headers(
                            200,
                            &[
                                ("Content-Type", "video/mp4"),
                                ("Content-Length", &init.len().to_string()),
                                ("Cache-Control", "no-store"),
                                ("Access-Control-Allow-Origin", "*"),
                            ],
                        )
                        .await?;
                    writer.write_raw(&init).await?;
                }
                "part" => {
                    let file = req.path.rsplit('/').next().unwrap_or("");
                    let mut it = file.trim_end_matches(".m4s").split('-');
                    let _ = it.next();
                    let msn = it
                        .next()
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(u32::MAX);
                    let idx = it
                        .next()
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(u32::MAX);
                    let bytes = {
                        channel
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .parts
                            .get(&(msn, idx))
                            .map(|part| part.bytes.clone())
                    };
                    let Some(bytes) = bytes else {
                        writer
                            .write_body(404, "text/plain", b"Part not found")
                            .await?;
                        return Ok(writer);
                    };
                    writer
                        .write_headers(
                            200,
                            &[
                                ("Content-Type", "video/iso.segment"),
                                ("Content-Length", &bytes.len().to_string()),
                                ("Cache-Control", "no-store"),
                                ("Access-Control-Allow-Origin", "*"),
                            ],
                        )
                        .await?;
                    writer.write_raw(&bytes).await?;
                }
                "segment" => {
                    let seq = req
                        .path
                        .rsplit('/')
                        .next()
                        .and_then(|f| f.strip_suffix(".m4s"))
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(u32::MAX);
                    let bytes = {
                        channel
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .segments
                            .get(&seq)
                            .map(|(bytes, _)| bytes.clone())
                    };
                    let Some(bytes) = bytes else {
                        writer
                            .write_body(404, "text/plain", b"Segment not found")
                            .await?;
                        return Ok(writer);
                    };
                    writer
                        .write_headers(
                            200,
                            &[
                                ("Content-Type", "video/iso.segment"),
                                ("Content-Length", &bytes.len().to_string()),
                                ("Cache-Control", "no-store"),
                                ("Access-Control-Allow-Origin", "*"),
                            ],
                        )
                        .await?;
                    writer.write_raw(&bytes).await?;
                }
                _ => {
                    writer.write_body(404, "text/plain", b"Not Found").await?;
                }
            }
            Ok(writer)
        })
    })
}
