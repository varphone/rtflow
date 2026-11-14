//! RTMP ingest + HLS live/VOD + read-only browser console.
//!
//! Run:
//! `cargo run --example live_vod_gateway --features "server,rtmp,hls,http-flv,file-storage,mp4,mkv"`
//! OBS: Server `rtmp://127.0.0.1:1935/live`, Stream Key `cam` (or `cam?token=...`).
use clap::Parser;
use rtflow::{
    MountRegistry,
    file::{FileSourceOptions, FileVodSource},
    hls::{
        HlsConfig,
        live::{HlsLiveManager, create_hls_live_handler},
        vod::{HlsVodConfig, HlsVodEntry, HlsVodManager, create_hls_vod_handler},
    },
    http::{HttpConfig, HttpServer, Method},
    http_flv::console::{
        create_console_handler, create_health_api, create_mounts_api, create_vods_api,
    },
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
            segment_count: 5,
            live: true,
        },
    ));
    let mut http = HttpServer::bind(HttpConfig::new(args.http)).await?;
    http.add_route(Method::Get, "/hls/*", create_hls_live_handler(live))
        .await;
    http.add_route(Method::Get, "/vod/*", create_hls_vod_handler(vod.clone()))
        .await;
    http.add_route(Method::Get, "/", create_console_handler(false))
        .await;
    http.add_route(Method::Get, "/admin", create_console_handler(true))
        .await;
    http.add_route(
        Method::Get,
        "/api/mounts",
        create_mounts_api(mounts.clone()),
    )
    .await;
    http.add_route(Method::Get, "/api/vods", create_vods_api(Some(vod)))
        .await;
    http.add_route(
        Method::Get,
        "/api/health",
        create_health_api(mounts.clone()),
    )
    .await;
    let http_addr = http.local_addr();
    let http_task = tokio::spawn(async move { http.run().await });

    println!("RTSP: rtsp://{}/", rtsp.local_addr());
    println!("RTMP: rtmp://{rtmp_addr}/live  (stream key: cam)");
    println!("Web:  http://{http_addr}/  (admin: /admin)");
    tokio::signal::ctrl_c().await?;
    rtmp_cancel.cancel();
    let _ = rtmp_task.await;
    http_task.abort();
    rtsp.shutdown().await?;
    Ok(())
}
