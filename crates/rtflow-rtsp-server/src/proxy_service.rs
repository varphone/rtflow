//! Public proxy lifecycle service.
//!
//! Owns proxy registrations: DESCRIBE resolution (with retry), sanitized mount
//! registration, subscriber watchers and status snapshots. The media loop
//! itself stays in `proxy_runtime`.
use rtflow_mount::upstream::{ProxyState, UpstreamConfig};
use rtflow_mount::{MountConfig, MountHandle, MountOptions, MountRegistry};
use rtflow_rtsp_client::{ClientOptions, RtspClient};
use rtflow_runtime::TaskSupervisor;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
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
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Describing,
    Registered,
    Backoff,
}

struct EntryState {
    phase: Phase,
    attempts: u32,
    last_error: Option<String>,
    retry_at: Option<Instant>,
}

struct Entry {
    upstream: UpstreamConfig,
    options: MountOptions,
    mount: Option<MountHandle>,
    state: EntryState,
    grace: Duration,
    always: bool,
    cancel: CancellationToken,
}

/// Proxy registration service shared by the RTSP server and applications.
pub struct ProxyService {
    mounts: Arc<MountRegistry>,
    supervisor: TaskSupervisor,
    cancel: CancellationToken,
    default_grace: Duration,
    entries: Mutex<HashMap<Arc<str>, Arc<Mutex<Entry>>>>,
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
            default_grace: no_subscriber_grace,
            entries: Mutex::new(HashMap::new()),
        })
    }

    /// Resolve the upstream description (unless already cached), sanitize it,
    /// register the proxy mount and start watching it. This is the synchronous
    /// library API: a failed DESCRIBE is returned to the caller.
    pub async fn register(
        &self,
        path: Arc<str>,
        mut upstream: UpstreamConfig,
        options: MountOptions,
    ) -> rtflow_core::error::Result<MountHandle> {
        validate_path(&path)?;
        if upstream.endpoints().is_empty() {
            return Err(rtflow_core::error::Error::Limit("proxy upstreams"));
        }
        if upstream.cached_description.is_none() {
            upstream.cached_description = Some(describe_upstream(&upstream).await?);
        }
        let status_upstream = upstream.clone();
        let config = MountConfig::Proxy {
            upstream: Box::new(upstream),
        };
        let handle = self.mounts.register_with(path.clone(), config, options)?;
        self.start_watch(&handle, self.default_grace, false);
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                path,
                Arc::new(Mutex::new(Entry {
                    upstream: status_upstream,
                    options: MountOptions::default(),
                    mount: Some(handle.clone()),
                    state: EntryState {
                        phase: Phase::Registered,
                        attempts: 0,
                        last_error: None,
                        retry_at: None,
                    },
                    grace: self.default_grace,
                    always: false,
                    cancel: CancellationToken::new(),
                })),
            );
        Ok(handle)
    }

    /// Register (or keep) a proxy that survives DESCRIBE failures: the worker
    /// retries with jittered backoff and registers the mount once an upstream
    /// becomes reachable. Returns immediately; progress is visible in
    /// [`Self::statuses`].
    pub fn ensure(
        self: &Arc<Self>,
        path: Arc<str>,
        upstream: UpstreamConfig,
        options: MountOptions,
        grace: Option<Duration>,
        always: bool,
    ) -> rtflow_core::error::Result<()> {
        validate_path(&path)?;
        if upstream.endpoints().is_empty() {
            return Err(rtflow_core::error::Error::Limit("proxy upstreams"));
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries.contains_key(&path) {
            return Ok(());
        }
        let entry = Arc::new(Mutex::new(Entry {
            upstream,
            options,
            mount: None,
            state: EntryState {
                phase: Phase::Describing,
                attempts: 0,
                last_error: None,
                retry_at: None,
            },
            grace: grace.unwrap_or(self.default_grace),
            always,
            cancel: CancellationToken::new(),
        }));
        entries.insert(path.clone(), entry.clone());
        drop(entries);
        let service = self.clone();
        self.supervisor.clone().spawn(
            rtflow_runtime::TaskSpec::new(
                format!("proxy-describe-{path}"),
                rtflow_core::error::Operation::ProxyConnect,
            ),
            async move { service.run_describe(path, entry).await },
        );
        Ok(())
    }

    async fn run_describe(
        self: Arc<Self>,
        path: Arc<str>,
        entry: Arc<Mutex<Entry>>,
    ) -> rtflow_core::error::Result<()> {
        loop {
            if self.cancel.is_cancelled() {
                return Ok(());
            }
            let (mut upstream, options, grace, always, entry_cancel) = {
                let mut entry = entry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if entry.cancel.is_cancelled() {
                    return Ok(());
                }
                entry.state.phase = Phase::Describing;
                entry.state.attempts = entry.state.attempts.saturating_add(1);
                entry.state.retry_at = None;
                (
                    entry.upstream.clone(),
                    entry.options.clone(),
                    entry.grace,
                    entry.always,
                    entry.cancel.clone(),
                )
            };
            match describe_upstream(&upstream).await {
                Ok(description) => {
                    upstream.cached_description = Some(description);
                    let config = MountConfig::Proxy {
                        upstream: Box::new(upstream),
                    };
                    match self.mounts.register_with(path.clone(), config, options) {
                        Ok(handle) => {
                            self.start_watch(&handle, grace, always);
                            let mut entry = entry
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            entry.state.phase = Phase::Registered;
                            entry.state.last_error = None;
                            entry.state.retry_at = None;
                            entry.mount = Some(handle);
                            return Ok(());
                        }
                        Err(error) => {
                            let mut entry = entry
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            entry.state.last_error = Some(error.to_string());
                        }
                    }
                }
                Err(error) => {
                    let mut entry = entry
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    entry.state.last_error = Some(error.to_string());
                }
            }
            let delay = {
                let mut entry = entry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                entry.state.phase = Phase::Backoff;
                entry.state.retry_at =
                    Some(Instant::now() + entry.upstream.next_retry(entry.state.attempts));
                entry.upstream.next_retry(entry.state.attempts)
            };
            tokio::select! {
                _ = self.cancel.cancelled() => return Ok(()),
                _ = entry_cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    fn start_watch(&self, handle: &MountHandle, grace: Duration, always: bool) {
        crate::proxy_runtime::watch_proxy(
            handle.mount().clone(),
            self.cancel.clone(),
            self.supervisor.clone(),
            grace,
            always,
        );
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
                    self.default_grace,
                    false,
                );
            }
        }
    }

    pub fn statuses(&self) -> Vec<ProxyStatus> {
        let entries: Vec<_> = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect();
        let mut statuses: Vec<ProxyStatus> = entries
            .into_iter()
            .map(|(path, entry)| status_of(&path, &entry))
            .collect();
        statuses.sort_by(|left, right| left.path.cmp(&right.path));
        statuses
    }

    pub fn status(&self, path: &str) -> Option<ProxyStatus> {
        let entry = {
            let entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entries
                .iter()
                .find(|(entry_path, _)| entry_path.as_ref() == path)
                .map(|(_, entry)| entry.clone())
        }?;
        let path: Arc<str> = Arc::from(path);
        Some(status_of(&path, &entry))
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
        let entry = entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entry.cancel.cancel();
        if let Some(mount) = &entry.mount {
            mount.mount().cancel.cancel();
        }
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
            let entry = entry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entry.cancel.cancel();
            if let Some(mount) = &entry.mount {
                mount.mount().cancel.cancel();
            }
        }
    }
}

