//! JSON API, authentication and SPA asset handlers.
mod views;

use super::{
    AdminState,
    auth::{clear_cookie, constant_time_eq, cookie_header, session_cookie},
};
use crate::http::{Handler, HttpRequest, HttpServer, Method, ResponseWriter};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use views::{Overview, streams, vods};

const COOKIE: &str = "rtflow_admin";
const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");
const HLS_JS: &str = include_str!("../assets/hls.min.js");

struct Auth {
    token: String,
    csrf: String,
}

fn auth(state: &AdminState, req: &HttpRequest) -> Option<Auth> {
    let token = session_cookie(req, COOKIE)?;
    let csrf = state.sessions.get(&token)?;
    Some(Auth { token, csrf })
}

async fn respond_json<W: tokio::io::AsyncWrite + Unpin>(
    mut writer: ResponseWriter<W>,
    status: u16,
    value: &impl Serialize,
    cookie: Option<String>,
) -> crate::Result<ResponseWriter<W>> {
    let body = serde_json::to_vec(value)
        .map_err(|error| crate::Error::Http(format!("admin json: {error}")))?;
    let length = body.len().to_string();
    let mut headers = vec![
        ("Content-Type", "application/json"),
        ("Content-Length", length.as_str()),
        ("Cache-Control", "no-store"),
        ("Connection", "close"),
    ];
    if let Some(cookie) = cookie.as_deref() {
        headers.push(("Set-Cookie", cookie));
    }
    writer.write_headers(status, &headers).await?;
    writer.write_raw(&body).await?;
    writer.flush().await?;
    Ok(writer)
}

async fn unauthorized<W: tokio::io::AsyncWrite + Unpin>(
    writer: ResponseWriter<W>,
) -> crate::Result<ResponseWriter<W>> {
    respond_json(
        writer,
        401,
        &json!({"error":"unauthorized","message":"请先登录"}),
        None,
    )
    .await
}

async fn forbidden<W: tokio::io::AsyncWrite + Unpin>(
    writer: ResponseWriter<W>,
    message: &str,
) -> crate::Result<ResponseWriter<W>> {
    respond_json(
        writer,
        403,
        &json!({"error":"forbidden","message":message}),
        None,
    )
    .await
}

#[derive(Serialize)]
struct SessionView {
    authenticated: bool,
    title: String,
    username: String,
    csrf: String,
    readonly: bool,
    features: Vec<&'static str>,
}

fn features() -> Vec<&'static str> {
    let mut out = vec!["admin"];
    if cfg!(feature = "hls") {
        out.push("hls");
    }
    if cfg!(feature = "rtmp") {
        out.push("rtmp");
    }
    if cfg!(feature = "srtp") {
        out.push("srtp");
    }
    if cfg!(feature = "multicast") {
        out.push("multicast");
    }
    if cfg!(feature = "webrtc") {
        out.push("webrtc");
    }
    if cfg!(feature = "dash") {
        out.push("dash");
    }
    out
}

