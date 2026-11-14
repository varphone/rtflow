//! Read-only console API and pages for live mounts and HLS VOD.
#[cfg(feature = "hls")]
use crate::hls::vod::HlsVodManager;
use crate::{
    MountRegistry,
    http::{Handler, ResponseWriter},
    media::MediaKind,
};
use std::sync::Arc;

const CONSOLE_HTML: &str = include_str!("console.html");

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
        .replace('<', "\\u003c")
}

fn mounts_json(registry: &MountRegistry) -> String {
    let mut out = String::from("[");
    let mut first = true;
    for id in registry.paths() {
        let Some(handle) = registry.get_handle(id.as_str()) else {
            continue;
        };
        let description = handle.description();
        let video = description
            .tracks
            .iter()
            .find(|track| track.kind == MediaKind::Video);
        let audio = description
            .tracks
            .iter()
            .find(|track| track.kind == MediaKind::Audio);
        let snapshot = handle.snapshot();
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&format!(
            "{{\"path\":\"{}\",\"id\":\"{}\",\"name\":\"{}\",\"video\":\"{}\",\"audio\":\"{}\",\
             \"width\":{},\"height\":{},\"publisher\":{},\"subscribers\":{},\
             \"accepted\":{},\"dropped\":{},\"rtp_packets\":{},\"rtp_octets\":{},\
             \"hls\":\"/hls{}/index.m3u8\",\"ingest\":\"rtmp://HOST/{}\"}}",
            escape(handle.path()),
            escape(id.as_str()),
            escape(handle.path().rsplit('/').next().unwrap_or(handle.path())),
            video
                .map(|track| format!("{:?}", track.codec))
                .unwrap_or_default(),
            audio
                .map(|track| format!("{:?}", track.codec))
                .unwrap_or_default(),
            video.and_then(|track| track.width).map_or(-1, i64::from),
            video.and_then(|track| track.height).map_or(-1, i64::from),
            snapshot.publisher_connected,
            snapshot.subscriber_count,
            snapshot.stats.accepted_frames,
            snapshot.stats.dropped_frames,
            snapshot.stats.rtp_packets,
            snapshot.stats.rtp_octets,
            escape(handle.path()),
            escape(handle.path().trim_start_matches('/')),
        ));
    }
    out.push(']');
    out
}

#[cfg(feature = "hls")]
fn vods_json(vod: Option<&Arc<HlsVodManager>>) -> String {
    let Some(vod) = vod else {
        return "[]".to_owned();
    };
    let mut entries = vod.entries().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.id.cmp(&right.id));
    let mut out = String::from("[");
    for (index, entry) in entries.into_iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"id\":\"{}\",\"title\":\"{}\",\"playlist\":\"/vod/{}/index.m3u8\",\"duration_ms\":{}}}",
            escape(&entry.id),
            escape(&entry.title),
            escape(&entry.id),
            entry.duration.map_or(0, |duration| duration.as_millis()),
        ));
    }
    out.push(']');
    out
}

/// `GET /api/mounts` — live mount status.
pub fn create_mounts_api(registry: Arc<MountRegistry>) -> Handler {
    Arc::new(move |_req, mut writer: ResponseWriter<_>| {
        let body = mounts_json(&registry);
        Box::pin(async move {
            writer
                .write_headers(
                    200,
                    &[
                        ("Content-Type", "application/json"),
                        ("Cache-Control", "no-store"),
                        ("Connection", "close"),
                    ],
                )
                .await?;
            writer.write_raw(body.as_bytes()).await?;
            writer.flush().await?;
            Ok(writer)
        })
    })
}

/// `GET /api/vods` — available HLS VOD entries.
#[cfg(feature = "hls")]
pub fn create_vods_api(vod: Option<Arc<HlsVodManager>>) -> Handler {
    Arc::new(move |_req, mut writer: ResponseWriter<_>| {
        let body = vods_json(vod.as_ref());
        Box::pin(async move {
            writer
                .write_headers(
                    200,
                    &[
                        ("Content-Type", "application/json"),
                        ("Cache-Control", "no-store"),
                        ("Connection", "close"),
                    ],
                )
                .await?;
            writer.write_raw(body.as_bytes()).await?;
            writer.flush().await?;
            Ok(writer)
        })
    })
}

/// Browser console. `admin=true` renders the read-only status table.
pub fn create_console_handler(admin: bool) -> Handler {
    Arc::new(move |_req, mut writer: ResponseWriter<_>| {
        let body = CONSOLE_HTML.replace("__MODE__", if admin { "admin" } else { "browse" });
        Box::pin(async move {
            writer
                .write_headers(
                    200,
                    &[
                        ("Content-Type", "text/html; charset=utf-8"),
                        ("Cache-Control", "no-store"),
                        ("Connection", "close"),
                    ],
                )
                .await?;
            writer.write_raw(body.as_bytes()).await?;
            writer.flush().await?;
            Ok(writer)
        })
    })
}

/// `GET /api/health` — liveness plus mount count.
pub fn create_health_api(registry: Arc<MountRegistry>) -> Handler {
    Arc::new(move |_req, mut writer: ResponseWriter<_>| {
        let body = format!(
            "{{\"status\":\"ok\",\"mounts\":{}}}",
            registry.paths().len()
        );
        Box::pin(async move {
            writer
                .write_headers(
                    200,
                    &[
                        ("Content-Type", "application/json"),
                        ("Cache-Control", "no-store"),
                        ("Connection", "close"),
                    ],
                )
                .await?;
            writer.write_raw(body.as_bytes()).await?;
            writer.flush().await?;
            Ok(writer)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MountConfig, MountRegistry, media::MediaDescription};
    use std::sync::Arc;

    #[test]
    fn mounts_json_includes_dynamic_tracks() {
        let registry = MountRegistry::new(2);
        let description = MediaDescription::new(
            "cam",
            vec![crate::media::TrackDescription::h264_video(
                0,
                &[],
                &[],
                1280,
                720,
                30,
            )],
        );
        registry
            .register(Arc::from("/live/cam"), MountConfig::Local { description })
            .unwrap();
        let json = mounts_json(&registry);
        assert!(json.contains("\"path\":\"/live/cam\""));
        assert!(json.contains("\"hls\":\"/hls/live/cam/index.m3u8\""));
        assert!(json.contains("\"width\":1280"));
        assert!(json.contains("\"height\":720"));
    }
}