fn validate_path(path: &str) -> rtflow_core::error::Result<()> {
    if !path.starts_with('/') {
        return Err(rtflow_core::error::Error::InvalidConfig(
            "proxy path must start with '/'".into(),
        ));
    }
    Ok(())
}

async fn describe_upstream(
    upstream: &UpstreamConfig,
) -> rtflow_core::error::Result<rtflow_media::MediaDescription> {
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
            Ok(description) => return Ok(sanitize_description(description)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or(rtflow_core::error::Error::Limit("proxy upstreams")))
}

fn status_of(path: &Arc<str>, entry: &Arc<Mutex<Entry>>) -> ProxyStatus {
    let entry = entry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (state, subscribers, bytes, tracks) = match &entry.mount {
        Some(mount) => {
            let snapshot = mount.snapshot();
            let state = match entry.upstream.state() {
                ProxyState::Idle => "idle",
                ProxyState::Connecting => "connecting",
                ProxyState::Streaming => "streaming",
                ProxyState::BackingOff { .. } => "backoff",
                ProxyState::Stopped => "stopped",
            };
            (
                state,
                snapshot.subscriber_count.max(snapshot.broadcast_receivers),
                snapshot.stats.published_bytes,
                mount.description().tracks.len(),
            )
        }
        None => ("describing", 0, 0, 0),
    };
    let state = match entry.state.phase {
        Phase::Describing if entry.mount.is_none() => "describing",
        Phase::Backoff => "backoff",
        _ => state,
    };
    ProxyStatus {
        path: path.clone(),
        state,
        subscribers,
        bytes,
        tracks,
        upstream: redacted_upstream(&entry.upstream),
        attempts: entry.state.attempts,
        retry_in: entry
            .state
            .retry_at
            .map(|retry_at| retry_at.saturating_duration_since(Instant::now())),
        last_error: entry.state.last_error.clone(),
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
