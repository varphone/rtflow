//! JSON view models used by the admin API.
use crate::AdminState;
use rtflow_media::{MediaKind, TrackDescription};
use serde::Serialize;

#[derive(Serialize)]
pub(super) struct TrackView {
    kind: &'static str,
    codec: String,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<String>,
    channels: Option<u8>,
    sample_rate: u32,
}

#[derive(Serialize)]
pub(super) struct StreamView {
    pub path: String,
    pub name: String,
    pub video: Option<TrackView>,
    pub audio: Option<TrackView>,
    pub publisher: bool,
    pub subscribers: usize,
    pub accepted_frames: u64,
    pub dropped_frames: u64,
    pub rtp_packets: u64,
    pub rtp_octets: u64,
    pub hls_url: String,
    pub ll_hls_url: String,
    pub dash_url: String,
    pub rtmp_url: String,
    pub rtsp_url: String,
    pub protocols: Vec<&'static str>,
}

pub(super) fn track_view(track: &TrackDescription) -> TrackView {
    TrackView {
        kind: match track.kind {
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
        },
        codec: format!("{:?}", track.codec),
        width: track.width,
        height: track.height,
        frame_rate: track.frame_rate.map(|(num, den)| {
            if den == 1 {
                format!("{num}")
            } else {
                format!("{num}/{den}")
            }
        }),
        channels: track.channels,
        sample_rate: track.clock_rate,
    }
}

pub(super) fn stream_view(state: &AdminState, path: &str) -> Option<StreamView> {
    let handle = state.mounts.get_handle(path)?;
    let description = handle.description();
    let snapshot = handle.snapshot();
    let video = description
        .tracks
        .iter()
        .find(|track| track.kind == MediaKind::Video)
        .map(track_view);
    let audio = description
        .tracks
        .iter()
        .find(|track| track.kind == MediaKind::Audio)
        .map(track_view);
    let base = state.config.base_path.trim_end_matches('/');
    let hls_url = format!(
        "{}{}/index.m3u8",
        state.config.hls_prefix.trim_end_matches('/'),
        path
    );
    let ll_hls_url = format!(
        "{}{}/index.m3u8",
        state.config.ll_hls_prefix.trim_end_matches('/'),
        path
    );
    let dash_url = format!("/dash/{}/manifest.mpd", path.trim_start_matches('/'));
    let rtmp_url = format!(
        "{}{}",
        state
            .config
            .rtmp_base
            .as_deref()
            .unwrap_or("rtmp://127.0.0.1:1935"),
        path
    );
    let rtsp_url = format!(
        "{}{}",
        state
            .config
            .rtsp_base
            .as_deref()
            .unwrap_or("rtsp://127.0.0.1:8554"),
        path
    );
    let _ = base;
    Some(StreamView {
        path: path.to_owned(),
        name: path.rsplit('/').next().unwrap_or(path).to_owned(),
        video,
        audio,
        publisher: publishing(&snapshot),
        subscribers: snapshot.subscriber_count.max(snapshot.broadcast_receivers),
        accepted_frames: snapshot.stats.accepted_frames,
        dropped_frames: snapshot.stats.dropped_frames,
        rtp_packets: snapshot.stats.rtp_packets,
        rtp_octets: snapshot.stats.rtp_octets,
        hls_url,
        ll_hls_url,
        dash_url,
        rtmp_url,
        rtsp_url,
        protocols: vec!["HLS", "DASH", "RTSP"],
    })
}

fn publishing(snapshot: &rtflow_mount::MountSnapshot) -> bool {
    snapshot.publisher_connected
        || matches!(
            snapshot.proxy_state,
            Some(rtflow_mount::upstream::ProxyState::Streaming)
        )
}

pub(super) fn streams(state: &AdminState) -> Vec<StreamView> {
    state
        .mounts
        .paths()
        .iter()
        .filter_map(|path| stream_view(state, path.as_str()))
        .collect()
}

#[derive(Serialize)]
pub(super) struct Overview {
    pub uptime_seconds: u64,
    pub mounts: usize,
    pub publishers: usize,
    pub subscribers: usize,
    pub accepted_frames: u64,
    pub dropped_frames: u64,
    pub rtp_packets: u64,
    pub rtp_octets: u64,
    pub vods: usize,
}

#[derive(Serialize)]
pub(super) struct VodView {
    id: String,
    title: String,
    duration_ms: u128,
    playlist: String,
    dash: String,
    cached: bool,
    cache_bytes: u64,
}

pub(super) fn vods(state: &AdminState) -> Vec<VodView> {
    {
        if let Some(vod) = state.config.vod.as_ref() {
            let entries = vod.entries();
            return entries
                .into_iter()
                .map(|entry| {
                    let cache_bytes = vod.cache_size(&entry.id).unwrap_or(0);
                    VodView {
                        id: entry.id.clone(),
                        title: entry.title.clone(),
                        duration_ms: entry.duration.map_or(0, |duration| duration.as_millis()),
                        playlist: format!("/vod/{}/index.m3u8", entry.id),
                        dash: format!("/dash-vod/{}/manifest.mpd", entry.id),
                        cached: cache_bytes > 0,
                        cache_bytes,
                    }
                })
                .collect();
        }
    }
    let _ = state;
    Vec::new()
}
