//! ONVIF discovery endpoints (compiled with the `onvif` feature).
use super::*;
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize, Default)]
#[serde(default)]
struct DiscoverRequest {
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct InspectRequest {
    xaddr: String,
    username: Option<String>,
    password: Option<String>,
}

pub(super) async fn register(http: &HttpServer, state: Arc<ApiState>) {
    http.add_route(
        Method::Get,
        "/admin/api/v1/onvif/status",
        api_json(state.clone(), |_state, _req| async move {
            Ok(serde_json::json!({
                "enabled": true,
                "discovery": "239.255.255.250:3702",
                "default_timeout_ms": 3000,
            }))
        }),
    )
    .await;
    http.add_route(
        Method::Post,
        "/admin/api/v1/onvif/discover",
        api_json(state.clone(), |_state, req| async move {
            let body: DiscoverRequest = if req.body.is_empty() {
                DiscoverRequest::default()
            } else {
                serde_json::from_slice(&req.body)
                    .map_err(|error| (400, format!("invalid body: {error}")))?
            };
            let timeout = Duration::from_millis(body.timeout_ms.unwrap_or(3000).clamp(500, 10_000));
            let devices = rtflow::onvif::discover(timeout)
                .await
                .map_err(|error| (502, error))?;
            Ok(serde_json::json!({ "devices": devices }))
        }),
    )
    .await;
    http.add_route(
        Method::Post,
        "/admin/api/v1/onvif/inspect",
        api_json(state.clone(), |_state, req| async move {
            let body: InspectRequest = serde_json::from_slice(&req.body)
                .map_err(|error| (400, format!("invalid body: {error}")))?;
            let details = rtflow::onvif::inspect(
                &body.xaddr,
                body.username.filter(|value| !value.is_empty()),
                body.password,
            )
            .await
            .map_err(|error| (502, error))?;
            Ok(serde_json::to_value(details).unwrap_or_default())
        }),
    )
    .await;
}
