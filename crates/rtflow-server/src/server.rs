//! Server application lifecycle and protocol wiring.
use crate::{
    audit::AuditLog,
    auth::AppAuth,
    config::Config,
    discovery::{Discovery, DiscoveryService, PathStatus, public_rtmp},
    hooks::HookClient,
    keys::{IssuedKey, KeySpec, KeyStatus, KeyStore},
    paths::{PathRecord, PathSpec, PathStore},
    recording::{RecordingStatus, RecordingSupervisor},
    vod::VodCatalog,
};
use rtflow::{
    MountRegistry,
    admin::{Admin, AdminConfig},
    hls::{
        HlsConfig,
        live::{HlsLiveManager, create_hls_live_handler},
        ll::{LlHlsConfig, LlHlsLiveManager, create_ll_hls_handler},
        vod::{HlsVodConfig, HlsVodManager, create_hls_vod_handler},
    },
    http::{HttpConfig, HttpServer, Method},
    http_flv::ws::{WsFlvConfig, WsFlvServer},
    metrics::MetricsRegistry,
    rtmp::{RtmpConfig, RtmpServer, session::RtmpSessionConfig},
    srt::{SrtConfig, SrtServer},
    webrtc::{WebRtcConfig, create_webrtc_handlers},
};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// SRT publish authorization adapter over the shared [`AppAuth`].
struct SrtAuth {
    auth: Arc<AppAuth>,
}

impl std::fmt::Debug for SrtAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SrtAuth")
    }
}

impl rtflow::srt::PublishAuthenticator for SrtAuth {
    fn authorize(&self, stream_id: &str, peer: std::net::SocketAddr) -> bool {
        self.auth.verify_srt(stream_id, peer)
    }
}

pub struct ServerApp;

pub struct ServerHandle {
    pub rtsp_addr: Option<SocketAddr>,
    pub rtmp_addr: Option<SocketAddr>,
    pub srt_addr: Option<SocketAddr>,
    pub http_addr: SocketAddr,
    pub ws_flv_addr: Option<SocketAddr>,
    cancel: CancellationToken,
    cancels: Vec<CancellationToken>,
    tasks: Vec<JoinHandle<()>>,
    rtsp: Option<rtflow::server::RtspServer>,
    recording: Arc<RecordingSupervisor>,
    paths: Arc<PathStore>,
    keys: Arc<KeyStore>,
    discovery: Arc<DiscoveryService>,
    vod: Arc<VodCatalog>,
    proxies: Arc<rtflow::server::ProxyService>,
    config: Arc<Config>,
}

/// What a SIGHUP reload applied and what still needs a restart.
#[derive(Debug, Default, serde::Serialize)]
pub struct ReloadReport {
    pub applied: Vec<&'static str>,
    pub restart_required: Vec<&'static str>,
}

