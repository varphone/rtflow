//! Registry-backed HLS live endpoints for dynamic mounts (notably RTMP
//! publishers that create their mount after startup).
use super::{
    HlsConfig, handler,
    segment_ring::SegmentRing,
    segmenter::{Segmenter, SegmenterConfig},
};
use crate::{
    MountRegistry,
    http::Handler,
    mount::{FrameRecvError, MountHandle},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

struct Channel {
    ring: Arc<SegmentRing>,
    started: Arc<Mutex<bool>>,
    /// Cache-busting token for init/segments. A server restart must not
    /// reuse the previous run's browser-cached init.m4s (same URL, new
    /// SPS/PPS).
    generation: String,
}

/// Per-registry HLS live manager. One segmenter/channel per mount.
pub struct HlsLiveManager {
    registry: Arc<MountRegistry>,
    config: HlsConfig,
    channels: Mutex<HashMap<String, Arc<Channel>>>,
}

impl HlsLiveManager {
    pub fn new(registry: Arc<MountRegistry>, config: HlsConfig) -> Self {
        Self {
            registry,
            config,
            channels: Mutex::new(HashMap::new()),
        }
    }

    fn channel(&self, mount: &MountHandle) -> Arc<Channel> {
        let key = mount.path().to_owned();
        self.channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key)
            .or_insert_with(|| {
                Arc::new(Channel {
                    ring: Arc::new(SegmentRing::new(self.config.segment_count)),
                    started: Arc::new(Mutex::new(false)),
                    generation: format!(
                        "{:x}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos()
                    ),
                })
            })
            .clone()
    }

    fn ensure_started(&self, mount: &MountHandle, channel: &Arc<Channel>) {
        let mut started = channel
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *started {
            return;
        }
        let description = mount.description();
        if description.tracks.is_empty() {
            return;
        }
        *started = true;
        let ring = channel.ring.clone();
        let mount = mount.clone();
        let cancel = mount.cancellation_token();
        let seg_config = SegmenterConfig {
            segment_duration: self.config.segment_duration,
            lookback_count: self.config.segment_count,
        };
        let started_flag = channel.started.clone();
        tokio::spawn(async move {
            let mut segmenter = Segmenter::new(seg_config, &description);
            let mut segments = segmenter.subscribe();
            let (mut receiver, startup) = mount.subscribe_with_startup();
            let startup_keys = crate::mount::startup_dedup_keys(&startup);
            let mut startup = std::collections::VecDeque::from(startup);
            let has_video = description
                .tracks
                .iter()
                .any(|track| track.kind == crate::media::MediaKind::Video);
            let mut awaiting_keyframe = has_video;
            let mut from_startup = true;
            loop {
                let unit = if let Some(unit) = startup.pop_front() {
                    unit
                } else {
                    from_startup = false;
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        result = receiver.recv() => match result {
                            Ok(unit) => unit,
                            Err(FrameRecvError::Lagged(count)) => {
                                tracing::debug!(count, "HLS live lagged; waiting for a keyframe");
                                segmenter.reset_after_discontinuity();
                                awaiting_keyframe = has_video;
                                continue;
                            }
                            Err(FrameRecvError::Closed) => break,
                        },
                    }
                };
                if awaiting_keyframe {
                    let video = matches!(
                        unit.codec,
                        crate::media::Codec::H264 | crate::media::Codec::H265
                    );
                    if video {
                        if !unit.is_keyframe {
                            continue;
                        }
                        awaiting_keyframe = false;
                    } else {
                        // Do not let audio start a segment before the first
                        // video IDR: the segment would begin at an audio PTS
                        // and the video track would join it mid-GOP. hls.js
                        // then has no decodable video at the segment start.
                        continue;
                    }
                }
                if from_startup || !startup_keys.contains(&(unit.track_id, unit.rtp_timestamp)) {
                    segmenter.push((*unit).clone());
                }
                while let Ok(segment) = segments.try_recv() {
                    ring.push(segment);
                }
            }
            ring.clear();
            *started_flag
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
        });
    }
}

fn mount_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/hls/")?;
    if let Some(mount) = rest.strip_suffix("/index.m3u8") {
        return Some((mount, "playlist"));
    }
    if let Some(mount) = rest.strip_suffix("/init.m4s") {
        return Some((mount, "init"));
    }
    let (mount, file) = rest.rsplit_once('/')?;
    if file.ends_with(".m4s") {
        return Some((mount, "segment"));
    }
    None
}

/// Handler for `/hls/{mount}/index.m3u8`, `/init.m4s`, `/{seq}.m4s`.
pub fn create_hls_live_handler(manager: Arc<HlsLiveManager>) -> Handler {
    Arc::new(move |req, mut writer| {
        let manager = manager.clone();
        Box::pin(async move {
            let Some((mount_path, kind)) =
                mount_path(&req.path).map(|(mount, kind)| (mount.to_owned(), kind))
            else {
                writer.write_body(404, "text/plain", b"Not Found").await?;
                return Ok(writer);
            };
            let Some(mount) = manager.registry.get_handle(&format!("/{mount_path}")) else {
                writer
                    .write_body(404, "text/plain", b"Mount not found")
                    .await?;
                return Ok(writer);
            };
            let channel = manager.channel(&mount);
            manager.ensure_started(&mount, &channel);
            match kind {
                "playlist" => {
                    if mount.description().tracks.is_empty() {
                        writer
                            .write_headers(
                                503,
                                &[
                                    ("Retry-After", "2"),
                                    ("Content-Length", "0"),
                                    ("Connection", "close"),
                                ],
                            )
                            .await?;
                        return Ok(writer);
                    }
                    // hls.js treats a live playlist without any segment
                    // (or without EXT-X-MEDIA-SEQUENCE) as a fatal
                    // internalException. Wait for the first segment after
                    // starting the segmenter; keep a bounded wait so a
                    // publisher that never sends data gets 503 + retry.
                    if channel.ring.is_empty() {
                        for _ in 0..60 {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            if !channel.ring.is_empty() {
                                break;
                            }
                        }
                    }
                    if channel.ring.is_empty() {
                        writer
                            .write_headers(
                                503,
                                &[
                                    ("Retry-After", "1"),
                                    ("Content-Length", "0"),
                                    ("Connection", "close"),
                                ],
                            )
                            .await?;
                        return Ok(writer);
                    }
                    let mut req = req;
                    req.query.insert("v".to_owned(), channel.generation.clone());
                    handler::serve_playlist(
                        req,
                        writer,
                        &channel.ring,
                        true,
                        manager.config.segment_duration,
                        "",
                    )
                    .await
                }
                "init" => handler::serve_init_segment(req, writer, &channel.ring).await,
                "segment" => {
                    let sequence = req
                        .path
                        .rsplit('/')
                        .next()
                        .and_then(|file| file.strip_suffix(".m4s"))
                        .and_then(|value| value.parse::<u64>().ok())
                        .unwrap_or(u64::MAX);
                    handler::serve_segment(req, writer, &channel.ring, sequence).await
                }
                _ => {
                    writer.write_body(404, "text/plain", b"Not Found").await?;
                    Ok(writer)
                }
            }
        })
    })
}

// Keep the type visible in docs/tests without exposing internals.
#[allow(dead_code)]
fn _assert_send_sync() {
    fn check<T: Send + Sync>() {}
    check::<HlsLiveManager>();
}
