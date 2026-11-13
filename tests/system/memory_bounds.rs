//! Embedded memory budget checks.

use bytes::Bytes;
use rtflow::{
    AccessUnit, Codec, ServerConfig,
    mount::continuity::{KeyframeCache, StartupCacheConfig},
    transport::UdpPair,
};
use rtflow_testkit::description;
use socket2::SockRef;
use std::net::{IpAddr, Ipv4Addr};

#[test]
fn embedded_preset_uses_small_buffers() {
    let config = ServerConfig::embedded("127.0.0.1:0".parse().unwrap());
    assert_eq!(config.udp_recv_buffer, 256 * 1024);
    assert_eq!(config.max_sessions, 32);
    assert_eq!(config.consumer_queue_capacity, 64);
    assert_eq!(config.resource_limits.max_tracks_per_session, 8);
    assert_eq!(config.startup_cache.max_bytes_total, 512 * 1024);
    config.validate().unwrap();
}

#[tokio::test]
async fn udp_receive_buffer_is_applied_and_mux_shares_one_socket() {
    let pair = UdpPair::bind_with_recv_buffer(IpAddr::V4(Ipv4Addr::LOCALHOST), None, 256 * 1024)
        .await
        .unwrap();
    // Linux doubles the requested value; other kernels may round it. It must
    // at least cover the configured size and stay far below the 4 MiB default.
    let applied = SockRef::from(&*pair.rtp).recv_buffer_size().unwrap();
    assert!(applied >= 256 * 1024, "applied SO_RCVBUF = {applied}");
    assert!(applied < 4 * 1024 * 1024, "applied SO_RCVBUF = {applied}");

    let mux = UdpPair::bind_mux(IpAddr::V4(Ipv4Addr::LOCALHOST), None, 256 * 1024)
        .await
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(&mux.rtp, &mux.rtcp));
    assert_eq!(mux.rtp_port().unwrap(), mux.rtcp_port().unwrap());
}

#[test]
fn startup_cache_respects_mount_wide_budget() {
    let description = description("cache", &[Codec::H264, Codec::H264]);
    let config = StartupCacheConfig {
        enabled: true,
        max_bytes: 2 * 1024 * 1024,
        max_bytes_total: 1024 * 1024,
        max_frames_per_track: 8,
    };
    let mut cache = KeyframeCache::new(&description, config).unwrap();
    for track_id in 0..2u8 {
        cache.push(&AccessUnit {
            track_id,
            codec: Codec::H264,
            pts: std::time::Duration::ZERO,
            dts: None,
            rtp_timestamp: 0,
            is_keyframe: true,
            data: Bytes::from(vec![0x55; 600 * 1024]),
        });
    }
    let (_, bytes) = cache.usage();
    assert!(
        bytes <= 1024 * 1024,
        "startup cache exceeded total budget: {bytes}"
    );
}