impl ServerHandle {
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        for cancel in &self.cancels {
            cancel.cancel();
        }
        self.recording.shutdown().await;
        self.proxies.shutdown();
        if let Some(rtsp) = self.rtsp.take()
            && let Err(error) = rtsp.shutdown().await
        {
            tracing::warn!(%error, "RTSP shutdown failed");
        }
        for task in self.tasks.drain(..) {
            task.abort();
        }
        // Give tasks a chance to observe cancellation before the runtime
        // drops them; recording and RTSP already drained above.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    /// Reload the mutable parts of the configuration (SIGHUP).
    ///
    /// Applies path rules, recording policy, discovery/publication metadata and
    /// the effective log level (the caller re-installs the filter). Listener
    /// addresses, enabled protocols and the auth mode are reported as
    /// `restart_required` instead of being half-applied.
    pub fn reload(&self, config: Config) -> Result<ReloadReport, String> {
        config.validate().map_err(|error| error.to_string())?;
        let mut report = ReloadReport::default();
        self.paths.set_rules(config.paths.clone());
        report.applied.push("paths");
        self.recording.set_policy(config.recording.clone());
        report.applied.push("recording");
        self.discovery.reload(Arc::new(config.clone()));
        report.applied.push("discovery");

        let old = &self.config;
        if old.rtsp.listen != config.rtsp.listen || old.rtsp.enabled != config.rtsp.enabled {
            report.restart_required.push("rtsp listen/enabled");
        }
        if old.rtmp.listen != config.rtmp.listen || old.rtmp.enabled != config.rtmp.enabled {
            report.restart_required.push("rtmp listen/enabled");
        }
        if old.srt.listen != config.srt.listen || old.srt.enabled != config.srt.enabled {
            report.restart_required.push("srt listen/enabled");
        }
        if old.http.listen != config.http.listen {
            report.restart_required.push("http listen");
        }
        if old.flv.ws_listen != config.flv.ws_listen {
            report.restart_required.push("flv ws_listen");
        }
        if old.auth.mode != config.auth.mode
            || old.auth.users.len() != config.auth.users.len()
            || old.auth.token_secrets != config.auth.token_secrets
        {
            report.restart_required.push("auth credentials");
        }
        if old.hooks.on_publish_auth != config.hooks.on_publish_auth
            || old.hooks.on_play_auth != config.hooks.on_play_auth
        {
            report.restart_required.push("auth hooks");
        }
        if old.http.admin_password != config.http.admin_password
            || old.http.admin_username != config.http.admin_username
        {
            report.restart_required.push("admin credentials");
        }
        Ok(report)
    }

    pub async fn wait_for_signal(self, config_path: Option<PathBuf>) {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
            let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    _ = term.recv() => break,
                    _ = hup.recv() => {
                        let Some(path) = config_path.as_deref() else {
                            tracing::warn!("SIGHUP ignored: no config file");
                            continue;
                        };
                        match Config::load(path) {
                            Ok(config) => match self.reload(config) {
                                Ok(report) => {
                                    tracing::info!(
                                        applied = ?report.applied,
                                        restart_required = ?report.restart_required,
                                        "configuration reloaded"
                                    );
                                }
                                Err(error) => tracing::warn!(%error, "configuration reload failed"),
                            },
                            Err(error) => {
                                tracing::warn!(%error, path = %path.display(), "configuration reload failed");
                            }
                        }
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = config_path;
            let _ = tokio::signal::ctrl_c().await;
        }
        self.shutdown().await;
    }

    pub fn discovery(&self) -> Discovery {
        self.discovery.discovery()
    }

    pub fn paths(&self) -> Vec<PathStatus> {
        self.discovery.list()
    }

    pub fn path(&self, name: &str) -> Option<PathStatus> {
        self.discovery.status(name)
    }

    pub fn create_path(&self, name: &str, spec: PathSpec) -> Result<PathRecord, String> {
        self.paths
            .create(name, spec)
            .map_err(|error| error.to_string())
    }

    pub fn update_path(&self, name: &str, spec: PathSpec) -> Result<PathRecord, String> {
        self.paths
            .update(name, spec)
            .map_err(|error| error.to_string())
    }

    pub fn delete_path(&self, name: &str) -> Result<PathRecord, String> {
        self.paths.remove(name).map_err(|error| error.to_string())
    }

    pub fn keys(&self) -> Vec<KeyStatus> {
        self.keys.list()
    }

    pub fn issue_key(&self, spec: KeySpec) -> Result<IssuedKey, String> {
        self.keys.issue(spec).map_err(|error| error.to_string())
    }

    pub fn revoke_key(&self, kid: &str) -> Result<(), String> {
        self.keys.revoke(kid).map_err(|error| error.to_string())
    }

    pub fn recordings(&self) -> Vec<RecordingStatus> {
        self.recording.status()
    }

    pub async fn start_recording(&self, path: &str) -> Result<(), String> {
        self.recording.start(path).await
    }

    pub async fn stop_recording(&self, path: &str) -> Result<(), String> {
        self.recording.stop(path).await
    }

    pub fn vods(&self) -> Vec<crate::vod::VodStatus> {
        self.vod.list()
    }

