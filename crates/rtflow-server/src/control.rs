//! Admin control-plane bridge and framework event sink.
use crate::{
    discovery::DiscoveryService, hooks::HookClient, keys::KeyStore, paths::PathStore,
    recording::RecordingSupervisor,
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
        if let rtflow::FrameworkEvent::Mount(rtflow::observe::MountEvent::Unregistered { id }) =
            event
            && let Some(hooks) = &self.hooks
        {
            hooks.notify(crate::hooks::HookRequest {
                event: crate::hooks::HookKind::PublishDone,
                path: id.as_str().trim_start_matches('/').to_owned(),
                role: "publish".into(),
                protocol: "server".into(),
                peer: None,
                key_id: None,
                stream_id: None,
                reason: None,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            });
        }
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
