//! Path discovery: merge logical paths with live mount state and mint URLs.
use crate::{
    config::Config,
    paths::{PathRecord, PathSource, PathStore},
};
use rtflow::{
    MountRegistry,
    media::{MediaKind, TrackDescription},
};
use serde::Serialize;
use std::{
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::SystemTime,
};

#[derive(Clone, Debug, Serialize)]
pub struct TrackView {
    pub kind: &'static str,
    pub codec: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<String>,
    pub channels: Option<u8>,
    pub sample_rate: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct PathUrls {
    pub rtsp: Option<String>,
    pub rtmp: Option<String>,
    pub srt: Option<String>,
    pub whep: Option<String>,
    pub hls: Option<String>,
    pub llhls: Option<String>,
    pub dash: Option<String>,
    pub flv: Option<String>,
    pub ws_flv: Option<String>,
    pub vod: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PathStatus {
    pub name: String,
    pub online: bool,
    pub source: PathSource,
    pub publisher: bool,
    pub readers: usize,
    pub recording: bool,
    pub uptime_seconds: u64,
    pub bytes: u64,
    pub tracks: Vec<TrackView>,
    pub urls: PathUrls,
    pub labels: std::collections::BTreeMap<String, String>,
    pub tenant: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Discovery {
    pub server: String,
    pub version: &'static str,
    pub generated_at: u64,
    pub protocols: ProtocolStatus,
    pub paths: Vec<PathStatus>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProtocolStatus {
    pub rtsp: bool,
    pub rtmp: bool,
    pub srt: bool,
    pub webrtc: bool,
    pub hls: bool,
    pub llhls: bool,
    pub dash: bool,
    pub flv: bool,
    pub ws_flv: bool,
}

pub struct DiscoveryService {
    config: RwLock<Arc<Config>>,
    paths: Arc<PathStore>,
    mounts: Arc<MountRegistry>,
    started: SystemTime,
}

impl DiscoveryService {
    pub fn new(config: Arc<Config>, paths: Arc<PathStore>, mounts: Arc<MountRegistry>) -> Self {
        Self {
            config: RwLock::new(config),
            paths,
            mounts,
            started: SystemTime::now(),
        }
    }

    /// Replace the effective configuration (SIGHUP). Listen addresses are
    /// reported from the new config but only take effect after a restart.
    pub fn reload(&self, config: Arc<Config>) {
        *self
            .config
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
    }

    fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn list(&self) -> Vec<PathStatus> {
        let mut names: Vec<String> = self
            .mounts
            .paths()
            .iter()
            .map(|path| path.as_str().trim_start_matches('/').to_owned())
            .collect();
        for record in self.paths.list() {
            if !names.contains(&record.name) {
                names.push(record.name);
            }
        }
        names.sort();
        names.iter().filter_map(|name| self.status(name)).collect()
    }

    pub fn status(&self, name: &str) -> Option<PathStatus> {
        let name = name.trim_start_matches('/');
        if !self.paths.known(name) {
            return None;
        }
        let record = self.paths.resolve(name);
        let handle = self.mounts.get_handle(&format!("/{name}"));
        let (online, publisher, readers, bytes, tracks, started) = match handle.as_ref() {
            Some(handle) => {
                let snapshot = handle.snapshot();
                let description = handle.description();
                (
                    true,
                    snapshot.publisher_connected,
                    snapshot.subscriber_count.max(snapshot.broadcast_receivers),
                    snapshot.stats.rtp_octets,
                    description
                        .tracks
                        .iter()
                        .map(track_view)
                        .collect::<Vec<_>>(),
                    None,
                )
            }
            None => (false, false, 0, 0, Vec::new(), None),
        };
        let uptime = started
            .and_then(|start| SystemTime::now().duration_since(start).ok())
            .map(|duration| duration.as_secs())
            .or_else(|| {
                online.then(|| {
                    SystemTime::now()
                        .duration_since(self.started)
                        .unwrap_or_default()
                        .as_secs()
                })
            })
            .unwrap_or(0);
        Some(PathStatus {
            name: name.to_owned(),
            online,
            source: record.source,
            publisher,
            readers,
            recording: record.record && publisher,
            uptime_seconds: uptime,
            bytes,
            tracks,
            urls: self.urls(name, &record),
            labels: record.labels.clone(),
            tenant: record.tenant.clone(),
        })
    }

    pub fn discovery(&self) -> Discovery {
        Discovery {
            server: self.config().server.name.clone(),
            version: env!("CARGO_PKG_VERSION"),
            generated_at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            protocols: ProtocolStatus {
                rtsp: self.config().rtsp.enabled,
                rtmp: self.config().rtmp.enabled,
                srt: self.config().srt.enabled,
                webrtc: self.config().webrtc.enabled && self.config().http.listen.port() != 0,
                hls: self.config().hls.enabled,
                llhls: self.config().hls.enabled && self.config().hls.low_latency,
                dash: self.config().dash.enabled,
                flv: self.config().flv.enabled,
                ws_flv: self.config().flv.enabled,
            },
            paths: self.list(),
        }
    }

    fn urls(&self, name: &str, record: &PathRecord) -> PathUrls {
        let config = self.config();
        let http = public_http(&config);
        PathUrls {
            rtsp: record
                .enabled
                .then(|| public_rtsp(&config).map(|base| format!("{base}/{name}")))
                .flatten(),
            rtmp: record
                .enabled
                .then(|| public_rtmp(&config).map(|base| format!("{base}/{name}")))
                .flatten(),
            srt: record
                .enabled
                .then(|| public_srt(&config).map(|base| format!("{base}?streamid={name}")))
                .flatten(),
            whep: (record.enabled && record.webrtc && config.webrtc.enabled)
                .then(|| format!("{http}/whep/{name}")),
            hls: (record.enabled && record.hls && config.hls.enabled)
                .then(|| format!("{http}/hls/{name}/index.m3u8")),
            llhls: (record.enabled && record.hls && config.hls.enabled && config.hls.low_latency)
                .then(|| format!("{http}/llhls/{name}/index.m3u8")),
            dash: (record.enabled && record.dash && config.dash.enabled)
                .then(|| format!("{http}/dash/{name}/manifest.mpd")),
            flv: (record.enabled && record.flv && config.flv.enabled)
                .then(|| format!("{http}/flv/{name}.flv")),
            ws_flv: (record.enabled && record.flv && config.flv.enabled).then(|| {
                let addr = host_for(config.flv.ws_listen);
                format!("ws://{addr}/{name}.flv")
            }),
            vod: Some(format!("{http}/vod/")),
        }
    }
}

fn track_view(track: &TrackDescription) -> TrackView {
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

fn host_for(addr: SocketAddr) -> String {
    if addr.ip().is_unspecified() {
        format!("127.0.0.1:{}", addr.port())
    } else {
        addr.to_string()
    }
}

pub fn public_http(config: &Config) -> String {
    config
        .server
        .public_http
        .clone()
        .unwrap_or_else(|| format!("http://{}", host_for(config.http.listen)))
        .trim_end_matches('/')
        .to_owned()
}

pub fn public_rtsp(config: &Config) -> Option<String> {
    if !config.rtsp.enabled {
        return None;
    }
    Some(
        config
            .server
            .public_rtsp
            .clone()
            .unwrap_or_else(|| format!("rtsp://{}", host_for(config.rtsp.listen)))
            .trim_end_matches('/')
            .to_owned(),
    )
}

pub fn public_rtmp(config: &Config) -> Option<String> {
    if !config.rtmp.enabled {
        return None;
    }
    Some(
        config
            .server
            .public_rtmp
            .clone()
            .unwrap_or_else(|| format!("rtmp://{}", host_for(config.rtmp.listen)))
            .trim_end_matches('/')
            .to_owned(),
    )
}

pub fn public_srt(config: &Config) -> Option<String> {
    if !config.srt.enabled {
        return None;
    }
    Some(
        config
            .server
            .public_srt
            .clone()
            .unwrap_or_else(|| format!("srt://{}", host_for(config.srt.listen)))
            .trim_end_matches('/')
            .to_owned(),
    )
}
