//! Runtime proxy management routes.
use super::*;

pub(super) async fn register(http: &HttpServer, state: Arc<ApiState>) {
    http.add_route(
        Method::Get,
        "/admin/api/v1/proxies",
        api_json(state.clone(), |state, _req| async move {
            Ok(serde_json::json!({ "proxies": proxy_json(&state) }))
        }),
    )
    .await;
    http.add_route(
        Method::Post,
        "/admin/api/v1/proxies",
        api_write(state.clone(), |state, req| async move {
            let mount: crate::config::ProxyMount = serde_json::from_slice(&req.body)
                .map_err(|error| (400, format!("invalid body: {error}")))?;
            proxy_upsert(&state, mount, false)
        }),
    )
    .await;
    http.add_route(
        Method::Patch,
        "/admin/api/v1/proxies",
        api_write(state.clone(), |state, req| async move {
            let mount: crate::config::ProxyMount = serde_json::from_slice(&req.body)
                .map_err(|error| (400, format!("invalid body: {error}")))?;
            proxy_upsert(&state, mount, true)
        }),
    )
    .await;
    http.add_route(
        Method::Delete,
        "/admin/api/v1/proxies",
        api_write(state.clone(), |state, req| async move {
            let Some(path) = req.query.get("path").cloned() else {
                return Err((400, "missing path query parameter".to_string()));
            };
            let path = if path.starts_with('/') {
                path
            } else {
                format!("/{path}")
            };
            let existed = state
                .proxy_config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .mounts
                .iter()
                .position(|mount| mount.path == path);
            let Some(index) = existed else {
                return Err((404, "proxy not found".into()));
            };
            {
                let mut config = state
                    .proxy_config
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                config.mounts.remove(index);
                crate::proxy_control::save_overlay(&state.config.server.state_dir, &config);
            }
            state.proxies.remove(&path);
            audit(&state, AuditEvent::new("proxy.delete", "ok").path(&path));
            Ok(serde_json::json!({"deleted": path}))
        }),
    )
    .await;
    http.add_route(
        Method::Post,
        "/admin/api/v1/proxies/restart",
        api_write(state.clone(), |state, req| async move {
            let Some(path) = req.query.get("path").cloned() else {
                return Err((400, "missing path query parameter".to_string()));
            };
            let path = if path.starts_with('/') {
                path
            } else {
                format!("/{path}")
            };
            let mount = state
                .proxy_config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .mounts
                .iter()
                .find(|mount| mount.path == path)
                .cloned()
                .ok_or((404, "proxy not found".to_string()))?;
            proxy_restart(&state, &mount)
        }),
    )
    .await;
    http.add_route(
        Method::Post,
        "/admin/api/v1/proxies/probe",
        api_write(state.clone(), |state, req| async move {
            let Some(path) = req.query.get("path").cloned() else {
                return Err((400, "missing path query parameter".to_string()));
            };
            let path = if path.starts_with('/') {
                path
            } else {
                format!("/{path}")
            };
            let mount = state
                .proxy_config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .mounts
                .iter()
                .find(|mount| mount.path == path)
                .cloned()
                .ok_or((404, "proxy not found".to_string()))?;
            let redirect = state
                .proxy_config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .redirect_policy;
            let upstream = mount
                .upstream_config_with(redirect)
                .map_err(|error| (400, error))?;
            let description = state
                .proxies
                .probe(&upstream)
                .await
                .map_err(|error| (502, error.to_string()))?;
            Ok(serde_json::json!({
                "path": path,
                "session_name": description.session_name.as_ref(),
                "tracks": description.tracks.iter().map(|track| {
                    serde_json::json!({
                        "id": track.id,
                        "codec": format!("{:?}", track.codec),
                        "kind": format!("{:?}", track.kind),
                        "clock_rate": track.clock_rate,
                        "channels": track.channels,
                    })
                }).collect::<Vec<_>>(),
            }))
        }),
    )
    .await;
}

fn proxy_upsert(
    state: &ApiState,
    mount: crate::config::ProxyMount,
    replace: bool,
) -> Result<serde_json::Value, (u16, String)> {
    mount.validate().map_err(|error| (400, error))?;
    let path = mount.path.clone();
    {
        let mut config = state
            .proxy_config
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = config.mounts.iter().position(|entry| entry.path == path) {
            if !replace {
                return Err((409, "proxy already exists".into()));
            }
            config.mounts.remove(index);
        }
        config.mounts.push(mount.clone());
        crate::proxy_control::save_overlay(&state.config.server.state_dir, &config);
    }
    let upstream = mount.upstream_config().map_err(|error| (400, error))?;
    let options = rtflow::mount::MountOptions::builder()
        .event_sink(state.proxy_event_sink.clone())
        .startup_cache_config(upstream.startup_cache)
        .cancel(state.proxy_cancel.child_token())
        .build();
    state.proxies.remove(&path);
    state
        .proxies
        .ensure(
            std::sync::Arc::from(path.as_str()),
            upstream,
            options,
            mount.idle_grace,
            mount.mode.is_always(),
            mount.description_refresh.or(Some(
                state
                    .proxy_config
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .description_refresh,
            )),
        )
        .map_err(|error| (500, error.to_string()))?;
    audit(state, AuditEvent::new("proxy.upsert", "ok").path(&path));
    let status = state
        .proxies
        .status(&path)
        .map(|status| proxy_status_value(&status))
        .unwrap_or_else(|| serde_json::json!({"path": path, "state": "describing"}));
    Ok(status)
}

fn proxy_restart(
    state: &ApiState,
    mount: &crate::config::ProxyMount,
) -> Result<serde_json::Value, (u16, String)> {
    let upstream = mount.upstream_config().map_err(|error| (400, error))?;
    let options = rtflow::mount::MountOptions::builder()
        .event_sink(state.proxy_event_sink.clone())
        .startup_cache_config(upstream.startup_cache)
        .cancel(state.proxy_cancel.child_token())
        .build();
    state.proxies.remove(&mount.path);
    state
        .proxies
        .ensure(
            std::sync::Arc::from(mount.path.as_str()),
            upstream,
            options,
            mount.idle_grace,
            mount.mode.is_always(),
            mount.description_refresh.or(Some(
                state
                    .proxy_config
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .description_refresh,
            )),
        )
        .map_err(|error| (500, error.to_string()))?;
    audit(
        state,
        AuditEvent::new("proxy.restart", "ok").path(&mount.path),
    );
    Ok(state
        .proxies
        .status(&mount.path)
        .map(|status| proxy_status_value(&status))
        .unwrap_or_else(|| serde_json::json!({"path": mount.path, "state": "describing"})))
}

fn proxy_status_value(status: &rtflow::server::ProxyStatus) -> serde_json::Value {
    json!({
        "path": status.path.as_ref(),
        "state": status.state,
        "subscribers": status.subscribers,
        "bytes": status.bytes,
        "tracks": status.tracks,
        "upstream": status.upstream,
        "attempts": status.attempts,
        "retry_in_ms": status.retry_in.map(|value| value.as_millis() as u64),
        "last_error": status.last_error,
    })
}
