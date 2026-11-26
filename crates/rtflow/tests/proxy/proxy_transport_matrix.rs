use rtflow::{
    Codec, MountConfig, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
    proxy::{UpstreamConfig, UpstreamTransport},
};
use rtflow_testkit::{description, unit, wait_until, within};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn proxy_forwards_every_upstream_downstream_transport_pair() {
    for upstream_transport in [UpstreamTransport::Udp, UpstreamTransport::TcpInterleaved] {
        for downstream_transport in [UpstreamTransport::Udp, UpstreamTransport::TcpInterleaved] {
            let upstream = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
                .await
                .unwrap();
            let source = upstream
                .register_mount(
                    Arc::from("/source"),
                    MountConfig::Local {
                        description: description("proxy-source", &[Codec::H264, Codec::Pcma]),
                    },
                )
                .await
                .unwrap();
            let publisher = source.publisher().unwrap();

            let mut proxy_config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
            proxy_config.no_subscriber_grace = Duration::from_millis(200);
            let proxy = RtspServer::bind(proxy_config).await.unwrap();
            let upstream_config = UpstreamConfig::new(
                url::Url::parse(&format!("rtsp://{}/source", upstream.local_addr())).unwrap(),
                upstream_transport,
            );
            let proxy_mount = proxy
                .register_mount(
                    Arc::from("/proxy"),
                    MountConfig::Proxy {
                        upstream: Box::new(upstream_config),
                    },
                )
                .await
                .unwrap();
            let mut client = RtspClient::connect(
                url::Url::parse(&format!("rtsp://{}/proxy", proxy.local_addr())).unwrap(),
                ClientOptions::builder()
                    .transport(downstream_transport)
                    .build()
                    .unwrap(),
            )
            .await
            .unwrap();
            // Generous waits: the four transport pairs run under the same
            // parallel test binary and CI runners are heavily loaded.
            wait_until(Duration::from_secs(10), || {
                source.snapshot().subscriber_count > 0
                    && proxy_mount.snapshot().subscriber_count > 0
            })
            .await;
            for (track, codec) in [(0, Codec::H264), (1, Codec::Pcma)] {
                publisher.try_push(unit(track, codec, 1)).unwrap();
            }
            let mut codecs = Vec::new();
            for _ in 0..2 {
                codecs.push(
                    within(Duration::from_secs(5), client.next())
                        .await
                        .unwrap()
                        .unwrap()
                        .codec,
                );
            }
            codecs.sort_by_key(|codec| format!("{codec:?}"));
            assert!(codecs.contains(&Codec::H264));
            assert!(codecs.contains(&Codec::Pcma));
            drop(client);
            wait_until(Duration::from_secs(10), || {
                matches!(
                    proxy_mount.snapshot().proxy_state,
                    Some(rtflow::mount::upstream::ProxyState::Idle)
                )
            })
            .await;
            proxy.shutdown().await.unwrap();
            upstream.shutdown().await.unwrap();
        }
    }
}
