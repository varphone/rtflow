mod support;

use rtflow::{
    Codec, Error, MountConfig, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
    proxy::UpstreamTransport,
};
use std::{sync::Arc, time::Duration};
use support::{description, unit};

#[tokio::test]
async fn invalid_capacities_and_mount_operations_fail_explicitly() {
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    config.consumer_queue_capacity = 0;
    assert!(matches!(
        RtspServer::bind(config).await,
        Err(Error::Limit("consumer queue capacity"))
    ));

    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    config.max_sessions = 0;
    assert!(matches!(
        RtspServer::bind(config).await,
        Err(Error::Limit("RTSP sessions"))
    ));

    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let media = description("limits", &[Codec::H264]);
    assert!(
        matches!(
            server
                .register_mount(
                    Arc::from("missing-slash"),
                    MountConfig::Local {
                        description: media.clone(),
                    },
                )
                .await,
            Err(Error::Rtsp(_))
        ),
        "invalid mount paths must be rejected"
    );
    let mount = server
        .register_mount(
            Arc::from("/limits"),
            MountConfig::Local {
                description: media.clone(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        server
            .register_mount(
                Arc::from("/limits"),
                MountConfig::Local { description: media },
            )
            .await,
        Err(Error::Conflict(_))
    ));
    let publisher = mount.publisher().unwrap();
    let wrong_codec = unit(0, Codec::Pcma, 1);
    assert!(matches!(
        publisher.try_push(wrong_codec),
        Err(Error::Media(_))
    ));
    assert_eq!(
        publisher.try_push(unit(0, Codec::H264, 1)).unwrap(),
        rtflow::server::PushResult::DroppedNoSubscribers
    );
    assert_eq!(mount.snapshot().stats.dropped_frames, 1);
    server.unregister_mount("/limits").await.unwrap();
    assert!(publisher.is_closed());
    assert!(matches!(
        server.unregister_mount("/limits").await,
        Err(Error::NotFound(_))
    ));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn per_session_byte_budget_drops_oversized_access_units() {
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    config.resource_limits.max_queued_bytes_per_session = 2;
    let server = RtspServer::bind(config).await.unwrap();
    let mount = server
        .register_mount(
            Arc::from("/bytes"),
            MountConfig::Local {
                description: description("bytes", &[Codec::H264]),
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let url = url::Url::parse(&format!("rtsp://{}/bytes", server.local_addr())).unwrap();
    let options = ClientOptions::builder()
        .transport(UpstreamTransport::TcpInterleaved)
        .build()
        .unwrap();
    let mut client = RtspClient::connect(url, options).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    publisher.try_push(unit(0, Codec::H264, 1)).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while mount.snapshot().stats.dropped_frames == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), client.next())
            .await
            .is_err()
    );
    drop(client);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_client_options_are_checked_before_network_io() {
    // The builder validates at build() time, so degenerate options never
    // even reach RtspClient (which re-validates defensively).
    assert!(matches!(
        ClientOptions::builder().output_queue_capacity(0).build(),
        Err(Error::Limit("client output queue capacity"))
    ));
    assert!(matches!(
        ClientOptions::builder()
            .keep_alive_interval(Some(Duration::ZERO))
            .build(),
        Err(Error::Limit("client keep-alive interval"))
    ));
    assert!(matches!(
        ClientOptions::builder()
            .request_timeout(Duration::ZERO)
            .build(),
        Err(Error::Limit("client request timeout"))
    ));
}
