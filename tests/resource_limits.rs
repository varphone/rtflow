mod support;

use rtsp::{
    Codec, Error, MountConfig, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
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
        rtsp::server::PushResult::DroppedNoSubscribers
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
async fn invalid_client_options_are_checked_before_network_io() {
    let url = url::Url::parse("rtsp://127.0.0.1:9/missing").unwrap();
    assert!(matches!(
        RtspClient::describe(
            url.clone(),
            ClientOptions {
                output_queue_capacity: 0,
                ..Default::default()
            },
        )
        .await,
        Err(Error::Limit("client output queue capacity"))
    ));
    assert!(matches!(
        RtspClient::describe(
            url,
            ClientOptions {
                keep_alive_interval: Some(Duration::ZERO),
                ..Default::default()
            },
        )
        .await,
        Err(Error::Limit("client keep-alive interval"))
    ));
}
