//! Media HTTP handlers: auth guard, HTTP-FLV, DASH and health probes.
use crate::{auth::AppAuth, config::Config, vod::VodCatalog};
use rtflow::{
    MountRegistry,
    dash::{DashConfig, DashHandlers, create_dash_router, mpd::MpdConfig},
    http::Handler,
    http_flv::handler::serve_flv_stream,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

pub(crate) fn health_handler() -> Handler {
    Arc::new(|_req, mut writer| {
        Box::pin(async move {
            writer
                .write_body(200, "application/json", br#"{"status":"ok"}"#)
                .await?;
            Ok(writer)
        })
    })
}

pub(crate) fn auth_guard(
    inner: Handler,
    auth: Arc<AppAuth>,
    mounts: Arc<MountRegistry>,
    prefix: &'static str,
    role: rtflow::auth::AccessRole,
    protocol: &'static str,
) -> Handler {
    Arc::new(move |req, writer| {
        let auth = auth.clone();
        let mounts = mounts.clone();
        let inner = inner.clone();
        Box::pin(async move {
            let full = req.path.trim_start_matches('/').to_owned();
            if prefix == "/vod/" {
                // VOD ids are not mount paths; authorize against the VOD
                // namespace so `vod/**` tokens and read rules apply.
                if !auth.verify_http_request("vod", &req, role, protocol) {
                    let mut writer = writer;
                    writer
                        .write_body(401, "text/plain", b"unauthorized")
                        .await?;
                    return Ok(writer);
                }
                return inner(req, writer).await;
            }
            let mount_path = mounts
                .find_longest_prefix(&format!("/{full}"))
                .map(|(id, _)| id.as_str().trim_start_matches('/').to_owned())
                .or_else(|| {
                    req.path.strip_prefix(prefix).map(|rest| {
                        if protocol == "flv" {
                            return rest.trim_end_matches(".flv").to_owned();
                        }
                        // HLS/LL-HLS/DASH: the mount is the directory that
                        // contains the playlist/manifest/segment file.
                        rest.rsplit_once('/')
                            .map(|(dir, _)| dir.to_owned())
                            .unwrap_or_else(|| rest.to_owned())
                    })
                })
                .unwrap_or_else(|| full.clone());
            tracing::debug!(
                protocol,
                request_path = %req.path,
                mount_path = %mount_path,
                "media auth check"
            );
            if !auth.verify_http_request(&mount_path, &req, role, protocol) {
                let mut writer = writer;
                writer
                    .write_body(401, "text/plain", b"unauthorized")
                    .await?;
                return Ok(writer);
            }
            inner(req, writer).await
        })
    })
}

/// VOD ids are not mount paths: map an id back to the recording's mount path
/// and authorize against that path's read policy.
pub(crate) fn vod_auth_guard(
    inner: Handler,
    auth: Arc<AppAuth>,
    vod: Arc<VodCatalog>,
    prefix: &'static str,
) -> Handler {
    Arc::new(move |req, writer| {
        let auth = auth.clone();
        let vod = vod.clone();
        let inner = inner.clone();
        Box::pin(async move {
            let id = req
                .path
                .strip_prefix(prefix)
                .and_then(|rest| rest.split('/').next())
                .unwrap_or("")
                .to_owned();
            let mount_path = vod.source_path(&id).unwrap_or_else(|| id.clone());
            if !auth.verify_http_request(&mount_path, &req, rtflow::auth::AccessRole::Read, "vod") {
                let mut writer = writer;
                writer
                    .write_body(401, "text/plain", b"unauthorized")
                    .await?;
                return Ok(writer);
            }
            inner(req, writer).await
        })
    })
}

pub(crate) fn flv_handler(mounts: Arc<MountRegistry>) -> Handler {
    Arc::new(move |req, writer| {
        let mounts = mounts.clone();
        Box::pin(async move {
            let path = req
                .path
                .trim_start_matches("/flv/")
                .strip_suffix(".flv")
                .unwrap_or("");
            tracing::debug!(request_path = %req.path, mount = %path, "FLV request");
            let Some(handle) = mounts.get_handle(&format!("/{path}")) else {
                tracing::debug!(mount = %path, "FLV mount not found");
                let mut writer = writer;
                writer
                    .write_body(404, "text/plain", b"stream not found")
                    .await?;
                return Ok(writer);
            };
            let description = handle.description();
            let (receiver, startup) = handle.subscribe_with_startup();
            serve_flv_stream(req, writer, description, receiver, startup, handle).await
        })
    })
}

struct CachedDash {
    mount: Arc<rtflow::mount::Mount>,
    handlers: Arc<DashHandlers>,
}

fn dash_config(config: &Config) -> DashConfig {
    let segment_duration = config
        .dash
        .segment_duration
        .unwrap_or(config.hls.segment_duration);
    let segment_count = config.dash.window.unwrap_or(config.hls.window);
    let time_shift_buffer_depth = config
        .dash
        .time_shift_buffer_depth
        .unwrap_or_else(|| segment_duration.saturating_mul(segment_count as u32));
    DashConfig {
        segment_duration,
        segment_count,
        mpd: MpdConfig {
            min_buffer_time: config.dash.min_buffer_time,
            time_shift_buffer_depth,
            segment_duration,
            suggested_presentation_delay: config.dash.suggested_presentation_delay,
            utc_timing_url: Some("/api/v1/time".into()),
            ..MpdConfig::default()
        },
    }
}

pub(crate) fn dash_handler(mounts: Arc<MountRegistry>, config: Arc<Config>) -> Handler {
    let cache: Arc<Mutex<HashMap<String, CachedDash>>> = Arc::new(Mutex::new(HashMap::new()));
    Arc::new(move |req, writer| {
        let mounts = mounts.clone();
        let config = config.clone();
        let cache = cache.clone();
        Box::pin(async move {
            let Some((mount_path, kind)) = dash_route(&req.path) else {
                let mut writer = writer;
                writer.write_body(404, "text/plain", b"not found").await?;
                return Ok(writer);
            };
            let Some(handle) = mounts.get_handle(&format!("/{mount_path}")) else {
                let mut writer = writer;
                writer
                    .write_body(404, "text/plain", b"mount not found")
                    .await?;
                return Ok(writer);
            };
            let handlers = {
                let mut cache = cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let current = handle.mount();
                let stale = cache
                    .get(mount_path)
                    .is_none_or(|entry| !Arc::ptr_eq(&entry.mount, current));
                if stale {
                    cache.insert(
                        mount_path.to_owned(),
                        CachedDash {
                            mount: current.clone(),
                            handlers: Arc::new(create_dash_router(dash_config(&config), &handle)),
                        },
                    );
                }
                cache
                    .get(mount_path)
                    .expect("just inserted")
                    .handlers
                    .clone()
            };
            let handler = match kind {
                DashKind::Manifest => handlers.manifest.clone(),
                DashKind::Init => handlers.init.clone(),
                DashKind::Segment => handlers.segment.clone(),
            };
            handler(req, writer).await
        })
    })
}

enum DashKind {
    Manifest,
    Init,
    Segment,
}

fn dash_route(path: &str) -> Option<(&str, DashKind)> {
    let path = path.strip_prefix("/dash/")?;
    if let Some(mount) = path.strip_suffix("/manifest.mpd") {
        return Some((mount, DashKind::Manifest));
    }
    let (mount, file) = path.rsplit_once('/')?;
    if file.ends_with("-init.mp4") || file == "init.mp4" || file == "init.m4s" {
        return Some((mount, DashKind::Init));
    }
    if file.ends_with(".m4s") {
        return Some((mount, DashKind::Segment));
    }
    None
}
