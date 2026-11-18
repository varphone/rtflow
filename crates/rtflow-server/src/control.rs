//! Admin control-plane bridge and framework event sink.
use crate::{
    audit::{AuditEvent, AuditLog},
    discovery::DiscoveryService,
    hooks::HookClient,
    keys::KeyStore,
    paths::PathStore,
    recording::RecordingSupervisor,
    testing::TestJobManager,
};
use rtflow::MountRegistry;
use std::sync::Arc;

/// Bridges the admin console to the control plane types.
pub(crate) struct ControlBridge {
    pub(crate) discovery: Arc<DiscoveryService>,
    pub(crate) paths: Arc<PathStore>,
    pub(crate) keys: Arc<KeyStore>,
    pub(crate) recording: Arc<RecordingSupervisor>,
    pub(crate) mounts: Arc<MountRegistry>,
    pub(crate) proxies: Arc<rtflow::server::ProxyService>,
    pub(crate) audit: Option<Arc<AuditLog>>,
    pub(crate) testing: Arc<TestJobManager>,
}

#[derive(serde::Deserialize)]
struct ControlPathBody {
    name: String,
    #[serde(flatten)]
    spec: crate::paths::PathSpec,
}

impl rtflow::admin::ControlPlane for ControlBridge {
    fn paths(&self) -> serde_json::Value {
        serde_json::to_value(self.discovery.list()).unwrap_or_default()
    }

    fn create_path(&self, body: serde_json::Value) -> Result<serde_json::Value, String> {
        let body: ControlPathBody =
            serde_json::from_value(body).map_err(|error| format!("invalid body: {error}"))?;
        self.paths
            .create(&body.name, body.spec)
            .map(|record| serde_json::to_value(record).unwrap_or_default())
            .map_err(|error| error.to_string())
    }

    fn update_path(
        &self,
        name: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let patch: crate::paths::PathPatch =
            serde_json::from_value(body).map_err(|error| format!("invalid body: {error}"))?;
        self.paths
            .patch(name, patch)
            .map(|record| serde_json::to_value(record).unwrap_or_default())
            .map_err(|error| error.to_string())
    }

    fn delete_path(&self, name: &str) -> Result<(), String> {
        self.paths.remove(name).map_err(|error| error.to_string())?;
        if let Some(mount) = self.mounts.unregister(&format!("/{name}")) {
            mount.mount().cancel.cancel();
        }
        Ok(())
    }

    fn kick(&self, name: &str) -> Result<(), String> {
        let Some(mount) = self.mounts.unregister(&format!("/{name}")) else {
            return Err("mount is not online".into());
        };
        mount.mount().cancel.cancel();
        Ok(())
    }

    fn keys(&self) -> serde_json::Value {
        serde_json::to_value(self.keys.list()).unwrap_or_default()
    }

    fn issue_key(&self, body: serde_json::Value) -> Result<serde_json::Value, String> {
        let spec: crate::keys::KeySpec =
            serde_json::from_value(body).map_err(|error| format!("invalid body: {error}"))?;
        self.keys
            .issue(spec)
            .map(|issued| serde_json::json!({"key": issued.status, "secret": issued.secret}))
            .map_err(|error| error.to_string())
    }

    fn revoke_key(&self, kid: &str) -> Result<(), String> {
        self.keys.revoke(kid).map_err(|error| error.to_string())
    }

    fn recordings(&self) -> serde_json::Value {
        serde_json::to_value(self.recording.status()).unwrap_or_default()
    }

    fn audit_events(&self, limit: usize) -> serde_json::Value {
        match &self.audit {
            Some(log) => serde_json::json!({ "events": log.recent(limit) }),
            None => serde_json::json!({ "events": [], "enabled": false }),
        }
    }

    fn proxies(&self) -> serde_json::Value {
        let proxies: Vec<_> = self
            .proxies
            .statuses()
            .into_iter()
            .map(|status| {
                serde_json::json!({
                    "path": status.path.as_ref(),
                    "state": status.state,
                    "subscribers": status.subscribers,
                    "bytes": status.bytes,
                    "tracks": status.tracks,
                    "upstream": status.upstream,
                    "attempts": status.attempts,
                    "last_error": status.last_error,
                })
            })
            .collect();
        serde_json::json!({ "proxies": proxies })
    }

