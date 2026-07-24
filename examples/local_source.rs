use bytes::Bytes;
use rtsp::{
    AccessUnit, Codec, MediaDescription, MediaKind, MountConfig, RtspServer, ServerConfig,
    TrackDescription,
};
use std::{sync::Arc, time::Duration};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = RtspServer::bind(ServerConfig::new("0.0.0.0:8554".parse()?)).await?;
    let description = MediaDescription {
        session_name: Arc::from("camera"),
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Video,
            codec: Codec::H264,
            clock_rate: 90_000,
            channels: None,
            payload_type: 96,
            fmtp: None,
            control: Arc::from("trackID=0"),
        }],
    };
    let mount = server
        .register_mount(Arc::from("/camera"), MountConfig::Local { description })
        .await?;
    let publisher = mount.publisher()?;
    // Replace this loop with frames from a hardware encoder. `Bytes` is shared by subscribers.
    loop {
        let _ = publisher.try_push(AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::ZERO,
            dts: None,
            rtp_timestamp: 0,
            is_keyframe: true,
            data: Bytes::from_static(&[0x65, 0x88, 0x84]),
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