fn session_view(state: &AdminState, auth: Option<&Auth>) -> SessionView {
    SessionView {
        authenticated: auth.is_some(),
        title: state.config.title.clone(),
        username: if auth.is_some() {
            state.config.username.clone()
        } else {
            String::new()
        },
        csrf: auth.map(|auth| auth.csrf.clone()).unwrap_or_default(),
        readonly: state.config.readonly,
        features: features(),
    }
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn index_handler(state: Arc<AdminState>) -> Handler {
    Arc::new(move |_req, mut writer: ResponseWriter<_>| {
        let body = INDEX_HTML
            .replace("__BASE__", &state.config.base_path)
            .replace("__TITLE__", &html_escape(&state.config.title));
        Box::pin(async move {
            let length = body.len().to_string();
            writer
                .write_headers(
                    200,
                    &[
                        ("Content-Type", "text/html; charset=utf-8"),
                        ("Content-Length", length.as_str()),
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

fn asset_handler(
    asset: &'static str,
    content_type: &'static str,
    cache_control: &'static str,
) -> Handler {
    Arc::new(move |_req, mut writer: ResponseWriter<_>| {
        Box::pin(async move {
            let length = asset.len().to_string();
            writer
                .write_headers(
                    200,
                    &[
                        ("Content-Type", content_type),
                        ("Content-Length", length.as_str()),
                        ("Cache-Control", cache_control),
                        ("Connection", "close"),
                    ],
                )
                .await?;
            writer.write_raw(asset.as_bytes()).await?;
            writer.flush().await?;
            Ok(writer)
        })
    })
}

async fn login<W: tokio::io::AsyncWrite + Unpin>(
    state: Arc<AdminState>,
    req: HttpRequest,
    writer: ResponseWriter<W>,
) -> crate::Result<ResponseWriter<W>> {
    let Ok(body) = serde_json::from_slice::<LoginRequest>(&req.body) else {
        return respond_json(
            writer,
            400,
            &json!({"error":"bad_request","message":"请求格式错误"}),
            None,
        )
        .await;
    };
    if !constant_time_eq(&body.username, &state.config.username)
        || !state.config.password.verify(&body.password)
    {
        return respond_json(
            writer,
            401,
            &json!({"error":"invalid_credentials","message":"用户名或密码错误"}),
            None,
        )
        .await;
    }
    let (token, csrf) = state.sessions.create()?;
    let cookie = cookie_header(
        COOKIE,
        &token,
        state.config.session_ttl,
        state.config.secure_cookies,
        &state.config.base_path,
    );
    respond_json(writer, 200, &json!({"ok":true,"csrf":csrf}), Some(cookie)).await
}

fn login_handler(state: Arc<AdminState>) -> Handler {
    Arc::new(move |req, writer: ResponseWriter<_>| {
        let state = state.clone();
        Box::pin(async move { login(state, req, writer).await })
    })
}

fn session_handler(state: Arc<AdminState>) -> Handler {
    Arc::new(move |req, writer: ResponseWriter<_>| {
        let state = state.clone();
        Box::pin(async move {
            let auth = auth(&state, &req);
            let view = session_view(&state, auth.as_ref());
            respond_json(writer, 200, &view, None).await
        })
    })
}

fn logout_handler(state: Arc<AdminState>) -> Handler {
    Arc::new(move |req, writer: ResponseWriter<_>| {
        let state = state.clone();
        Box::pin(async move {
            if let Some(auth) = auth(&state, &req) {
                state.sessions.remove(&auth.token);
            }
            let clear = clear_cookie(COOKIE, &state.config.base_path);
            respond_json(writer, 200, &json!({"ok":true}), Some(clear)).await
        })
    })
}

fn streams_handler(state: Arc<AdminState>) -> Handler {
    Arc::new(move |req, writer: ResponseWriter<_>| {
        let state = state.clone();
        Box::pin(async move {
            if auth(&state, &req).is_none() {
                return unauthorized(writer).await;
            }
            let items = streams(&state);
            let total = items.len();
            respond_json(writer, 200, &json!({"items":items,"total":total}), None).await
        })
    })
}

fn overview_handler(state: Arc<AdminState>) -> Handler {
    Arc::new(move |req, writer: ResponseWriter<_>| {
        let state = state.clone();
        Box::pin(async move {
            if auth(&state, &req).is_none() {
                return unauthorized(writer).await;
            }
            let items = streams(&state);
            let overview = Overview {
                uptime_seconds: state.started.elapsed().as_secs(),
                mounts: items.len(),
                publishers: items.iter().filter(|item| item.publisher).count(),
                subscribers: items.iter().map(|item| item.subscribers).sum(),
                accepted_frames: items.iter().map(|item| item.accepted_frames).sum(),
                dropped_frames: items.iter().map(|item| item.dropped_frames).sum(),
                rtp_packets: items.iter().map(|item| item.rtp_packets).sum(),
                rtp_octets: items.iter().map(|item| item.rtp_octets).sum(),
                vods: vods(&state).len(),
            };
            respond_json(writer, 200, &overview, None).await
        })
    })
}

fn vods_handler(state: Arc<AdminState>) -> Handler {
    Arc::new(move |req, writer: ResponseWriter<_>| {
        let state = state.clone();
        Box::pin(async move {
            if auth(&state, &req).is_none() {
                return unauthorized(writer).await;
            }
            let items = vods(&state);
            respond_json(
                writer,
                200,
                &json!({"items":items,"total":items.len()}),
                None,
            )
            .await
        })
    })
}

fn system_handler(state: Arc<AdminState>) -> Handler {
    Arc::new(move |req, writer: ResponseWriter<_>| {
        let state = state.clone();
        Box::pin(async move {
            if auth(&state, &req).is_none() {
                return unauthorized(writer).await;
            }
            let body = json!({
                "version": env!("CARGO_PKG_VERSION"),
                "title": state.config.title,
                "readonly": state.config.readonly,
                "uptime_seconds": state.started.elapsed().as_secs(),
                "features": features(),
                "mounts": state.mounts.paths().len(),
                "base_path": state.config.base_path,
                "hls_prefix": state.config.hls_prefix,
                "rtmp_base": state.config.rtmp_base,
            });
            respond_json(writer, 200, &body, None).await
        })
    })
}

fn csrf_allows(state: &AdminState, req: &HttpRequest, auth: &Auth) -> bool {
    if state.config.readonly {
        return false;
    }
    req.header("x-csrf-token")
        .is_some_and(|value| constant_time_eq(value, &auth.csrf))
}

fn kick_handler(state: Arc<AdminState>) -> Handler {
    Arc::new(move |req, writer: ResponseWriter<_>| {
        let state = state.clone();
        Box::pin(async move {
            let Some(auth) = auth(&state, &req) else {
                return unauthorized(writer).await;
            };
            if !csrf_allows(&state, &req, &auth) {
                return forbidden(
                    writer,
                    if state.config.readonly {
                        "只读模式"
                    } else {
                        "CSRF 校验失败"
                    },
                )
                .await;
            }
            let Some(path) = req.query.get("path") else {
                return respond_json(writer, 400, &json!({"error":"missing_path"}), None).await;
            };
            if state.mounts.unregister(path).is_none() {
                return respond_json(writer, 404, &json!({"error":"not_found"}), None).await;
            }
            respond_json(writer, 200, &json!({"ok":true}), None).await
        })
    })
}

#[cfg(feature = "hls")]
fn vods_refresh_handler(state: Arc<AdminState>) -> Handler {
    Arc::new(move |req, writer: ResponseWriter<_>| {
        let state = state.clone();
        Box::pin(async move {
            let Some(auth) = auth(&state, &req) else {
                return unauthorized(writer).await;
            };
            if !csrf_allows(&state, &req, &auth) {
                return forbidden(
                    writer,
                    if state.config.readonly {
                        "只读模式"
                    } else {
                        "CSRF 校验失败"
                    },
                )
                .await;
            }
            let Some(id) = req.path_params.first() else {
                return respond_json(writer, 400, &json!({"error":"missing_id"}), None).await;
            };
            let Some(vod) = state.config.vod.as_ref() else {
                return respond_json(writer, 404, &json!({"error":"vod_disabled"}), None).await;
            };
            match vod.invalidate(id) {
                Ok(true) => respond_json(writer, 200, &json!({"ok":true}), None).await,
                Ok(false) => respond_json(writer, 404, &json!({"error":"not_found"}), None).await,
                Err(error) => {
                    tracing::warn!(%error, %id, "admin VOD refresh failed");
                    respond_json(writer, 500, &json!({"error":"io"}), None).await
                }
            }
        })
    })
}

pub(super) async fn register(http: &HttpServer, state: Arc<AdminState>) {
    let base = state.config.base_path.clone();
    let index = index_handler(state.clone());
    http.add_route(Method::Get, &base, index.clone()).await;
    http.add_route(Method::Get, &format!("{base}/"), index.clone())
        .await;

    http.add_route(
        Method::Get,
        &format!("{base}/assets/app.css"),
        asset_handler(APP_CSS, "text/css; charset=utf-8", "no-cache"),
    )
    .await;
    http.add_route(
        Method::Get,
        &format!("{base}/assets/app.js"),
        asset_handler(APP_JS, "text/javascript; charset=utf-8", "no-cache"),
    )
    .await;
    http.add_route(
        Method::Get,
        &format!("{base}/assets/hls.min.js"),
        asset_handler(HLS_JS, "text/javascript; charset=utf-8", "no-cache"),
    )
    .await;

    http.add_route(
        Method::Post,
        &format!("{base}/api/login"),
        login_handler(state.clone()),
    )
    .await;
    http.add_route(
        Method::Post,
        &format!("{base}/api/logout"),
        logout_handler(state.clone()),
    )
    .await;
    http.add_route(
        Method::Get,
        &format!("{base}/api/session"),
        session_handler(state.clone()),
    )
    .await;
    http.add_route(
        Method::Get,
        &format!("{base}/api/overview"),
        overview_handler(state.clone()),
    )
    .await;
    http.add_route(
        Method::Get,
        &format!("{base}/api/streams"),
        streams_handler(state.clone()),
    )
    .await;
    http.add_route(
        Method::Delete,
        &format!("{base}/api/streams"),
        kick_handler(state.clone()),
    )
    .await;
    http.add_route(
        Method::Get,
        &format!("{base}/api/vods"),
        vods_handler(state.clone()),
    )
    .await;
    http.add_route(
        Method::Get,
        &format!("{base}/api/system"),
        system_handler(state.clone()),
    )
    .await;
    #[cfg(feature = "hls")]
    http.add_route(
        Method::Post,
        &format!("{base}/api/vods/{{id}}/refresh"),
        vods_refresh_handler(state.clone()),
    )
    .await;

    // SPA fallback. Unknown /api routes stay JSON 404 instead of returning
    // index.html (which would make client-side debugging misleading).
    let fallback_state = state.clone();
    let fallback: Handler = Arc::new(move |req, mut writer: ResponseWriter<_>| {
        let state = fallback_state.clone();
        Box::pin(async move {
            if req
                .path
                .starts_with(&format!("{}/api/", state.config.base_path))
            {
                let body = br#"{"error":"api_not_found"}"#;
                let length = body.len().to_string();
                writer
                    .write_headers(
                        404,
                        &[
                            ("Content-Type", "application/json"),
                            ("Content-Length", length.as_str()),
                            ("Connection", "close"),
                        ],
                    )
                    .await?;
                writer.write_raw(body).await?;
                return Ok(writer);
            }
            let body = INDEX_HTML
                .replace("__BASE__", &state.config.base_path)
                .replace("__TITLE__", &html_escape(&state.config.title));
            let length = body.len().to_string();
            writer
                .write_headers(
                    200,
                    &[
                        ("Content-Type", "text/html; charset=utf-8"),
                        ("Content-Length", length.as_str()),
                        ("Cache-Control", "no-store"),
                        ("Connection", "close"),
                    ],
                )
                .await?;
            writer.write_raw(body.as_bytes()).await?;
            writer.flush().await?;
            Ok(writer)
        })
    });
    http.add_route(Method::Get, &format!("{base}/*"), fallback)
        .await;
}