    fn testing(&self) -> serde_json::Value {
        serde_json::to_value(self.testing.view()).unwrap_or_default()
    }

    fn start_test(&self, body: serde_json::Value) -> Result<serde_json::Value, String> {
        self.testing
            .start(body)
            .map(|job| serde_json::to_value(job).unwrap_or_default())
    }

    fn stop_test(&self, id: &str) -> Result<(), String> {
        if let Some(audit) = &self.audit {
            audit.record(
                &AuditEvent::new("testing.stop", "requested")
                    .path(id)
                    .protocol("test"),
            );
        }
        self.testing.stop(id)
    }

    fn start_recording<'a>(
        &'a self,
        path: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.recording.start(path).await })
    }

    fn stop_recording<'a>(
        &'a self,
        path: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.recording.stop(path).await })
    }
}

pub(crate) struct AppEventSink {
    pub(crate) metrics: rtflow::core::metrics::MetricsEventSink,
    pub(crate) hooks: Option<Arc<HookClient>>,
}

impl rtflow::EventSink for AppEventSink {
    fn record(&self, event: &rtflow::FrameworkEvent) {
        self.metrics.record(event);
        let Some(hooks) = &self.hooks else {
            return;
        };
        let (event, path, role, protocol, peer) = match event {
            rtflow::FrameworkEvent::Mount(rtflow::observe::MountEvent::Unregistered { id }) => (
                crate::hooks::HookKind::PublishDone,
                id.as_str().trim_start_matches('/').to_owned(),
                "publish",
                "server",
                None,
            ),
            rtflow::FrameworkEvent::Session(rtflow::observe::SessionEvent::PlaybackStopped {
                peer,
                mount,
                ..
            }) => (
                crate::hooks::HookKind::PlayDone,
                mount.as_str().trim_start_matches('/').to_owned(),
                "read",
                "rtsp",
                Some(peer.to_string()),
            ),
            _ => return,
        };
        hooks.notify(crate::hooks::HookRequest {
            event,
            path,
            role: role.into(),
            protocol: protocol.into(),
            peer,
            key_id: None,
            stream_id: None,
            reason: None,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        });
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HooksSection;
    use rtflow::{EventSink, FrameworkEvent};
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn playback_stopped_triggers_play_done_hook() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(tokio::sync::Mutex::new(String::new()));
        let received_worker = received.clone();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    match stream.read(&mut chunk).await {
                        Ok(0) => break,
                        Ok(size) => {
                            buffer.extend_from_slice(&chunk[..size]);
                            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                                let text = String::from_utf8_lossy(&buffer).to_string();
                                *received_worker.lock().await = text;
                                let _ = tokio::io::AsyncWriteExt::write_all(
                                    &mut stream,
                                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}",
                                )
                                .await;
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        });

        let hooks = HookClient::new(HooksSection {
            on_play_done: Some(format!("http://{addr}/play-done")),
            timeout: Duration::from_secs(2),
            ..HooksSection::default()
        })
        .unwrap();
        let sink = AppEventSink {
            metrics: rtflow::core::metrics::MetricsEventSink::new(Arc::new(
                rtflow::core::metrics::MetricsRegistry::default(),
            )),
            hooks: Some(hooks),
        };
        sink.record(&FrameworkEvent::Session(
            rtflow::observe::SessionEvent::PlaybackStopped {
                peer: "127.0.0.1:5000".parse().unwrap(),
                mount: rtflow::MountId::from("live/cam"),
                elapsed: Duration::from_secs(3),
            },
        ));

        let mut text = String::new();
        for _ in 0..50 {
            text = received.lock().await.clone();
            if !text.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(text.contains("/play-done"), "hook not called: {text}");
        assert!(text.contains("play_done"), "payload missing event: {text}");
        assert!(text.contains("live/cam"), "payload missing path: {text}");
    }
}
