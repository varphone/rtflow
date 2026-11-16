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

/// Network policy applied to upstream endpoints.
#[derive(Clone, Debug)]
pub struct ProxyPolicy {
    pub allow_private_networks: bool,
    pub allowed_hosts: Vec<String>,
    pub max_upstreams: usize,
    pub describe_concurrency: usize,
}

impl Default for ProxyPolicy {
    fn default() -> Self {
        Self {
            allow_private_networks: true,
            allowed_hosts: Vec::new(),
            max_upstreams: 4,
            describe_concurrency: 8,
        }
    }
}

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
    refresh: Option<Duration>,
    cancel: CancellationToken,
}

/// Proxy registration service shared by the RTSP server and applications.
pub struct ProxyService {
    mounts: Arc<MountRegistry>,
    supervisor: TaskSupervisor,
    cancel: CancellationToken,
    default_grace: Duration,
    policy: ProxyPolicy,
    describe_slots: Arc<tokio::sync::Semaphore>,
    entries: Mutex<HashMap<Arc<str>, Arc<Mutex<Entry>>>>,
}

impl ProxyService {
    pub fn new(
        mounts: Arc<MountRegistry>,
        supervisor: TaskSupervisor,
        cancel: CancellationToken,
        no_subscriber_grace: Duration,
    ) -> Arc<Self> {
        Self::new_with_policy(
            mounts,
            supervisor,
            cancel,
            no_subscriber_grace,
            ProxyPolicy::default(),
        )
    }

