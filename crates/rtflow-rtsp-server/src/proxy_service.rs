//! Public proxy lifecycle service.
//!
//! Owns proxy registrations for an RTSP server (or a standalone server app):
//! DESCRIBE resolution, sanitized mount registration, subscriber watchers and
//! status snapshots. The media loop itself stays in `proxy_runtime`.
use rtflow_mount::upstream::{ProxyState, UpstreamConfig};
use rtflow_mount::{MountConfig, MountHandle, MountOptions, MountRegistry};
use rtflow_rtsp_client::{ClientOptions, RtspClient};
use rtflow_runtime::TaskSupervisor;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// Status of one configured proxy mount.
#[derive(Clone, Debug)]
pub struct ProxyStatus {
    pub path: Arc<str>,
    pub state: &'static str,
    pub subscribers: usize,
    pub bytes: u64,
    pub tracks: usize,
    pub upstream: String,
    pub attempts: u32,
    pub retry_in: Option<Duration>,
}

struct Registered {
    upstream: UpstreamConfig,
    mount: MountHandle,
}

/// Proxy registration service shared by the RTSP server and applications.
pub struct ProxyService {
    mounts: Arc<MountRegistry>,
    supervisor: TaskSupervisor,
    cancel: CancellationToken,
    no_subscriber_grace: Duration,
    entries: Mutex<HashMap<Arc<str>, Registered>>,
}

impl ProxyService {
    pub fn new(
        mounts: Arc<MountRegistry>,
        supervisor: TaskSupervisor,
        cancel: CancellationToken,
        no_subscriber_grace: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            mounts,
            supervisor,
            cancel,
            no_subscriber_grace,
            entries: Mutex::new(HashMap::new()),
        })
    }

    /// Resolve the upstream description (unless already cached), sanitize it
    /// for downstream clients, register the proxy mount and start watching it.
    pub async fn register(
        &self,
        path: Arc<str>,
        mut upstream: UpstreamConfig,
        options: MountOptions,
    ) -> rtflow_core::error::Result<MountHandle> {
        if !path.starts_with('/') {
            return Err(rtflow_core::error::Error::InvalidConfig(
                "proxy path must start with '/'".into(),
            ));
        }
        if upstream.endpoints().is_empty() {
            return Err(rtflow_core::error::Error::Limit("proxy upstreams"));
        }
        if upstream.cached_description.is_none() {
            let mut last_error = None;
            for endpoint in upstream.endpoints() {
                let mut builder = ClientOptions::builder()
                    .transport(endpoint.transport)
                    .request_timeout(endpoint.request_timeout)
                    .jitter(endpoint.jitter);
                if let Some(credentials) = endpoint.credentials {
                    builder = builder.credentials(credentials);
                }
                #[cfg(feature = "tls")]
                if let Some(tls) = endpoint.tls {
                    builder = builder.tls(tls);
                }
                let Ok(options) = builder.build() else {
                    continue;
                };
                match RtspClient::describe(endpoint.url, options).await {
                    Ok(description) => {
                        upstream.cached_description = Some(sanitize_description(description));
                        break;
                    }
                    Err(error) => last_error = Some(error),
                }
            }
            if upstream.cached_description.is_none() {
                return Err(
                    last_error.unwrap_or(rtflow_core::error::Error::Limit("proxy upstreams"))
                );
            }
        }

        let status_upstream = upstream.clone();
        let config = MountConfig::Proxy {
            upstream: Box::new(upstream),
        };
        let handle = self.mounts.register_with(path.clone(), config, options)?;
        crate::proxy_runtime::watch_proxy(
            handle.mount().clone(),
            self.cancel.clone(),
            self.supervisor.clone(),
            self.no_subscriber_grace,
        );
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                path,
                Registered {
                    upstream: status_upstream,
                    mount: handle.clone(),
                },
            );
        Ok(handle)
    }

    /// Start watchers for proxy mounts that already exist in the registry
    /// (gateway-style registration before the RTSP server binds).
    pub fn watch_existing(&self) {
        for path in self.mounts.paths() {
            let Some(handle) = self.mounts.get_handle(path.as_str()) else {
                continue;
            };
            let mount = handle.mount();
            if matches!(mount.config, MountConfig::Proxy { .. }) {
                crate::proxy_runtime::watch_proxy(
                    mount.clone(),
                    self.cancel.clone(),
                    self.supervisor.clone(),
                    self.no_subscriber_grace,
                );
            }
        }
    }

    pub fn statuses(&self) -> Vec<ProxyStatus> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut statuses: Vec<ProxyStatus> = entries
            .iter()
            .map(|(path, entry)| status_of(path, entry))
            .collect();
        statuses.sort_by(|left, right| left.path.cmp(&right.path));
        statuses
    }

    pub fn status(&self, path: &str) -> Option<ProxyStatus> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries
            .iter()
            .find(|(entry_path, _)| entry_path.as_ref() == path)
            .map(|(path, entry)| status_of(path, entry))
    }

    /// Remove a proxy: cancel its worker, unregister the mount and forget it.
    pub fn remove(&self, path: &str) -> bool {
        let entry = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(path);
        let Some(entry) = entry else {
            return false;
        };
        entry.mount.mount().cancel.cancel();
        self.mounts.unregister(path);
        true
    }

    pub fn shutdown(&self) {
        let entries: Vec<_> = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        for entry in entries {
            entry.mount.mount().cancel.cancel();
        }
    }
}

fn status_of(path: &Arc<str>, entry: &Registered) -> ProxyStatus {
    let snapshot = entry.mount.snapshot();
    let (state, attempts, retry_in) = match entry.upstream.state() {
        ProxyState::Idle => ("idle", 0, None),
        ProxyState::Connecting => ("connecting", 0, None),
        ProxyState::Streaming => ("streaming", 0, None),
        ProxyState::BackingOff { retry_at, attempts } => (
            "backoff",
            attempts,
            Some(retry_at.saturating_duration_since(std::time::Instant::now())),
        ),
        ProxyState::Stopped => ("stopped", 0, None),
    };
    ProxyStatus {
        path: path.clone(),
        state,
        subscribers: snapshot.subscriber_count.max(snapshot.broadcast_receivers),
        bytes: snapshot.stats.rtp_octets,
        tracks: entry.mount.description().tracks.len(),
        upstream: redacted_upstream(&entry.upstream),
        attempts,
        retry_in,
    }
}

fn redacted_upstream(upstream: &UpstreamConfig) -> String {
    let url = &upstream.url;
    match url.port_or_known_default() {
        Some(port) => format!(
            "{}://{}:{port}",
            url.scheme(),
            url.host_str().unwrap_or("?")
        ),
        None => format!("{}://{}", url.scheme(), url.host_str().unwrap_or("?")),
    }
}

/// Rewrite upstream-absolute control URLs and strip upstream RTCP hints so a
/// downstream client SETUPs against this server, not against the camera.
fn sanitize_description(
    mut description: rtflow_media::MediaDescription,
) -> rtflow_media::MediaDescription {
    for track in &mut description.tracks {
        track.rtcp = None;
        if let Ok(url) = url::Url::parse(&track.control) {
            track.control = Arc::from(url.path().trim_start_matches('/').to_string());
        }
    }
    description
}
