//! Server application lifecycle and protocol wiring.
use crate::{
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
    dash::{DashConfig, DashHandlers, create_dash_router},
    hls::{
        HlsConfig,
        live::{HlsLiveManager, create_hls_live_handler},
        ll::{LlHlsConfig, LlHlsLiveManager, create_ll_hls_handler},
        vod::{HlsVodConfig, HlsVodManager, create_hls_vod_handler},
    },
    http::{Handler, HttpConfig, HttpServer, Method},
    http_flv::{
        handler::serve_flv_stream,
        ws::{WsFlvConfig, WsFlvServer},
    },
    metrics::{MetricsRegistry, create_metrics_handler},
    rtmp::{RtmpConfig, RtmpServer, session::RtmpSessionConfig},
    srt::{SrtConfig, SrtServer},
    webrtc::{WebRtcConfig, create_webrtc_handlers},
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

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
}

impl ServerHandle {
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        for cancel in &self.cancels {
            cancel.cancel();
        }
        self.recording.shutdown().await;
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

    pub async fn wait_for_signal(self) {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
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
        let auth =
            Arc::new(AppAuth::new(&config, keys.clone(), paths.clone())?.with_hooks(hooks.clone()));
        let recording =
            RecordingSupervisor::new(config.clone(), paths.clone(), mounts.clone(), hooks.clone());

        let event_sink: Arc<dyn rtflow::EventSink> = Arc::new(AppEventSink {
            metrics: rtflow::core::metrics::MetricsEventSink::new(metrics.clone()),
            hooks: hooks.clone(),
        });

        let cancel = CancellationToken::new();
        let mut cancels = Vec::new();
        let mut tasks = Vec::new();

        let rtsp = if config.rtsp.enabled {
            let mut rtsp_config = rtflow::server::ServerConfig::new(config.rtsp.listen);
            rtsp_config.session_timeout = config.rtsp.session_timeout;
            rtsp_config.max_sessions = config.rtsp.max_sessions;
            rtsp_config.dynamic_publishers = config.rtsp.dynamic_publishers;
            rtsp_config.authenticator = auth.clone();
            rtsp_config.event_sink = event_sink.clone();
            let server = rtflow::server::RtspServer::bind_with(rtsp_config, mounts.clone()).await?;
            Some(server)
        } else {
            None
        };

        let (rtmp_addr, rtmp_cancel) = if config.rtmp.enabled {
            let session = RtmpSessionConfig {
                authenticator: Some(auth.clone()),
                ..RtmpSessionConfig::default()
            };
            #[allow(clippy::needless_update)] // `metrics` only exists with feature
            let rtmp_config = RtmpConfig {
                bind_addr: config.rtmp.listen,
                session,
                max_connections: config.rtmp.max_connections,
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

        let (srt_addr, srt_cancel) = if config.srt.enabled {
            let srt_config = SrtConfig {
                bind_addr: config.srt.listen,
                allowed_stream_ids: config.srt.allowed_stream_ids.clone(),
                ..SrtConfig::default()
            };
            let server = SrtServer::with_config(srt_config, mounts.clone()).await?;
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

        let mut http = HttpServer::bind(HttpConfig::new(config.http.listen)).await?;
        let http_addr = http.local_addr();
        if config.hls.enabled {
            http.add_route(
                Method::Get,
                "/hls/*",
                auth_guard(
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
                    auth_guard(
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
                auth_guard(
                    create_hls_vod_handler(vod_manager.clone()),
                    auth.clone(),
                    mounts.clone(),
                    "/vod/",
                    rtflow::auth::AccessRole::Read,
                    "vod",
                ),
            )
            .await;
        }
        if config.flv.enabled {
            http.add_route(
                Method::Get,
                "/flv/*",
                auth_guard(
                    flv_handler(mounts.clone()),
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
                auth_guard(
                    dash_handler(mounts.clone(), config.clone()),
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
                auth_guard(
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
                auth_guard(
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
            http.add_route(Method::Get, "/healthz", health_handler())
                .await;
            http.add_route(Method::Get, "/readyz", health_handler())
                .await;
        }
        if config.http.metrics {
            http.add_route(
                Method::Get,
                "/metrics",
                create_metrics_handler(metrics.clone()),
            )
            .await;
        }
        let discovery = Arc::new(DiscoveryService::new(
            config.clone(),
            paths.clone(),
            mounts.clone(),
        ));
        crate::api::register_api_routes(
            &http,
            config.clone(),
            discovery.clone(),
            paths.clone(),
            keys.clone(),
            vod.clone(),
            recording.clone(),
            mounts.clone(),
        )
        .await;

        if config.http.admin && !config.http.admin_password.is_empty() {
            let admin_config =
                AdminConfig::new(&config.http.admin_username, &config.http.admin_password)?
                    .with_title(format!("{} 控制台", config.server.name))
                    .with_hls_prefix("/hls")
                    .with_ll_hls_prefix("/llhls")
                    .with_rtmp_base(
                        public_rtmp(&config).unwrap_or_else(|| "rtmp://127.0.0.1:1935".into()),
                    )
                    .with_vod(vod_manager.clone());
            let admin = Admin::new(admin_config, mounts.clone())?;
            admin.register(&http).await;
        } else if config.http.admin {
            tracing::warn!("admin console disabled: http.admin_password is empty");
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
        })
    }
}

struct AppEventSink {
    metrics: rtflow::core::metrics::MetricsEventSink,
    hooks: Option<Arc<HookClient>>,
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

fn health_handler() -> Handler {
    Arc::new(|_req, mut writer| {
        Box::pin(async move {
            writer
                .write_body(200, "application/json", br#"{"status":"ok"}"#)
                .await?;
            Ok(writer)
        })
    })
}

fn auth_guard(
    inner: Handler,
    auth: Arc<AppAuth>,
    mounts: Arc<MountRegistry>,
    prefix: &'static str,
    role: rtflow::auth::AccessRole,
    protocol: &'static str,
) -> Handler {
    Arc::new(move |req, writer| {
        let auth = auth.clone();
        let mounts = mounts.clone();
        let inner = inner.clone();
        Box::pin(async move {
            let full = req.path.trim_start_matches('/').to_owned();
            if prefix == "/vod/" {
                // VOD ids are not mount paths; authorize against the VOD
                // namespace so `vod/**` tokens and read rules apply.
                if !auth.verify_http_request("vod", &req, role, protocol) {
                    let mut writer = writer;
                    writer
                        .write_body(401, "text/plain", b"unauthorized")
                        .await?;
                    return Ok(writer);
                }
                return inner(req, writer).await;
            }
            let mount_path = mounts
                .find_longest_prefix(&format!("/{full}"))
                .map(|(id, _)| id.as_str().trim_start_matches('/').to_owned())
                .or_else(|| {
                    req.path.strip_prefix(prefix).map(|rest| {
                        rest.split('/')
                            .next()
                            .unwrap_or(rest)
                            .trim_end_matches(".flv")
                            .to_owned()
                    })
                })
                .unwrap_or_else(|| full.clone());
            if !auth.verify_http_request(&mount_path, &req, role, protocol) {
                let mut writer = writer;
                writer
                    .write_body(401, "text/plain", b"unauthorized")
                    .await?;
                return Ok(writer);
            }
            inner(req, writer).await
        })
    })
}

fn flv_handler(mounts: Arc<MountRegistry>) -> Handler {
    Arc::new(move |req, writer| {
        let mounts = mounts.clone();
        Box::pin(async move {
            let path = req
                .path
                .trim_start_matches("/flv/")
                .strip_suffix(".flv")
                .unwrap_or("");
            tracing::debug!(request_path = %req.path, mount = %path, "FLV request");
            let Some(handle) = mounts.get_handle(&format!("/{path}")) else {
                tracing::debug!(mount = %path, "FLV mount not found");
                let mut writer = writer;
                writer
                    .write_body(404, "text/plain", b"stream not found")
                    .await?;
                return Ok(writer);
            };
            let description = handle.description();
            let (receiver, startup) = handle.subscribe_with_startup();
            serve_flv_stream(req, writer, description, receiver, startup, handle).await
        })
    })
}

fn dash_handler(mounts: Arc<MountRegistry>, config: Arc<Config>) -> Handler {
    let cache: Arc<Mutex<HashMap<String, Arc<DashHandlers>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    Arc::new(move |req, writer| {
        let mounts = mounts.clone();
        let config = config.clone();
        let cache = cache.clone();
        Box::pin(async move {
            let Some((mount_path, kind)) = dash_route(&req.path) else {
                let mut writer = writer;
                writer.write_body(404, "text/plain", b"not found").await?;
                return Ok(writer);
            };
            let Some(handle) = mounts.get_handle(&format!("/{mount_path}")) else {
                let mut writer = writer;
                writer
                    .write_body(404, "text/plain", b"mount not found")
                    .await?;
                return Ok(writer);
            };
            let handlers = {
                let mut cache = cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                cache
                    .entry(mount_path.to_owned())
                    .or_insert_with(|| {
                        Arc::new(create_dash_router(
                            DashConfig {
                                segment_duration: config.hls.segment_duration,
                                segment_count: config.hls.window,
                                ..DashConfig::default()
                            },
                            &handle,
                        ))
                    })
                    .clone()
            };
            let handler = match kind {
                DashKind::Manifest => handlers.manifest.clone(),
                DashKind::Init => handlers.init.clone(),
                DashKind::Segment => handlers.segment.clone(),
            };
            handler(req, writer).await
        })
    })
}

enum DashKind {
    Manifest,
    Init,
    Segment,
}

fn dash_route(path: &str) -> Option<(&str, DashKind)> {
    let path = path.strip_prefix("/dash/")?;
    if let Some(mount) = path.strip_suffix("/manifest.mpd") {
        return Some((mount, DashKind::Manifest));
    }
    if let Some(mount) = path.strip_suffix("/init.m4s") {
        return Some((mount, DashKind::Init));
    }
    let (mount, file) = path.rsplit_once('/')?;
    if file.ends_with(".m4s") {
        return Some((mount, DashKind::Segment));
    }
    None
}
