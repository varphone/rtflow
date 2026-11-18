//! Low-latency DASH (LL-DASH) using CMAF chunks.
//!
//! Each track gets its own [`transmux::LlSegmenter`], so the output is
//! per-track as required by DASH-IF low-latency IOP. A segment URL is served
//! with HTTP chunked transfer: chunks already produced are written immediately
//! and the response stays open until the segment completes.
use bytes::Bytes;
use rtflow_hls::segmenter::track_spec;
use rtflow_http::{Handler, HttpRequest, ResponseWriter};
use rtflow_media::{AccessUnit, Codec, MediaKind, TrackDescription};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use tokio::io::AsyncWrite;
use tokio::sync::Notify;
use transmux::{Chunk, LlSegmenter, Sample, TrackId};

#[derive(Clone, Debug)]
pub struct LlDashConfig {
    pub segment_duration: Duration,
    pub chunk_duration: Duration,
    pub latency_target: Duration,
    pub time_shift: Duration,
    pub utc_timing_url: Option<String>,
}

impl Default for LlDashConfig {
    fn default() -> Self {
        Self {
            segment_duration: Duration::from_secs(1),
            chunk_duration: Duration::from_millis(200),
            latency_target: Duration::from_secs(2),
            time_shift: Duration::from_secs(6),
            utc_timing_url: None,
        }
    }
}

pub struct LlDashHandlers {
    pub manifest: Handler,
    pub init: Handler,
    pub segment: Handler,
}

struct LlTrackState {
    id: u8,
    kind: MediaKind,
    track: TrackDescription,
    init: Bytes,
    segments: Mutex<BTreeMap<u64, Vec<Bytes>>>,
    complete: Mutex<BTreeSet<u64>>,
    retain_segments: u64,
    notify: Notify,
}

struct LlState {
    tracks: Vec<Arc<LlTrackState>>,
    availability_start: Mutex<Option<SystemTime>>,
    start_notify: Notify,
}

impl LlState {
    fn mark_start(&self) {
        let mut start = self
            .availability_start
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if start.is_none() {
            *start = Some(SystemTime::now());
            drop(start);
            self.start_notify.notify_waiters();
        }
    }

    fn start(&self) -> SystemTime {
        self.availability_start
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }
}

pub fn create_ll_dash_handlers(
    config: LlDashConfig,
    mount: &rtflow_mount::MountHandle,
) -> LlDashHandlers {
    let description = mount.description();
    let mut tracks = Vec::new();
    let mut plans = Vec::new();
    for track in &description.tracks {
        let Some(spec) = track_spec(track) else {
            continue;
        };
        let chunk_samples = chunk_samples(track, config.chunk_duration);
        let Ok(segmenter) = LlSegmenter::new(
            vec![spec],
            90_000,
            config.segment_duration.as_secs_f64(),
            chunk_samples,
        ) else {
            continue;
        };
        let init = segmenter.init_segment().unwrap_or_default();
        let state = Arc::new(LlTrackState {
            id: track.id,
            kind: track.kind,
            track: track.clone(),
            init: Bytes::from(init),
            segments: Mutex::new(BTreeMap::new()),
            complete: Mutex::new(BTreeSet::new()),
            retain_segments: (config.time_shift.as_secs_f64()
                / config.segment_duration.as_secs_f64().max(0.001))
            .ceil()
            .max(2.0) as u64,
            notify: Notify::new(),
        });
        tracks.push(state.clone());
        plans.push((state, segmenter));
    }
    let state = Arc::new(LlState {
        tracks: tracks.clone(),
        availability_start: Mutex::new(None),
        start_notify: Notify::new(),
    });
    let (mut receiver, startup) = mount.subscribe_with_startup();
    let cancel = mount.cancellation_token();
    let task_state = state.clone();
    tokio::spawn(async move {
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
                        Err(rtflow_mount::FrameRecvError::Lagged(_)) => {
                            awaiting_keyframe = true;
                            continue;
                        }
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
            let index = plans
                .iter()
                .position(|(track, _)| track.id == unit.track_id);
            if let Some(index) = index {
                let track = plans[index].0.clone();
                let sample = to_sample(&unit, &track.track);
                if plans[index]
                    .1
                    .push(TrackId::new(u32::from(track.id) + 1), sample)
                    .is_err()
                {
                    continue;
                }
                let chunks = plans[index].1.take_ready();
                if !chunks.is_empty() {
                    task_state.mark_start();
                }
                store_chunks(&track, chunks);
            }
        }
        for (track, segmenter) in &mut plans {
            let _ = segmenter.flush();
            store_chunks(track, segmenter.take_ready());
            track
                .complete
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(
                    track
                        .segments
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .keys()
                        .copied(),
                );
            track.notify.notify_waiters();
        }
    });

    let manifest_state = state.clone();
    let manifest_config = config.clone();
    let manifest: Handler = Arc::new(move |_req, writer| {
        let state = manifest_state.clone();
        let config = manifest_config.clone();
        Box::pin(async move {
            let mut writer = writer;
            if state.start() == SystemTime::UNIX_EPOCH {
                let notified = state.start_notify.notified();
                let _ = tokio::time::timeout(Duration::from_secs(5), notified).await;
            }
            if state.start() == SystemTime::UNIX_EPOCH {
                writer
                    .write_body(
                        503,
                        "application/dash+xml",
                        b"<MPD/>\n<!-- No chunks available yet -->",
                    )
                    .await?;
                return Ok(writer);
            }
            let body = generate_ll_mpd(&state, &config);
            writer
                .write_body(200, "application/dash+xml", body.as_bytes())
                .await?;
            Ok(writer)
        })
    });

    let init_state = state.clone();
    let init: Handler = Arc::new(move |req, writer| {
        let state = init_state.clone();
        Box::pin(async move { serve_init(req, writer, &state).await })
    });

    let segment_state = state.clone();
    let segment: Handler = Arc::new(move |req, writer| {
        let state = segment_state.clone();
        Box::pin(async move { serve_segment(req, writer, &state).await })
    });

    LlDashHandlers {
        manifest,
        init,
        segment,
    }
}

