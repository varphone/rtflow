//! RTMP ingest + HLS live/VOD + feature-gated admin console.
//!
//! Run:
//! `cargo run --example live_vod_gateway --features "server,rtmp,hls,http-flv,file-storage,mp4,mkv,admin"`
//! OBS: Server `rtmp://127.0.0.1:1935/live`, Stream Key `cam` (or `cam?token=...`).
use clap::Parser;
use rtflow::{
    MountRegistry,
    admin::{Admin, AdminConfig},
    file::{FileSourceOptions, FileVodSource},
    hls::{
        HlsConfig,
        live::{HlsLiveManager, create_hls_live_handler},
        vod::{HlsVodConfig, HlsVodEntry, HlsVodManager, create_hls_vod_handler},
    },
    http::{HttpConfig, HttpServer, Method},
    rtmp::{RtmpConfig, RtmpServer, session::RtmpSessionConfig},
    server::{RtspServer, ServerConfig},
    storage::RecordingVodSource,
};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8554")]
    rtsp: SocketAddr,
    #[arg(long, default_value = "0.0.0.0:1935")]
    rtmp: SocketAddr,
    #[arg(long, default_value = "0.0.0.0:8080")]
    http: SocketAddr,
    /// Optional file VOD: `id=path`.
    #[arg(long = "vod-file")]
    vod_files: Vec<String>,
    /// Optional recording directory VOD: `id=path`.
    #[arg(long = "recording")]
    recordings: Vec<String>,
    #[arg(long)]
    vod_cache: Option<PathBuf>,
    /// HMAC token secret for RTMP publish/play.
    #[arg(long)]
    token: Option<String>,
    #[arg(long, default_value = "admin")]
    admin_user: String,
    #[arg(long, default_value = "rtflow-admin")]
    admin_password: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mounts = Arc::new(MountRegistry::new(1024));

    let mut vod_entries = Vec::new();
    for spec in &args.vod_files {
        let (id, path) = spec.split_once('=').ok_or("--vod-file expects id=path")?;
        let source = FileVodSource::open(path, FileSourceOptions::default())?;
        vod_entries.push(HlsVodEntry {
            id: id.to_owned(),
            title: id.to_owned(),
            duration: None,
            source: Arc::new(source),
        });
    }
    for spec in &args.recordings {
        let (id, path) = spec
            .split_once('=')
            .ok_or("--recording expects id=directory")?;
        let source = RecordingVodSource::open(path, 8 * 1024 * 1024)?;
        vod_entries.push(HlsVodEntry {
            id: id.to_owned(),
            title: id.to_owned(),
            duration: None,
            source: Arc::new(source),
        });
    }
    let vod = Arc::new(HlsVodManager::new(
        vod_entries,
        HlsVodConfig {
            segment_duration: Duration::from_secs(1),
            cache_dir: args
                .vod_cache
                .unwrap_or_else(|| std::env::temp_dir().join("rtflow-vod-cache")),
        },
    ));

    let rtsp = RtspServer::bind_with(ServerConfig::new(args.rtsp), mounts.clone()).await?;
    let mut rtmp_config = RtmpConfig {
        bind_addr: args.rtmp,
        ..RtmpConfig::default()
    };
    rtmp_config.session = RtmpSessionConfig {
        token: args
            .token
            .as_ref()
            .map(|secret| rtflow::token_auth::TokenConfig::new(secret.as_bytes().to_vec())),
        ..RtmpSessionConfig::default()
    };
    let rtmp = RtmpServer::with_config(rtmp_config, mounts.clone()).await?;
    let rtmp_addr = rtmp.local_addr()?;
    let rtmp_cancel = rtmp.cancellation_token();
    let rtmp_task = tokio::spawn(rtmp.run());

    let live = Arc::new(HlsLiveManager::new(
        mounts.clone(),
        HlsConfig {
            segment_duration: Duration::from_secs(1),
            // 60 fps HLS cuts close to 1 s segments; keep a 10 s live
            // window so a slow segment request cannot evict the segment
            // hls.js is about to fetch.
            segment_count: 10,
            live: true,
        },
    ));
    let mut http = HttpServer::bind(HttpConfig::new(args.http)).await?;
    http.add_route(Method::Get, "/hls/*", create_hls_live_handler(live))
        .await;
    http.add_route(Method::Get, "/vod/*", create_hls_vod_handler(vod.clone()))
        .await;
    let admin_config = AdminConfig::new(&args.admin_user, &args.admin_password)?
        .with_title("rtflow 流媒体控制台")
        .with_hls_prefix("/hls")
        .with_rtmp_base(format!("rtmp://{rtmp_addr}"))
        .with_vod(vod);
    let admin = Admin::new(admin_config, mounts.clone())?;
    admin.register(&http).await;
    let http_addr = http.local_addr();
    let http_task = tokio::spawn(async move { http.run().await });

    println!("RTSP: rtsp://{}/", rtsp.local_addr());
    println!("RTMP: rtmp://{rtmp_addr}/live  (stream key: cam)");
    println!(
        "Admin: http://{http_addr}/admin  (user: {})",
        args.admin_user
    );
    tokio::signal::ctrl_c().await?;
    rtmp_cancel.cancel();
    let _ = rtmp_task.await;
    http_task.abort();
    rtsp.shutdown().await?;
    Ok(())
}
