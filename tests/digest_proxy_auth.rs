mod support;

use futures::future::BoxFuture;
use rtsp::{
    AuthDecision, Authenticator, Codec, Credentials, MountConfig, RtspServer, ServerConfig,
    auth::{AuthRequest, verify_digest},
    client::{ClientOptions, RtspClient},
    proxy::{UpstreamConfig, UpstreamTransport},
};
use std::{sync::Arc, time::Duration};
use support::{description, unit, wait_until, within};

struct DigestAuthenticator {
    credentials: Credentials,
}

impl Authenticator for DigestAuthenticator {
    fn authorize<'a>(&'a self, request: AuthRequest<'a>) -> BoxFuture<'a, AuthDecision> {
        Box::pin(async move {
            if request.authorization.is_some_and(|header| {
                verify_digest(header, request.method, &self.credentials, "upstream")
                    .unwrap_or(false)
            }) {
                AuthDecision::Allow
            } else {
                AuthDecision::DigestChallenge {
                    realm: "upstream".into(),
                    nonce: "integration-nonce".into(),
                }
            }
        })
    }
}

#[tokio::test]
async fn proxy_authenticates_to_a_digest_protected_upstream() {
    let credentials = Credentials {
        username: "proxy".into(),
        password: "secret".into(),
    };
    let mut upstream_config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    upstream_config.authenticator = Arc::new(DigestAuthenticator {
        credentials: credentials.clone(),
    });
    let upstream = RtspServer::bind(upstream_config).await.unwrap();
    let source = upstream
        .register_mount(
            Arc::from("/secured"),
            MountConfig::Local {
                description: description("secured", &[Codec::H264]),
            },
        )
        .await
        .unwrap();
    let publisher = source.publisher().unwrap();
    let upstream_url =
        url::Url::parse(&format!("rtsp://{}/secured", upstream.local_addr())).unwrap();
    assert!(
        matches!(
            RtspClient::describe(upstream_url.clone(), ClientOptions::default()).await,
            Err(rtsp::Error::Unauthorized)
        ),
        "anonymous DESCRIBE must be rejected"
    );

    let proxy = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut config = UpstreamConfig::new(upstream_url, UpstreamTransport::TcpInterleaved);
    config.credentials = Some(credentials);
    let proxy_mount = proxy
        .register_mount(
            Arc::from("/proxy"),
            MountConfig::Proxy {
                upstream: Box::new(config),
            },
        )
        .await
        .unwrap();
    let mut client = RtspClient::connect(
        url::Url::parse(&format!("rtsp://{}/proxy", proxy.local_addr())).unwrap(),
        ClientOptions::default(),
    )
    .await
    .unwrap();
    wait_until(Duration::from_secs(2), || {
        source.snapshot().subscriber_count > 0 && proxy_mount.snapshot().subscriber_count > 0
    })
    .await;
    let expected = unit(0, Codec::H264, 1);
    publisher.try_push(expected.clone()).unwrap();
    let received = within(Duration::from_secs(1), client.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received.data, expected.data);
    drop(client);
    proxy.shutdown().await.unwrap();
    upstream.shutdown().await.unwrap();
}
