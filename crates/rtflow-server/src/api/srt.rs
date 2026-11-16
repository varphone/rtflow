//! Dynamic SRT pull/push endpoints backed by SQLite.
use super::*;
use crate::store::srt::{SrtDirection, SrtRecord};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct SrtBody {
    id: Option<String>,
    name: String,
    enabled: bool,
    remote: String,
    stream_id: Option<String>,
    mount: Option<String>,
    passphrase: Option<String>,
    pbkeylen: u32,
    latency_ms: Option<u32>,
}

impl Default for SrtBody {
    fn default() -> Self {
        Self {
            id: None,
            name: String::new(),
            enabled: true,
            remote: String::new(),
            stream_id: None,
            mount: None,
            passphrase: None,
            pbkeylen: 0,
            latency_ms: None,
        }
    }
}

pub(super) async fn register(http: &HttpServer, state: Arc<ApiState>) {
    http.add_route(
        Method::Get,
        "/admin/api/v1/srt/pulls",
        api_json(state.clone(), |state, _req| async move { Ok(state.srt.pull_json()) }),
    )
    .await;
    http.add_route(
        Method::Post,
        "/admin/api/v1/srt/pulls",
        api_write(state.clone(), |state, req| async move {
            let body: SrtBody = serde_json::from_slice(&req.body)
                .map_err(|error| (400, format!("invalid body: {error}")))?;
            srt_upsert(&state, body, SrtDirection::Pull, false).await
        }),
    )
    .await;
    http.add_route(
        Method::Patch,
        "/admin/api/v1/srt/pulls/*",
        api_write(state.clone(), |state, req| async move {
            let Some(id) = req.path.strip_prefix("/admin/api/v1/srt/pulls/") else {
                return Err((400, "missing SRT endpoint id".into()));
            };
            let mut body: SrtBody = serde_json::from_slice(&req.body)
                .map_err(|error| (400, format!("invalid body: {error}")))?;
            body.id = Some(id.to_owned());
            srt_upsert(&state, body, SrtDirection::Pull, true).await
        }),
    )
    .await;
    http.add_route(
        Method::Delete,
        "/admin/api/v1/srt/pulls/*",
        api_write(state.clone(), |state, req| async move {
            let Some(id) = req.path.strip_prefix("/admin/api/v1/srt/pulls/") else {
                return Err((400, "missing SRT endpoint id".into()));
            };
            state
                .srt
                .delete(id)
                .map_err(|error| (404, error))?;
            state.srt.reload();
            audit(&state, AuditEvent::new("srt.delete", "ok").detail(id));
            Ok(json!({"deleted": id}))
        }),
    )
    .await;

    http.add_route(
        Method::Get,
        "/admin/api/v1/srt/pushes",
        api_json(state.clone(), |state, _req| async move { Ok(state.srt.push_json()) }),
    )
    .await;
    http.add_route(
        Method::Post,
        "/admin/api/v1/srt/pushes",
        api_write(state.clone(), |state, req| async move {
            let body: SrtBody = serde_json::from_slice(&req.body)
                .map_err(|error| (400, format!("invalid body: {error}")))?;
            srt_upsert(&state, body, SrtDirection::Push, false).await
        }),
    )
    .await;
    http.add_route(
        Method::Patch,
        "/admin/api/v1/srt/pushes/*",
        api_write(state.clone(), |state, req| async move {
            let Some(id) = req.path.strip_prefix("/admin/api/v1/srt/pushes/") else {
                return Err((400, "missing SRT endpoint id".into()));
            };
            let mut body: SrtBody = serde_json::from_slice(&req.body)
                .map_err(|error| (400, format!("invalid body: {error}")))?;
            body.id = Some(id.to_owned());
            srt_upsert(&state, body, SrtDirection::Push, true).await
        }),
    )
    .await;
    http.add_route(
        Method::Delete,
        "/admin/api/v1/srt/pushes/*",
        api_write(state.clone(), |state, req| async move {
            let Some(id) = req.path.strip_prefix("/admin/api/v1/srt/pushes/") else {
                return Err((400, "missing SRT endpoint id".into()));
            };
            state
                .srt
                .delete(id)
                .map_err(|error| (404, error))?;
            state.srt.reload();
            audit(&state, AuditEvent::new("srt.delete", "ok").detail(id));
            Ok(json!({"deleted": id}))
        }),
    )
    .await;
}

async fn srt_upsert(
    state: &ApiState,
    body: SrtBody,
    direction: SrtDirection,
    replace: bool,
) -> Result<serde_json::Value, (u16, String)> {
    if body.remote.parse::<std::net::SocketAddr>().is_err() {
        return Err((400, "remote must be host:port".into()));
    }
    if direction == SrtDirection::Push
        && body
            .mount
            .as_deref()
            .is_none_or(|mount| !mount.starts_with('/'))
    {
        return Err((400, "push mount must start with '/'".into()));
    }
    if let Some(passphrase) = &body.passphrase
        && !(10..=80).contains(&passphrase.len())
    {
        return Err((400, "passphrase must be 10..80 bytes".into()));
    }
    if !matches!(body.pbkeylen, 0 | 16 | 24 | 32) {
        return Err((400, "pbkeylen must be 0, 16, 24 or 32".into()));
    }
    let id = body.id.unwrap_or_else(|| {
        let mut bytes = [0u8; 8];
        let _ = getrandom::getrandom(&mut bytes);
        format!("srt_{}", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>())
    });
    let record = SrtRecord {
        id: id.clone(),
        name: if body.name.is_empty() {
            body.stream_id.clone().unwrap_or_else(|| id.clone())
        } else {
            body.name
        },
        enabled: body.enabled,
        direction,
        remote: body.remote,
        stream_id: body.stream_id,
        mount: body.mount,
        passphrase: body.passphrase,
        pbkeylen: body.pbkeylen,
        latency_ms: body.latency_ms,
        revision: 1,
    };
    state
        .srt
        .upsert(record, replace)
        .map_err(|error| (409, error))?;
    state.srt.reload();
    audit(
        state,
        AuditEvent::new("srt.upsert", "ok").detail(format!("{direction:?}/{id}")),
    );
    Ok(json!({"id": id, "direction": format!("{direction:?}").to_lowercase()}))
}