fn chunk_samples(track: &TrackDescription, chunk: Duration) -> usize {
    if track.kind == MediaKind::Video {
        let fps = track
            .frame_rate
            .map(|(n, d)| f64::from(n) / f64::from(d))
            .unwrap_or(25.0);
        (fps * chunk.as_secs_f64()).round().max(1.0) as usize
    } else {
        (f64::from(track.clock_rate) * chunk.as_secs_f64() / 1024.0)
            .round()
            .max(1.0) as usize
    }
}

fn to_sample(unit: &AccessUnit, track: &TrackDescription) -> Sample {
    let timescale = u64::from(track.clock_rate.max(1));
    let ticks = |duration: Duration| {
        (duration.as_secs() * timescale
            + u64::from(duration.subsec_nanos()) * timescale / 1_000_000_000) as i64
    };
    let dts = ticks(unit.dts.unwrap_or(unit.pts));
    let pts = ticks(unit.pts);
    let mut sample = if matches!(unit.codec, Codec::Aac | Codec::Pcma | Codec::Pcmu) {
        Sample::from_raw(unit.data.to_vec(), Some(dts), Some(pts), None)
    } else {
        Sample::from_annexb(&unit.data, Some(dts), Some(pts), None, unit.is_keyframe)
    };
    sample.duration = Some(if track.kind == MediaKind::Video {
        track
            .frame_rate
            .map(|(n, d)| (timescale * u64::from(d) / u64::from(n).max(1)) as u32)
            .unwrap_or(3000)
    } else {
        1024
    });
    sample
}

fn store_chunks(track: &LlTrackState, chunks: Vec<Chunk>) {
    if chunks.is_empty() {
        return;
    }
    let mut segments = track
        .segments
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut complete = track
        .complete
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for chunk in chunks {
        if chunk.is_segment_start {
            for number in segments
                .keys()
                .copied()
                .filter(|number| *number < chunk.segment_number)
            {
                complete.insert(number);
            }
        }
        segments
            .entry(chunk.segment_number)
            .or_default()
            .push(Bytes::from(chunk.data));
    }
    if let Some(last) = segments.keys().next_back().copied() {
        let cutoff = last.saturating_sub(track.retain_segments);
        segments.retain(|number, _| *number > cutoff);
        complete.retain(|number| *number > cutoff);
    }
    drop(segments);
    drop(complete);
    track.notify.notify_waiters();
}

fn parse_track<'a>(
    file: &str,
    tracks: &'a [Arc<LlTrackState>],
) -> Option<(&'a Arc<LlTrackState>, Option<u64>, bool)> {
    let (prefix, rest) = if let Some(rest) = file.strip_prefix("video-") {
        ("video", rest)
    } else if let Some(rest) = file.strip_prefix("audio-") {
        ("audio", rest)
    } else {
        return None;
    };
    let _ = prefix;
    if let Some(id) = rest.strip_suffix("-init.mp4") {
        let id = id.parse::<u8>().ok()?;
        return tracks
            .iter()
            .find(|track| track.id == id)
            .map(|track| (track, None, true));
    }
    let (id, segment) = rest.split_once('-')?;
    let id = id.parse::<u8>().ok()?;
    let segment = segment
        .strip_suffix(".m4s")
        .and_then(|value| value.parse::<u64>().ok())?;
    tracks
        .iter()
        .find(|track| track.id == id)
        .map(|track| (track, Some(segment), false))
}

async fn serve_init<W: AsyncWrite + Unpin>(
    req: HttpRequest,
    mut writer: ResponseWriter<W>,
    state: &LlState,
) -> rtflow_core::error::Result<ResponseWriter<W>> {
    let file = req.path.rsplit('/').next().unwrap_or("");
    let Some((track, _, true)) = parse_track(file, &state.tracks) else {
        writer.write_body(404, "text/plain", b"not found").await?;
        return Ok(writer);
    };
    let content_type = if track.kind == MediaKind::Video {
        "video/mp4"
    } else {
        "audio/mp4"
    };
    writer.write_body(200, content_type, &track.init).await?;
    Ok(writer)
}

