//! `register_mount_with` must pass per-mount options through to the mount.
#![cfg(feature = "server")]

use rtflow::{
    Codec, MountConfig, MountOptions, RtspServer, ServerConfig,
    mount::continuity::StartupCacheConfig,
};
use rtflow_testkit::description;
use std::sync::Arc;

#[tokio::test]
async fn register_mount_with_honors_startup_cache_policy() {
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount_with(
            Arc::from("/mount-options"),
            MountConfig::Local {
                description: description("mount-options", &[Codec::H264]),
            },
            MountOptions {
                startup_cache_config: Some(StartupCacheConfig {
                    enabled: true,
                    max_bytes: 1024 * 1024,
                    max_bytes_total: 1024 * 1024,
                    max_frames_per_track: 1,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    publisher
        .try_push(rtflow_testkit::unit(0, Codec::H264, 0))
        .unwrap();
    publisher
        .try_push(rtflow_testkit::unit(0, Codec::H264, 1))
        .unwrap();
    assert_eq!(
        mount.snapshot().startup_cache_frames,
        1,
        "startup cache max_frames_per_track from MountOptions was ignored"
    );
    server.shutdown().await.unwrap();
}