    pub fn refresh_vods(&self) -> usize {
        self.vod.refresh()
    }
}

impl ServerApp {
    /// Start every enabled listener. The returned handle owns the tasks and
    /// shuts them down in dependency order.
    pub async fn start(
        config: Config,
    ) -> Result<ServerHandle, Box<dyn std::error::Error + Send + Sync>> {
        config.validate()?;
        let config = Arc::new(config);
        let metrics = Arc::new(MetricsRegistry::default());
        let mut mounts = MountRegistry::new(4096);
        mounts.attach_global(metrics.clone());
        let mounts = Arc::new(mounts);
        let paths = Arc::new(PathStore::new(&config, false)?);
        let keys = Arc::new(KeyStore::new(&config)?);
        let hooks = if config.hooks.on_publish_auth.is_some()
            || config.hooks.on_play_auth.is_some()
            || config.hooks.on_publish_done.is_some()
            || config.hooks.on_play_done.is_some()
            || config.hooks.on_record_done.is_some()
        {
            Some(HookClient::new(config.hooks.clone())?)
        } else {
            None
        };
        let audit = config.audit.enabled.then(|| {
            Arc::new(AuditLog::new(
                config
                    .audit
                    .path
                    .clone()
                    .unwrap_or_else(|| config.server.state_dir.join("audit.log")),
                config.audit.max_bytes,
                config.audit.max_files,
            ))
        });
        let counters = Arc::new(crate::metrics::ServerCounters::default());
        let auth = Arc::new(
            AppAuth::new(&config, keys.clone(), paths.clone(), mounts.clone())?
                .with_hooks(hooks.clone())
                .with_audit(audit.clone())
                .with_counters(counters.clone()),
        );
        let recording =
            RecordingSupervisor::new(config.clone(), paths.clone(), mounts.clone(), hooks.clone());

        let event_sink: Arc<dyn rtflow::EventSink> = Arc::new(crate::control::AppEventSink {
            metrics: rtflow::core::metrics::MetricsEventSink::new(metrics.clone()),
            hooks: hooks.clone(),
        });

        let cancel = CancellationToken::new();
        let proxy_supervisor = rtflow::runtime::TaskSupervisor::new(event_sink.clone());
        let proxies = rtflow::server::ProxyService::new(
            mounts.clone(),
            proxy_supervisor,
            cancel.clone(),
            config.proxy.idle_grace,
        );
        let mut cancels = Vec::new();
        let mut tasks = Vec::new();

        // SRT pull (caller) sources run for the lifetime of the server.
        let srt_pull_status = crate::srt_pull::status_table(&config);
        tasks.extend(crate::srt_pull::spawn(
            config.clone(),
            mounts.clone(),
            cancel.clone(),
            srt_pull_status.clone(),
        ));
        let srt_push_status = crate::srt_push::status_table(&config);
        tasks.extend(crate::srt_push::spawn(
            config.clone(),
            mounts.clone(),
            cancel.clone(),
            srt_push_status.clone(),
        ));

        let rtsp = if config.rtsp.enabled {
            let mut rtsp_config = rtflow::server::ServerConfig::new(config.rtsp.listen);
            rtsp_config.session_timeout = config.rtsp.session_timeout;
            rtsp_config.max_sessions = config.rtsp.max_sessions;
            rtsp_config.dynamic_publishers = config.rtsp.dynamic_publishers;
            rtsp_config.authenticator = auth.clone();
            if let Some(tls) = &config.rtsp.tls {
                let certificate = std::fs::read(&tls.certificate)?;
                let private_key = std::fs::read(&tls.private_key)?;
                rtsp_config.tls = Some(rtflow::security::TlsServerConfig::from_pem(
                    &certificate,
                    &private_key,
                )?);
                tracing::info!(
                    cert = %tls.certificate.display(),
                    "RTSP TLS enabled"
                );
            }
            rtsp_config.event_sink = event_sink.clone();
            let server = rtflow::server::RtspServer::bind_with_proxy_service(
                rtsp_config,
                mounts.clone(),
                proxies.clone(),
            )
            .await?;
            Some(server)
        } else {
            None
        };

        let (rtmp_addr, rtmp_cancel) = if config.rtmp.enabled {
            let session = RtmpSessionConfig {
                authenticator: Some(auth.clone()),
                async_authenticator: Some(auth.clone()),
                ..RtmpSessionConfig::default()
            };
            #[allow(clippy::needless_update)] // `metrics` only exists with the feature
            let rtmp_config = RtmpConfig {
                bind_addr: config.rtmp.listen,
                session,
                max_connections: config.rtmp.max_connections,
                metrics: Some(metrics.clone()),
                ..RtmpConfig::default()
            };
            let server = RtmpServer::with_config(rtmp_config, mounts.clone()).await?;
            let addr = server.local_addr()?;
            let token = server.cancellation_token();
            let task = tokio::spawn(async move {
                if let Err(error) = server.run().await {
                    tracing::error!(%error, "RTMP server stopped");
                }
            });
            tasks.push(task);
            cancels.push(token.clone());
            (Some(addr), Some(token))
        } else {
            (None, None)
        };

        let mut srt_listener_stats: Option<
            Arc<std::sync::RwLock<Vec<rtflow::srt::SrtConnectionStats>>>,
        > = None;
        let (srt_addr, srt_cancel) = if config.srt.enabled {
            let srt_config = SrtConfig {
                bind_addr: config.srt.listen,
                latency_ms: config.srt.latency_ms,
                peer_idle_timeout: config.srt.peer_idle_timeout,
                tlpktdrop: config.srt.tlpktdrop,
                mtu: config.srt.mtu,
                max_connections: config.srt.max_connections,
                passphrase: config.srt.passphrase.clone(),
                pbkeylen: config.srt.pbkeylen,
                enforced_encryption: config.srt.enforced_encryption,
                allowed_stream_ids: config.srt.allowed_stream_ids.clone(),
                authenticator: Some(Arc::new(SrtAuth { auth: auth.clone() })),
                metrics: Some(metrics.clone()),
                ..SrtConfig::default()
            };
            let server = SrtServer::with_config(srt_config, mounts.clone()).await?;
            srt_listener_stats = Some(server.stats_handle());
            let addr = server.local_addr()?;
            let token = server.cancellation_token();
            let task = tokio::spawn(async move {
                if let Err(error) = server.run().await {
                    tracing::error!(%error, "SRT server stopped");
                }
            });
            tasks.push(task);
            cancels.push(token.clone());
            (Some(addr), Some(token))
        } else {
            (None, None)
        };

        let hls = Arc::new(HlsLiveManager::new(
            mounts.clone(),
            HlsConfig {
                segment_duration: config.hls.segment_duration,
                segment_count: config.hls.window,
                live: true,
            },
        ));
        let ll_hls = Arc::new(LlHlsLiveManager::new(
            mounts.clone(),
            LlHlsConfig {
                segment_duration: config.hls.segment_duration.as_secs_f64(),
                ..LlHlsConfig::default()
            },
        ));
        let vod_manager = Arc::new(HlsVodManager::new(
            Vec::new(),
            HlsVodConfig {
                segment_duration: config.hls.segment_duration,
                cache_dir: config.server.state_dir.join("vod-cache"),
            },
        ));
        let vod = Arc::new(VodCatalog::new(&config, vod_manager.clone()));
        vod.refresh();

        let mut http_config = HttpConfig::new(config.http.listen);
        if let Some(tls) = &config.http.tls {
            let certificate = std::fs::read(&tls.certificate)?;
            let private_key = std::fs::read(&tls.private_key)?;
            http_config = http_config.with_tls(rtflow::security::TlsServerConfig::from_pem(
                &certificate,
                &private_key,
            )?);
            tracing::info!(cert = %tls.certificate.display(), "HTTPS enabled");
        }
        let mut http = HttpServer::bind(http_config).await?;
        let http_addr = http.local_addr();
        if config.hls.enabled {
            http.add_route(
                Method::Get,
                "/hls/*",
                crate::media::auth_guard(
                    create_hls_live_handler(hls),
                    auth.clone(),
                    mounts.clone(),
                    "/hls/",
                    rtflow::auth::AccessRole::Read,
                    "hls",
                ),
            )
            .await;
            if config.hls.low_latency {
                http.add_route(
                    Method::Get,
                    "/llhls/*",
                    crate::media::auth_guard(
                        create_ll_hls_handler(ll_hls),
                        auth.clone(),
                        mounts.clone(),
                        "/llhls/",
                        rtflow::auth::AccessRole::Read,
                        "llhls",
                    ),
                )
                .await;
            }
            http.add_route(
                Method::Get,
                "/vod/*",
                crate::media::vod_auth_guard(
                    create_hls_vod_handler(vod_manager.clone()),
                    auth.clone(),
                    vod.clone(),
                ),
            )
            .await;
        }
        if config.flv.enabled {
            http.add_route(
                Method::Get,
                "/flv/*",
                crate::media::auth_guard(
                    crate::media::flv_handler(mounts.clone()),
                    auth.clone(),
                    mounts.clone(),
                    "/flv/",
                    rtflow::auth::AccessRole::Read,
                    "flv",
                ),
            )
            .await;
        }
        if config.dash.enabled {
            http.add_route(
                Method::Get,
                "/dash/*",
                crate::media::auth_guard(
                    crate::media::dash_handler(mounts.clone(), config.clone()),
                    auth.clone(),
                    mounts.clone(),
                    "/dash/",
                    rtflow::auth::AccessRole::Read,
                    "dash",
                ),
            )
            .await;
        }
        if config.webrtc.enabled {
            let (whip, whep) = create_webrtc_handlers(WebRtcConfig::default(), mounts.clone());
            http.add_route(
                Method::Post,
                "/whip/*",
                crate::media::auth_guard(
                    whip,
                    auth.clone(),
                    mounts.clone(),
                    "/whip/",
                    rtflow::auth::AccessRole::Publish,
                    "webrtc",
                ),
            )
            .await;
            http.add_route(
                Method::Post,
                "/whep/*",
                crate::media::auth_guard(
                    whep,
                    auth.clone(),
                    mounts.clone(),
                    "/whep/",
                    rtflow::auth::AccessRole::Read,
                    "webrtc",
                ),
            )
            .await;
        }
        if config.http.health {
            http.add_route(Method::Get, "/healthz", crate::media::health_handler())
                .await;
            http.add_route(Method::Get, "/readyz", crate::media::health_handler())
                .await;
        }
        if config.http.metrics {
            http.add_route(
                Method::Get,
                "/metrics",
                crate::metrics::metrics_handler(
                    metrics.clone(),
                    mounts.clone(),
                    recording.clone(),
                    counters.clone(),
                    hooks.clone(),
                ),
            )
            .await;
        }
        let discovery = Arc::new(DiscoveryService::new(
            config.clone(),
            paths.clone(),
            mounts.clone(),
        ));
        let admin = if config.http.admin && !config.http.admin_password.is_empty() {
            let admin_config =
                AdminConfig::new(&config.http.admin_username, &config.http.admin_password)?
                    .with_title(format!("{} 控制台", config.server.name))
                    .with_hls_prefix("/hls")
                    .with_ll_hls_prefix("/llhls")
                    .with_rtmp_base(
                        public_rtmp(&config).unwrap_or_else(|| "rtmp://127.0.0.1:1935".into()),
                    )
                    .with_vod(vod_manager.clone())
                    .with_control(Arc::new(crate::control::ControlBridge {
                        discovery: discovery.clone(),
                        paths: paths.clone(),
                        keys: keys.clone(),
                        recording: recording.clone(),
                        mounts: mounts.clone(),
                    }));
            Some(Arc::new(Admin::new(admin_config, mounts.clone())?))
        } else {
            if config.http.admin {
                tracing::warn!("admin console disabled: http.admin_password is empty");
            }
            None
        };
        crate::api::register_api_routes(
            &http,
            config.clone(),
            discovery.clone(),
            paths.clone(),
            keys.clone(),
            vod.clone(),
            recording.clone(),
            mounts.clone(),
            audit.clone(),
            admin.clone(),
            srt_listener_stats,
            srt_pull_status.clone(),
            srt_push_status.clone(),
            proxies.clone(),
        )
        .await;
        // The console's catch-all assets route must be registered after the
        // JSON API, otherwise `/admin/api/v1/...` would be served the SPA.
        if let Some(admin) = &admin {
            admin.register(&http).await;
        }

        cancels.push(http.cancellation_token());
        let task = tokio::spawn(async move {
            if let Err(error) = http.run().await {
                tracing::error!(%error, "HTTP server stopped");
            }
        });
        tasks.push(task);

        let ws_flv_addr = if config.flv.enabled {
            let server = WsFlvServer::bind(
                WsFlvConfig {
                    bind_addr: config.flv.ws_listen,
                    ..WsFlvConfig::default()
                },
                mounts.clone(),
            )
            .await?;
            let addr = server.local_addr();
            tasks.push(tokio::spawn(async move {
                if let Err(error) = server.run().await {
                    tracing::error!(%error, "WS-FLV server stopped");
                }
            }));
            Some(addr)
        } else {
            None
        };

        {
            let recording = recording.clone();
            let vod = vod.clone();
            let retention = config.recording.retention;
            let enabled = config.recording.enabled;
            tasks.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(500));
                let mut refresh = tokio::time::interval(Duration::from_secs(2));
                let mut prune = tokio::time::interval(Duration::from_secs(3600));
                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            recording.tick().await;
                        }
                        _ = refresh.tick() => {
                            vod.refresh();
                        }
                        _ = prune.tick() => {
                            if enabled {
                                let removed = vod.prune(retention);
                                if removed > 0 {
                                    tracing::info!(removed, "pruned expired recordings");
                                }
                            }
                        }
                    }
                }
            }));
        }

        if config.proxy.enabled {
            for mount in &config.proxy.mounts {
                if !mount.enabled {
                    continue;
                }
                let proxies = proxies.clone();
                let event_sink = event_sink.clone();
                let cancel = cancel.clone();
                let mount = mount.clone();
                tasks.push(tokio::spawn(async move {
                    let path: std::sync::Arc<str> = std::sync::Arc::from(mount.path.as_str());
                    let upstream = match mount.upstream_config() {
                        Ok(upstream) => upstream,
                        Err(error) => {
                            tracing::warn!(path = %mount.path, %error, "proxy config rejected");
                            return;
                        }
                    };
                    let startup_cache = upstream.startup_cache;
                    let options = rtflow::mount::MountOptions::builder()
                        .event_sink(event_sink)
                        .startup_cache_config(startup_cache)
                        .cancel(cancel.child_token())
                        .build();
                    match proxies.register(path.clone(), upstream, options).await {
                        Ok(_) => tracing::info!(path = %path, "proxy mount registered"),
                        Err(error) => {
                            tracing::warn!(path = %path, %error, "proxy mount registration failed");
                        }
                    }
                }));
            }
        }

        let rtsp_addr = rtsp.as_ref().map(|server| server.local_addr());
        tracing::info!(
            rtsp = ?rtsp_addr,
            rtmp = ?rtmp_addr,
            srt = ?srt_addr,
            http = %http_addr,
            ws_flv = ?ws_flv_addr,
            "rtflow-server started"
        );
        let _ = (rtmp_cancel, srt_cancel);
        Ok(ServerHandle {
            rtsp_addr,
            rtmp_addr,
            srt_addr,
            http_addr,
            ws_flv_addr,
            cancel,
            cancels,
            tasks,
            rtsp,
            recording,
            paths,
            keys,
            discovery,
            vod,
            proxies,
            config,
        })
    }
}