    pub fn new_with_policy(
        mounts: Arc<MountRegistry>,
        supervisor: TaskSupervisor,
        cancel: CancellationToken,
        no_subscriber_grace: Duration,
        policy: ProxyPolicy,
    ) -> Arc<Self> {
        Arc::new(Self {
            mounts,
            supervisor,
            cancel,
            default_grace: no_subscriber_grace,
            describe_slots: Arc::new(tokio::sync::Semaphore::new(
                policy.describe_concurrency.max(1),
            )),
            policy,
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
        if upstream.endpoints().len() > self.policy.max_upstreams {
            return Err(rtflow_core::error::Error::Limit("proxy upstream count"));
        }
        prepare_upstream(&mut upstream, &self.policy).await?;
        if upstream.cached_description.is_none() {
            upstream.cached_description = Some(describe_prepared(&upstream).await?);
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
                    refresh: None,
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
        refresh: Option<Duration>,
    ) -> rtflow_core::error::Result<()> {
        validate_path(&path)?;
        if upstream.endpoints().is_empty() {
            return Err(rtflow_core::error::Error::Limit("proxy upstreams"));
        }
        if upstream.endpoints().len() > self.policy.max_upstreams {
            return Err(rtflow_core::error::Error::Limit("proxy upstream count"));
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
            refresh,
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
            let (mut upstream, options, grace, always, refresh, entry_cancel) = {
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
                    entry.refresh,
                    entry.cancel.clone(),
                )
            };
            let slot = self
                .describe_slots
                .acquire()
                .await
                .map_err(|_| rtflow_core::error::Error::Limit("proxy describe slots"))?;
            let described = match prepare_upstream(&mut upstream, &self.policy).await {
                Ok(()) => describe_prepared(&upstream).await,
                Err(error) => Err(error),
            };
            drop(slot);
            match described {
                Ok(description) => {
                    upstream.cached_description = Some(description);
                    let runtime_upstream = upstream.clone();
                    let config = MountConfig::Proxy {
                        upstream: Box::new(upstream),
                    };
                    match self.mounts.register_with(path.clone(), config, options) {
                        Ok(handle) => {
                            self.start_watch(&handle, grace, always);
                            {
                                let mut guard = entry
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                guard.upstream = runtime_upstream;
                                guard.upstream.cached_description = Some(handle.description());
                                guard.state.phase = Phase::Registered;
                                guard.state.last_error = None;
                                guard.state.retry_at = None;
                                guard.mount = Some(handle.clone());
                            }
                            if let Some(interval) = refresh {
                                self.start_refresh(path.clone(), entry.clone(), interval);
                            }
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

    fn start_refresh(&self, path: Arc<str>, entry: Arc<Mutex<Entry>>, interval: Duration) {
        let policy = self.policy.clone();
        self.supervisor.clone().spawn(
            rtflow_runtime::TaskSpec::new(
                format!("proxy-refresh-{path}"),
                rtflow_core::error::Operation::ProxyConnect,
            ),
            async move {
                loop {
                    let cancel = entry
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .cancel
                        .clone();
                    tokio::select! {
                        _ = cancel.cancelled() => return Ok(()),
                        _ = tokio::time::sleep(interval) => {}
                    }
                    let upstream = entry
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .upstream
                        .clone();
                    let Ok(description) = describe_upstream(&upstream, &policy).await else {
                        continue;
                    };
                    let (Some(mount), subscribers) = ({
                        let entry = entry
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let subscribers = entry
                            .mount
                            .as_ref()
                            .map(|mount| {
                                let snapshot = mount.snapshot();
                                snapshot.subscriber_count.max(snapshot.broadcast_receivers)
                            })
                            .unwrap_or(0);
                        (entry.mount.clone(), subscribers)
                    }) else {
                        continue;
                    };
                    if subscribers == 0
                        && mount.description() != description
                        && mount.update_description(description.clone()).is_ok()
                    {
                        entry
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .upstream
                            .cached_description = Some(description);
                        tracing::info!(path = %path, "proxy description refreshed");
                    }
                }
            },
        );
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

    /// Probe an upstream without registering or disturbing a running mount.
    pub async fn probe(
        &self,
        upstream: &UpstreamConfig,
    ) -> rtflow_core::error::Result<rtflow_media::MediaDescription> {
        describe_upstream(upstream, &self.policy).await
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

async fn describe_upstream(
    upstream: &UpstreamConfig,
    policy: &ProxyPolicy,
) -> rtflow_core::error::Result<rtflow_media::MediaDescription> {
    let mut prepared = upstream.clone();
    prepare_upstream(&mut prepared, policy).await?;
    describe_prepared(&prepared).await
}

async fn describe_prepared(
    upstream: &UpstreamConfig,
) -> rtflow_core::error::Result<rtflow_media::MediaDescription> {
    let mut last_error = None;
    for endpoint in upstream.endpoints() {
        let mut builder = ClientOptions::builder()
            .transport(endpoint.transport)
            .request_timeout(endpoint.request_timeout)
            .follow_redirects(endpoint.follow_redirects)
            .allow_cross_origin_redirect(endpoint.allow_cross_origin_redirect)
            .jitter(endpoint.jitter);
        if let Some(address) = endpoint.resolved {
            builder = builder.peer_addr(address);
        }
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

async fn prepare_upstream(
    upstream: &mut UpstreamConfig,
    policy: &ProxyPolicy,
) -> rtflow_core::error::Result<()> {
    let endpoints = upstream.endpoints();
    if endpoints.len() > policy.max_upstreams {
        return Err(rtflow_core::error::Error::Limit("proxy upstream count"));
    }
    let mut resolved = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        let address = resolve_endpoint(&endpoint, policy).await?;
        resolved.push(address);
    }
    upstream.resolved = resolved.first().copied().flatten();
    let mut fallbacks: Vec<_> = upstream.endpoints().into_iter().skip(1).collect();
    for (endpoint, address) in fallbacks.iter_mut().zip(resolved.iter().skip(1)) {
        endpoint.resolved = *address;
    }
    upstream.fallbacks = fallbacks;
    Ok(())
}

async fn resolve_endpoint(
    endpoint: &rtflow_mount::upstream::UpstreamEndpoint,
    policy: &ProxyPolicy,
) -> rtflow_core::error::Result<Option<std::net::SocketAddr>> {
    let host = endpoint
        .url
        .host_str()
        .ok_or_else(|| rtflow_core::error::Error::InvalidConfig("proxy URL has no host".into()))?;
    let port = endpoint.url.port_or_known_default().unwrap_or(554);
    if !policy.allowed_hosts.is_empty() && !host_matches(host, port, &policy.allowed_hosts) {
        return Err(rtflow_core::error::Error::InvalidConfig(format!(
            "proxy host {host} is not in proxy.allowed_hosts"
        )));
    }
    let addresses: Vec<_> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| {
            rtflow_core::error::Error::InvalidConfig(format!("proxy DNS lookup failed: {error}"))
        })?
        .collect();
    if addresses.is_empty() {
        return Err(rtflow_core::error::Error::InvalidConfig(format!(
            "proxy host {host} did not resolve"
        )));
    }
    if !policy.allow_private_networks {
        for address in &addresses {
            if is_non_public(address.ip()) {
                return Err(rtflow_core::error::Error::InvalidConfig(format!(
                    "proxy host {host} resolves to non-public address {}",
                    address.ip()
                )));
            }
        }
    }
    Ok(addresses.first().copied())
}

fn host_matches(host: &str, port: u16, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim();
        if let Ok(cidr) = pattern.parse::<rtflow_security::IpCidr>()
            && let Ok(address) = host.parse::<std::net::IpAddr>()
        {
            return cidr.contains(address);
        }
        if let Some(suffix) = pattern.strip_prefix("*.") {
            return host.len() > suffix.len()
                && host.ends_with(suffix)
                && host.as_bytes()[host.len() - suffix.len() - 1] == b'.';
        }
        pattern == host || pattern == format!("{host}:{port}")
    })
}

fn is_non_public(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(address) => {
            address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_unspecified()
                || address.is_broadcast()
        }
        std::net::IpAddr::V6(address) => {
            address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_unique_local()
                || address.is_unicast_link_local()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_allow_list_matches_hosts_wildcards_and_cidrs() {
        let patterns = vec![
            "camera.local".to_string(),
            "*.example.com".to_string(),
            "10.0.0.0/8".to_string(),
            "rtsp.example.net:8554".to_string(),
        ];
        assert!(host_matches("camera.local", 554, &patterns));
        assert!(host_matches("cam.example.com", 554, &patterns));
        assert!(host_matches("10.2.3.4", 554, &patterns));
        assert!(host_matches("rtsp.example.net", 8554, &patterns));
        assert!(!host_matches("example.com", 554, &patterns));
        assert!(!host_matches("192.168.1.1", 554, &patterns));
    }

    #[tokio::test]
    async fn dns_policy_pins_public_and_rejects_private() {
        let mut upstream = UpstreamConfig::new(
            url::Url::parse("rtsp://127.0.0.1/live").unwrap(),
            rtflow_mount::upstream::UpstreamTransport::TcpInterleaved,
        );
        let strict = ProxyPolicy {
            allow_private_networks: false,
            ..ProxyPolicy::default()
        };
        assert!(prepare_upstream(&mut upstream, &strict).await.is_err());
        let permissive = ProxyPolicy::default();
        prepare_upstream(&mut upstream, &permissive).await.unwrap();
        assert_eq!(upstream.resolved, Some("127.0.0.1:554".parse().unwrap()));
    }

    #[test]
    fn private_and_loopback_addresses_are_non_public() {
        assert!(is_non_public("127.0.0.1".parse().unwrap()));
        assert!(is_non_public("192.168.1.10".parse().unwrap()));
        assert!(is_non_public("169.254.1.1".parse().unwrap()));
        assert!(!is_non_public("8.8.8.8".parse().unwrap()));
    }
}
