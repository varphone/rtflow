//! Runtime proxy management routes.
use super::*;
use std::collections::HashSet;

pub(super) async fn register(http: &HttpServer, state: Arc<ApiState>) {
    http.add_route(
        Method::Get,
        "/admin/api/v1/proxies",
        api_json(state.clone(), |state, _req| async move {
            let mut proxies = proxy_json(&state);
            let recordings = state.recording.status();
            for entry in &mut proxies {
                let path = entry
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .trim_start_matches('/');
                entry["recording"] = recordings
                    .iter()
                    .find(|recording| recording.path == path)
                    .map(|recording| {
                        serde_json::json!({
                            "active": recording.active,
                            "started_at": recording.started_at,
                            "segments": recording.segments,
                            "bytes": recording.bytes,
                            "dropped": recording.dropped,
                            "directory": recording.directory,
                            "last_error": recording.last_error,
                        })
                    })
                    .unwrap_or(serde_json::Value::Null);
            }
            let now = chrono::Local::now();
            let known: HashSet<String> = proxies
                .iter()
                .filter_map(|entry| entry.get("path").and_then(serde_json::Value::as_str))
                .map(str::to_owned)
                .collect();
            let section = state
                .proxy_config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            for mount in &section.mounts {
                if known.contains(&mount.path) {
                    continue;
                }
                let scheduled_off = mount
                    .schedule
                    .as_ref()
                    .is_some_and(|schedule| !schedule.allows(now));
                if !scheduled_off {
                    continue;
                }
                proxies.push(serde_json::json!({
                    "path": mount.path,
                    "state": "scheduled",
                    "subscribers": 0,
                    "bytes": 0,
                    "tracks": 0,
                    "upstream": upstream_display(mount),
                    "attempts": 0,
                    "retry_in_ms": null,
                    "last_error": null,
                    "schedule": mount.schedule,
                    "recording": null,
                }));
            }
            Ok(serde_json::json!({ "proxies": proxies }))
        }),
    )
    .await;
    http.add_route(
        Method::Get,
        "/admin/api/v1/proxies/config",
        api_json(state.clone(), |state, _req| async move {
            Ok(serde_json::json!({
                "mounts": state
                    .proxy_config
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .mounts
                    .iter()
                    .map(|mount| serde_json::json!({
                        "path": mount.path,
                        "revision": crate::store::proxies::revision(&state.store, &mount.path).ok().flatten(),
                        "enabled": mount.enabled,
                        "mode": mount.mode,
                        "idle_grace_ms": mount.idle_grace.map(|value| value.as_millis() as u64),
                        "description_refresh_ms": mount.description_refresh.map(|value| value.as_millis() as u64),
                        "schedule": mount.schedule,
                        "outputs": mount
                            .outputs
                            .or_else(|| crate::pull_policy::outputs(&state.paths, &mount.path)),
                        "managed": crate::pull_policy::managed(&state.paths, &mount.path),
                        "upstreams": mount.upstreams.iter().map(|upstream| serde_json::json!({
                            "url": upstream.url,
                            "transport": upstream.transport,
                            "username": upstream.username,
                            "password": if upstream.password.is_some() { "<redacted>" } else { "" },
                            "request_timeout_ms": upstream.request_timeout.as_millis() as u64,
                            "media_idle_timeout_ms": upstream.media_idle_timeout.as_millis() as u64,
                        })).collect::<Vec<_>>(),
                    }))
                    .collect::<Vec<_>>(),
            }))
        }),
    )
    .await;
    http.add_route(
        Method::Post,
        "/admin/api/v1/proxies",
        api_write(state.clone(), |state, req| async move {
            let mount: crate::config::ProxyMount = serde_json::from_slice(&req.body)
                .map_err(|error| (400, format!("invalid body: {error}")))?;
            let expected = expected_revision(&req)?;
            proxy_upsert(&state, mount, false, expected)
        }),
    )
    .await;
    http.add_route(
        Method::Patch,
        "/admin/api/v1/proxies",
        api_write(state.clone(), |state, req| async move {
            let mount: crate::config::ProxyMount = serde_json::from_slice(&req.body)
                .map_err(|error| (400, format!("invalid body: {error}")))?;
            let expected = expected_revision(&req)?;
            proxy_upsert(&state, mount, true, expected)
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
            let expected = expected_revision(&req)?;
            if let Some(expected) = expected {
                let current = crate::store::proxies::revision(&state.store, &path)
                    .map_err(|error| (500, error.to_string()))?;
                if current != Some(expected) {
                    return Err((412, "precondition failed".into()));
                }
            }
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
            crate::store::proxies::delete(&state.store, &path)
                .map_err(|error| (500, error.to_string()))?;
            state
                .proxy_config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .mounts
                .remove(index);
            state.proxies.remove(&path);
            crate::pull_policy::remove(&state.paths, &path);
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

fn upstream_display(mount: &crate::config::ProxyMount) -> String {
    let Some(upstream) = mount.upstreams.first() else {
        return String::new();
    };
    let Ok(url) = url::Url::parse(&upstream.url) else {
        return String::new();
    };
    match url.port() {
        Some(port) => format!(
            "{}://{}:{port}",
            url.scheme(),
            url.host_str().unwrap_or("?")
        ),
        None => format!("{}://{}", url.scheme(), url.host_str().unwrap_or("?")),
    }
}

fn proxy_upsert(
    state: &ApiState,
    mount: crate::config::ProxyMount,
    replace: bool,
    expected: Option<u64>,
) -> Result<serde_json::Value, (u16, String)> {
    mount.validate().map_err(|error| (400, error))?;
    let mut mount = mount;
    let path = mount.path.clone();
    let previous = state
        .proxy_config
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .mounts
        .iter()
        .find(|entry| entry.path == path)
        .cloned();
    let existed = previous.is_some();
    if existed && !replace {
        return Err((409, "proxy already exists".into()));
    }
    if let Some(previous) = &previous {
        for upstream in &mut mount.upstreams {
            if upstream.password.is_none()
                && let Some(old) = previous
                    .upstreams
                    .iter()
                    .find(|old| old.url == upstream.url)
            {
                upstream.password = old.password.clone();
            }
        }
    }
    let revision = if existed {
        let revision = crate::store::proxies::revision(&state.store, &path)
            .map_err(|error| (500, error.to_string()))?
            .ok_or((404, "proxy not found".into()))?;
        if expected.is_some_and(|expected| expected != revision) {
            return Err((412, "precondition failed".into()));
        }
        Some(revision)
    } else {
        None
    };
    // A PATCH that omits outputs keeps the source's current outputs; removing
    // management is a separate, explicit operation (not exposed yet).
    if mount.outputs.is_none() {
        mount.outputs = previous.as_ref().and_then(|mount| mount.outputs);
    }
    mount.upstream_config().map_err(|error| (400, error))?;
    if let Some(outputs) = mount.outputs {
        if outputs.record {
            if !state.config.recording.enabled {
                return Err((
                    400,
                    "recording is disabled in [recording]; cannot enable pull recording".into(),
                ));
            }
            if !state.config.recording.auto_start {
                return Err((
                    400,
                    "recording.auto_start is false; pull recording would not start".into(),
                ));
            }
        }
        crate::pull_policy::apply(&state.paths, &path, &outputs).map_err(|error| (409, error))?;
    }
    let restore_policy = || {
        if let Some(outputs) = previous.as_ref().and_then(|mount| mount.outputs) {
            let _ = crate::pull_policy::apply(&state.paths, &path, &outputs);
        } else {
            crate::pull_policy::remove(&state.paths, &path);
        }
    };
    let persisted = if let Some(revision) = revision {
        crate::store::proxies::update(&state.store, &mount, revision)
    } else {
        crate::store::proxies::insert(&state.store, &mount)
    };
    if let Err(error) = persisted {
        restore_policy();
        return Err((500, error.to_string()));
    }
    {
        let mut config = state
            .proxy_config
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = config.mounts.iter().position(|entry| entry.path == path) {
            config.mounts.remove(index);
        }
        config.mounts.push(mount.clone());
    }
    state.proxies.remove(&path);
    let scheduled_off = mount
        .schedule
        .as_ref()
        .is_some_and(|schedule| !schedule.allows(chrono::Local::now()));
    let mut state_name = if scheduled_off {
        "scheduled"
    } else {
        "disabled"
    };
    if !scheduled_off && mount.enabled {
        let section = state
            .proxy_config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        crate::proxy_control::ensure_mount(
            &section,
            &state.paths,
            &state.proxies,
            &state.proxy_event_sink,
            &state.proxy_cancel,
            &mount,
        )
        .map_err(|error| (500, error))?;
        state_name = "describing";
    }
    audit(state, AuditEvent::new("proxy.upsert", "ok").path(&path));
    if scheduled_off {
        return Ok(serde_json::json!({"path": path, "state": "scheduled"}));
    }
    let status = state
        .proxies
        .status(&path)
        .map(|status| proxy_status_value(&status))
        .unwrap_or_else(|| serde_json::json!({"path": path, "state": state_name}));
    Ok(status)
}

fn proxy_restart(
    state: &ApiState,
    mount: &crate::config::ProxyMount,
) -> Result<serde_json::Value, (u16, String)> {
    let pending = state
        .proxies
        .restart(&mount.path)
        .map_err(|error| (404, error.to_string()))?;
    audit(
        state,
        AuditEvent::new("proxy.restart", "ok").path(&mount.path),
    );
    let mut value = state
        .proxies
        .status(&mount.path)
        .map(|status| proxy_status_value(&status))
        .unwrap_or_else(|| serde_json::json!({"path": mount.path, "state": "describing"}));
    value["pending"] = serde_json::json!(pending);
    Ok(value)
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