async fn serve_segment<W: AsyncWrite + Unpin>(
    req: HttpRequest,
    mut writer: ResponseWriter<W>,
    state: &LlState,
) -> rtflow_core::error::Result<ResponseWriter<W>> {
    let file = req.path.rsplit('/').next().unwrap_or("");
    let Some((track, Some(segment), false)) = parse_track(file, &state.tracks) else {
        writer.write_body(404, "text/plain", b"not found").await?;
        return Ok(writer);
    };
    // Wait briefly for the first chunk of a newly advertised segment.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if track
            .segments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&segment)
        {
            break;
        }
        let notified = track.notify.notified();
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            writer
                .write_body(404, "text/plain", b"segment not available")
                .await?;
            return Ok(writer);
        }
    }
    let content_type = if track.kind == MediaKind::Video {
        "video/iso.segment"
    } else {
        "audio/iso.segment"
    };
    writer
        .write_chunked_headers(content_type, &[("Access-Control-Allow-Origin", "*")])
        .await?;
    let mut sent = 0usize;
    loop {
        let (chunks, complete) = {
            let segments = track
                .segments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let chunks = segments.get(&segment).cloned().unwrap_or_default();
            let complete = track
                .complete
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&segment);
            (chunks, complete)
        };
        while sent < chunks.len() {
            writer.write_chunk(&chunks[sent]).await?;
            sent += 1;
        }
        if complete {
            break;
        }
        let notified = track.notify.notified();
        if tokio::time::timeout(Duration::from_secs(5), notified)
            .await
            .is_err()
        {
            break;
        }
    }
    writer.write_raw(b"0\r\n\r\n").await?;
    writer.flush().await?;
    Ok(writer)
}

fn generate_ll_mpd(state: &LlState, config: &LlDashConfig) -> String {
    let start = state.start();
    let ast = start
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let publish = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let ato =
        (config.segment_duration.as_secs_f64() - config.chunk_duration.as_secs_f64()).max(0.0);
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!(
        "<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" type=\"dynamic\" profiles=\"urn:mpeg:dash:profile:isoff-live:2011\" minBufferTime=\"PT2.000S\" availabilityStartTime=\"{}\" publishTime=\"{}\" minimumUpdatePeriod=\"PT{:.3}S\" timeShiftBufferDepth=\"PT{:.3}S\" suggestedPresentationDelay=\"PT2.000S\">\n",
        iso8601(ast),
        iso8601(publish),
        config.chunk_duration.as_secs_f64(),
        config.time_shift.as_secs_f64()
    ));
    xml.push_str(&format!(
        "  <ServiceDescription id=\"0\"><Latency target=\"{}\"/></ServiceDescription>\n",
        config.latency_target.as_millis()
    ));
    xml.push_str("  <Period id=\"0\" start=\"PT0S\">\n");
    for track in &state.tracks {
        let mime = if track.kind == MediaKind::Video {
            "video/mp4"
        } else {
            "audio/mp4"
        };
        let codec = match track.track.codec {
            Codec::H264 => "avc1.42E01E",
            Codec::H265 => "hvc1.1.6.L93.B0",
            Codec::Aac => "mp4a.40.2",
            _ => "avc1.42E01E",
        };
        let timescale = track.track.clock_rate.max(1);
        let duration = (config.segment_duration.as_secs_f64() * f64::from(timescale)).round();
        let prefix = format!(
            "{}-{}",
            if track.kind == MediaKind::Video {
                "video"
            } else {
                "audio"
            },
            track.id
        );
        xml.push_str(&format!(
            "    <AdaptationSet mimeType=\"{mime}\" segmentAlignment=\"true\" startWithSAP=\"1\">\n      <Representation id=\"{prefix}\" codecs=\"{codec}\" bandwidth=\"1000000\""
        ));
        if let Some((width, height)) = track.track.width.zip(track.track.height) {
            xml.push_str(&format!(" width=\"{width}\" height=\"{height}\""));
        }
        if track.kind == MediaKind::Audio {
            xml.push_str(&format!(
                " audioSamplingRate=\"{}\"",
                track.track.clock_rate
            ));
        }
        xml.push_str(&format!(
            ">\n        <SegmentTemplate timescale=\"{timescale}\" startNumber=\"1\" duration=\"{duration:.0}\" availabilityTimeOffset=\"{ato:.3}\" availabilityTimeComplete=\"false\" initialization=\"{prefix}-init.mp4\" media=\"{prefix}-$Number$.m4s\"/>\n      </Representation>\n    </AdaptationSet>\n"
        ));
    }
    xml.push_str("  </Period>\n");
    if let Some(url) = &config.utc_timing_url {
        xml.push_str(&format!(
            "  <UTCTiming schemeIdUri=\"urn:mpeg:dash:utc:http-iso:2014\" value=\"{url}\"/>\n"
        ));
    }
    xml.push_str("</MPD>\n");
    xml
}

fn iso8601(duration: Duration) -> String {
    let secs = duration.as_secs();
    let days = secs / 86_400;
    let time = secs % 86_400;
    let (year, month, day) = days_to_ymd(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}
