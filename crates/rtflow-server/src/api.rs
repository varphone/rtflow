//! HTTP control-plane API (paths, keys, recordings, VOD, discovery).
use crate::{
    audit::{AuditEvent, AuditLog},
    config::Config,
    discovery::DiscoveryService,
    keys::{KeySpec, KeyStore},
    paths::{PathSpec, PathStore},
    recording::RecordingSupervisor,
    vod::VodCatalog,
};
use base64::Engine as _;
use rtflow::{
    MountRegistry,
    http::{Handler, HttpRequest, HttpServer, Method, ResponseWriter},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use subtle::ConstantTimeEq;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn register_api_routes(
    http: &HttpServer,
    config: Arc<Config>,
    discovery: Arc<DiscoveryService>,
    paths: Arc<PathStore>,
    keys: Arc<KeyStore>,
    vod: Arc<VodCatalog>,
    recording: Arc<RecordingSupervisor>,
    mounts: Arc<MountRegistry>,
    audit_log: Option<Arc<AuditLog>>,
    admin: Option<Arc<rtflow::admin::Admin>>,
) {
    let state = ApiState {
        config,
        discovery,
        paths,
        keys,
        vod,
        recording,
        mounts,
        audit: audit_log,
        admin,
        idempotency: std::sync::Mutex::new(std::collections::HashMap::new()),
    };
    let state = Arc::new(state);
    http.add_route(
        Method::Get,
        "/.well-known/rtflow",
        api_open_json(state.clone(), |state, _req| async move {
            Ok(json!({
                "name": state.config.server.name,
                "version": env!("CARGO_PKG_VERSION"),
                "protocols": {
                    "rtsp": state.config.rtsp.enabled,
                    "rtmp": state.config.rtmp.enabled,
                    "srt": state.config.srt.enabled,
                    "webrtc": state.config.webrtc.enabled,
                    "hls": state.config.hls.enabled,
                    "dash": state.config.dash.enabled,
                    "flv": state.config.flv.enabled,
                },
                "api": "/api/v1/discovery"
            }))
        }),
    )
    .await;
    if state.config.discovery.compat_mediamtx {
        http.add_route(
            Method::Get,
            "/v3/paths/list",
            api_public_json(state.clone(), |state, _req| async move {
                let paths = state.discovery.list();
                let items: Vec<serde_json::Value> = paths
                    .iter()
                    .map(|path| {
                        json!({
                            "name": path.name,
                            "confName": path.name,
                            "source": if path.publisher { "publisher" } else { "unknown" },
                            "ready": path.online,
                            "readyTime": null,
                            "tracks": path.tracks.iter().map(|track| track.codec.clone()).collect::<Vec<_>>(),
                            "bytesReceived": path.bytes,
                            "readers": path.readers,
                        })
                    })
                    .collect();
                Ok(json!({
                    "itemCount": items.len(),
                    "pageCount": 1,
                    "items": items,
                }))
            }),
        )
        .await;
        http.add_route(
            Method::Get,
            "/v3/paths/get/*",
            api_public_json(state.clone(), |state, req| async move {
                let name = req.path.strip_prefix("/v3/paths/get/").unwrap_or("");
                state
                    .discovery
                    .status(name)
                    .map(|path| {
                        json!({
                            "name": path.name,
                            "ready": path.online,
                            "source": if path.publisher { "publisher" } else { "unknown" },
                            "tracks": path.tracks.iter().map(|track| track.codec.clone()).collect::<Vec<_>>(),
                            "bytesReceived": path.bytes,
                            "readers": path.readers,
                        })
                    })
                    .ok_or((404, "path not found".into()))
            }),
        )
        .await;
    }
    http.add_route(
        Method::Get,
        "/api/v1/version",
        api_json(state.clone(), |_state, _req| async move {
            Ok(json!({"name": env!("CARGO_PKG_NAME"), "version": env!("CARGO_PKG_VERSION")}))
        }),
    )
    .await;
    http.add_route(
        Method::Get,
        "/api/v1/discovery",
        api_public_json(state.clone(), |state, _req| async move {
            Ok(serde_json::to_value(state.discovery.discovery()).unwrap_or_default())
        }),
    )
    .await;
    http.add_route(
        Method::Get,
        "/api/v1/vods",
        api_public_json(state.clone(), |state, _req| async move {
            Ok(serde_json::to_value(state.vod.list()).unwrap_or_default())
        }),
    )
    .await;
    http.add_route(
        Method::Get,
        "/api/v1/paths",
        api_public_json(state.clone(), |state, _req| async move {
            Ok(serde_json::to_value(state.discovery.list()).unwrap_or_default())
        }),
    )
    .await;
    http.add_route(
        Method::Get,
        "/api/v1/paths/*",
        api_public_json(state.clone(), |state, req| async move {
            let Some(name) = req.path.strip_prefix("/api/v1/paths/") else {
                return Err((400, "missing path".to_string()));
            };
            state
                .discovery
                .status(name)
                .map(|status| serde_json::to_value(status).unwrap_or_default())
                .ok_or((404, "path not found".into()))
        }),
    )
    .await;
    // Administrative endpoints use the console credentials (Basic) so they
    // are usable from scripts without a browser session.
    http.add_route(
        Method::Get,
        "/admin/api/v1/paths",
        api_json(state.clone(), |state, _req| async move {
            Ok(serde_json::to_value(state.discovery.list()).unwrap_or_default())
        }),
    )
    .await;
    http.add_route(
        Method::Get,
        "/admin/api/v1/paths/*",
        api_json(state.clone(), |state, req| async move {
            let Some(name) = req.path.strip_prefix("/admin/api/v1/paths/") else {
                return Err((400, "missing path".to_string()));
            };
            if !state.paths.known(name) {
                return Err((404, "path not found".into()));
            }
            Ok(serde_json::to_value(state.paths.resolve(name)).unwrap_or_default())
        }),
    )
    .await;
    http.add_route(
        Method::Post,
        "/admin/api/v1/paths",
        api_write(state.clone(), |state, req| async move {
            let body: CreatePath = serde_json::from_slice(&req.body)
                .map_err(|error| (400, format!("invalid body: {error}")))?;
            match state.paths.create(&body.name, body.spec) {
                Ok(record) => {
                    audit(
                        &state,
                        AuditEvent::new("path.create", "ok")
                            .path(&record.name)
                            .detail(format!("source={:?}", record.source)),
                    );
                    Ok(serde_json::to_value(record).unwrap_or_default())
                }
                Err(error) => {
                    audit(
                        &state,
                        AuditEvent::new("path.create", "failed")
                            .path(&body.name)
                            .detail(error.to_string()),
                    );
                    Err((409, error.to_string()))
                }
            }
        }),
    )
    .await;
    http.add_route(
        Method::Patch,
        "/admin/api/v1/paths/*",
        api_write(state.clone(), |state, req| async move {
            let Some(name) = req.path.strip_prefix("/admin/api/v1/paths/") else {
                return Err((400, "missing path".to_string()));
            };
            if let Some(expected) = req.header("if-match") {
                let current = state.paths.get(name);
                let Some(record) = current else {
                    return Err((404, "path not found".to_string()));
                };
                let expected = expected.trim().trim_matches('"');
                if expected != record.updated_at.to_string() {
                    return Err((
                        412,
                        format!(
                            "precondition failed: current version is {}",
                            record.updated_at
                        ),
                    ));
                }
            }
            let patch: crate::paths::PathPatch = serde_json::from_slice(&req.body)
                .map_err(|error| (400, format!("invalid body: {error}")))?;
            match state.paths.patch(name, patch) {
                Ok(record) => {
                    audit(&state, AuditEvent::new("path.update", "ok").path(name));
                    Ok(serde_json::to_value(record).unwrap_or_default())
                }
                Err(error) => {
                    audit(
                        &state,
                        AuditEvent::new("path.update", "failed")
                            .path(name)
                            .detail(error.to_string()),
                    );
                    Err((404, error.to_string()))
                }
            }
        }),
    )
    .await;
    http.add_route(
        Method::Delete,
        "/admin/api/v1/paths/*",
        api_write(state.clone(), |state, req| async move {
            let Some(name) = req.path.strip_prefix("/admin/api/v1/paths/") else {
                return Err((400, "missing path".to_string()));
            };
            let record = match state.paths.remove(name) {
                Ok(record) => record,
                Err(error) => {
                    audit(
                        &state,
                        AuditEvent::new("path.delete", "failed")
                            .path(name)
                            .detail(error.to_string()),
                    );
                    return Err((404, error.to_string()));
                }
            };
            if let Some(mount) = state.mounts.unregister(&format!("/{name}")) {
                mount.mount().cancel.cancel();
            }
            audit(&state, AuditEvent::new("path.delete", "ok").path(name));
            Ok(serde_json::to_value(record).unwrap_or_default())
        }),
    )
    .await;
    http.add_route(
        Method::Post,
        "/admin/api/v1/kick",
        api_write(state.clone(), |state, req| async move {
            let Some(name) = req.query.get("path").map(String::as_str) else {
                return Err((400, "missing path".to_string()));
            };
            let Some(mount) = state.mounts.unregister(&format!("/{name}")) else {
                return Err((404, "mount is not online".to_string()));
            };
            mount.mount().cancel.cancel();
            audit(&state, AuditEvent::new("path.kick", "ok").path(name));
            Ok(json!({"ok": true}))
        }),
    )
    .await;
    http.add_route(
        Method::Get,
        "/admin/api/v1/vods",
        api_json(state.clone(), |state, _req| async move {
            Ok(serde_json::to_value(state.vod.list()).unwrap_or_default())
        }),
    )
    .await;
    http.add_route(
        Method::Post,
        "/admin/api/v1/vods/refresh",
        api_write(state.clone(), |state, _req| async move {
            let count = state.vod.refresh();
            audit(
                &state,
                AuditEvent::new("vod.refresh", "ok").detail(format!("entries={count}")),
            );
            Ok(json!({"count": count}))
        }),
    )
    .await;
    http.add_route(
        Method::Get,
        "/admin/api/v1/recordings",
        api_json(state.clone(), |state, _req| async move {
            Ok(serde_json::to_value(state.recording.status()).unwrap_or_default())
        }),
    )
    .await;
    http.add_route(
        Method::Post,
        "/admin/api/v1/recordings/start",
        api_write(state.clone(), |state, req| async move {
            let Some(path) = req.query.get("path").map(String::as_str) else {
                return Err((400, "missing path".to_string()));
            };
            match state.recording.start(path).await {
                Ok(()) => {
                    audit(&state, AuditEvent::new("record.start", "ok").path(path));
                    Ok(json!({"ok": true}))
                }
                Err(error) => {
                    audit(
                        &state,
                        AuditEvent::new("record.start", "failed")
                            .path(path)
                            .detail(&error),
                    );
                    Err((409, error))
                }
            }
        }),
    )
    .await;
    http.add_route(
        Method::Post,
        "/admin/api/v1/recordings/stop",
        api_write(state.clone(), |state, req| async move {
            let Some(path) = req.query.get("path").map(String::as_str) else {
                return Err((400, "missing path".to_string()));
            };
            match state.recording.stop(path).await {
                Ok(()) => {
                    audit(&state, AuditEvent::new("record.stop", "ok").path(path));
                    Ok(json!({"ok": true}))
                }
                Err(error) => {
                    audit(
                        &state,
                        AuditEvent::new("record.stop", "failed")
                            .path(path)
                            .detail(&error),
                    );
                    Err((409, error))
                }
            }
        }),
    )
    .await;
    http.add_route(
        Method::Get,
        "/admin/api/v1/keys",
        api_json(state.clone(), |state, _req| async move {
            Ok(serde_json::to_value(state.keys.list()).unwrap_or_default())
        }),
    )
    .await;
    http.add_route(
        Method::Post,
        "/admin/api/v1/keys",
        api_write(state.clone(), |state, req| async move {
            let spec: KeySpec = serde_json::from_slice(&req.body)
                .map_err(|error| (400, format!("invalid body: {error}")))?;
            match state.keys.issue(spec) {
                Ok(issued) => {
                    audit(
                        &state,
                        AuditEvent::new("key.issue", "ok")
                            .key_id(&issued.status.kid)
                            .detail(format!("paths={:?}", issued.status.paths)),
                    );
                    Ok(json!({"key": issued.status, "secret": issued.secret}))
                }
                Err(error) => {
                    audit(
                        &state,
                        AuditEvent::new("key.issue", "failed").detail(error.to_string()),
                    );
                    Err((500, error.to_string()))
                }
            }
        }),
    )
    .await;
    http.add_route(
        Method::Delete,
        "/admin/api/v1/keys/{kid}",
        api_write(state.clone(), |state, req| async move {
            let Some(kid) = req.path_params.first() else {
                return Err((400, "missing key id".to_string()));
            };
            match state.keys.revoke(kid) {
                Ok(()) => {
                    audit(&state, AuditEvent::new("key.revoke", "ok").key_id(kid));
                    Ok(json!({"ok": true}))
                }
                Err(error) => {
                    audit(
                        &state,
                        AuditEvent::new("key.revoke", "failed")
                            .key_id(kid)
                            .detail(error.to_string()),
                    );
                    Err((404, error.to_string()))
                }
            }
        }),
    )
    .await;
}

#[derive(Deserialize)]
struct CreatePath {
    name: String,
    #[serde(flatten)]
    spec: PathSpec,
}

struct ApiState {
    config: Arc<Config>,
    discovery: Arc<DiscoveryService>,
    paths: Arc<PathStore>,
    keys: Arc<KeyStore>,
    vod: Arc<VodCatalog>,
    recording: Arc<RecordingSupervisor>,
    mounts: Arc<MountRegistry>,
    audit: Option<Arc<AuditLog>>,
    admin: Option<Arc<rtflow::admin::Admin>>,
    /// Replayed responses for `Idempotency-Key` POSTs.
    idempotency: std::sync::Mutex<
        std::collections::HashMap<String, (std::time::Instant, u16, serde_json::Value)>,
    >,
}

fn audit(state: &ApiState, event: AuditEvent) {
    if let Some(log) = &state.audit {
        log.record(&event);
    }
}

fn api_json<F, Fut>(state: Arc<ApiState>, handler: F) -> Handler
where
    F: Fn(Arc<ApiState>, HttpRequest) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<serde_json::Value, (u16, String)>> + Send + 'static,
{
    let handler = Arc::new(handler);
    Arc::new(move |req, writer| {
        let state = state.clone();
        let handler = handler.clone();
        Box::pin(async move {
            if !api_authorized(&state, &req) {
                return write_api_error(writer, 401, "unauthorized").await;
            }
            match handler(state, req).await {
                Ok(value) => write_api_json(writer, 200, &value).await,
                Err((status, message)) => write_api_error(writer, status, &message).await,
            }
        })
    })
}

fn api_open_json<F, Fut>(state: Arc<ApiState>, handler: F) -> Handler
where
    F: Fn(Arc<ApiState>, HttpRequest) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<serde_json::Value, (u16, String)>> + Send + 'static,
{
    let handler = Arc::new(handler);
    Arc::new(move |req, writer| {
        let state = state.clone();
        let handler = handler.clone();
        Box::pin(async move {
            match handler(state, req).await {
                Ok(value) => write_api_json(writer, 200, &value).await,
                Err((status, message)) => write_api_error(writer, status, &message).await,
            }
        })
    })
}

fn api_public_json<F, Fut>(state: Arc<ApiState>, handler: F) -> Handler
where
    F: Fn(Arc<ApiState>, HttpRequest) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<serde_json::Value, (u16, String)>> + Send + 'static,
{
    let handler = Arc::new(handler);
    Arc::new(move |req, writer| {
        let state = state.clone();
        let handler = handler.clone();
        Box::pin(async move {
            if !public_authorized(&state, &req) {
                return write_api_error(writer, 401, "unauthorized").await;
            }
            match handler(state, req).await {
                Ok(value) => write_api_json(writer, 200, &value).await,
                Err((status, message)) => write_api_error(writer, status, &message).await,
            }
        })
    })
}

fn public_authorized(state: &ApiState, req: &HttpRequest) -> bool {
    if api_authorized(state, req) {
        return true;
    }
    if state.config.discovery.public {
        if let Some(token) = state.config.discovery.token.as_deref() {
            return request_token(req)
                .is_some_and(|provided| provided.as_bytes().ct_eq(token.as_bytes()).into());
        }
        return true;
    }
    if let Some(token) = state.config.discovery.token.as_deref() {
        return request_token(req)
            .is_some_and(|provided| provided.as_bytes().ct_eq(token.as_bytes()).into());
    }
    false
}

fn request_token(req: &HttpRequest) -> Option<&str> {
    if let Some(header) = req.header("authorization")
        && let Some(token) = header.strip_prefix("Bearer ")
    {
        return Some(token.trim());
    }
    req.query.get("token").map(String::as_str)
}

fn api_write<F, Fut>(state: Arc<ApiState>, handler: F) -> Handler
where
    F: Fn(Arc<ApiState>, HttpRequest) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<serde_json::Value, (u16, String)>> + Send + 'static,
{
    let handler = Arc::new(handler);
    Arc::new(move |req, writer| {
        let state = state.clone();
        let handler = handler.clone();
        Box::pin(async move {
            if !api_authorized(&state, &req) {
                return write_api_error(writer, 401, "unauthorized").await;
            }
            // Idempotent retries: a POST with the same Idempotency-Key returns
            // the first response instead of creating a second resource.
            let key = req
                .header("idempotency-key")
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            let cached = key.as_ref().and_then(|key| {
                let cache = state
                    .idempotency
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                cache
                    .get(key)
                    .cloned()
                    .filter(|(stored, _, _)| stored.elapsed() < std::time::Duration::from_secs(600))
            });
            if let Some((_, status, value)) = cached {
                return write_api_json(writer, status, &value).await;
            }
            match handler(state.clone(), req).await {
                Ok(value) => {
                    if let Some(key) = key {
                        let mut cache = state
                            .idempotency
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        cache.retain(|_, (at, _, _)| {
                            at.elapsed() < std::time::Duration::from_secs(600)
                        });
                        cache.insert(key, (std::time::Instant::now(), 200, value.clone()));
                        drop(cache);
                    }
                    write_api_json(writer, 200, &value).await
                }
                Err((status, message)) => write_api_error(writer, status, &message).await,
            }
        })
    })
}

fn api_authorized(state: &ApiState, req: &HttpRequest) -> bool {
    let config = &state.config;
    // Browser session from the console login.
    if let Some(admin) = &state.admin
        && admin.is_authenticated(req)
    {
        return true;
    }
    // Static API token: `Authorization: Bearer ...` or `?token=`.
    if !config.http.api_tokens.is_empty() {
        let provided = req
            .header("authorization")
            .and_then(|header| header.strip_prefix("Bearer ").map(str::trim))
            .or_else(|| req.query.get("token").map(String::as_str));
        if let Some(provided) = provided
            && config
                .http
                .api_tokens
                .iter()
                .any(|token| provided.as_bytes().ct_eq(token.as_bytes()).into())
        {
            return true;
        }
    }
    if config.http.admin_password.is_empty() {
        return false;
    }
    let Some(header) = req.header("authorization") else {
        return false;
    };
    let Some(encoded) = header.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
        return false;
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return false;
    };
    let Some((username, password)) = text.split_once(':') else {
        return false;
    };
    username
        .as_bytes()
        .ct_eq(config.http.admin_username.as_bytes())
        .into()
        && password
            .as_bytes()
            .ct_eq(config.http.admin_password.as_bytes())
            .into()
}

async fn write_api_json<W: tokio::io::AsyncWrite + Unpin>(
    mut writer: ResponseWriter<W>,
    status: u16,
    value: &serde_json::Value,
) -> rtflow::Result<ResponseWriter<W>> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{\"error\":\"serialize\"}".to_vec());
    let length = body.len().to_string();
    writer
        .write_headers(
            status,
            &[
                ("Content-Type", "application/json"),
                ("Content-Length", length.as_str()),
                ("Cache-Control", "no-store"),
                ("Connection", "close"),
            ],
        )
        .await?;
    writer.write_raw(&body).await?;
    writer.flush().await?;
    Ok(writer)
}

async fn write_api_error<W: tokio::io::AsyncWrite + Unpin>(
    writer: ResponseWriter<W>,
    status: u16,
    message: &str,
) -> rtflow::Result<ResponseWriter<W>> {
    write_api_json(writer, status, &json!({"error": message})).await
}
