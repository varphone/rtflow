use futures::future::BoxFuture;
use rtflow::{
    AccessUnit, AuthDecision, Authenticator, Codec, Credentials, MediaDescription, MediaKind,
    MountConfig, RtspServer, ServerConfig, TrackDescription,
    auth::{AuthRequest, verify_basic},
    client::{ClientOptions, RtspClient},
};
use std::sync::Arc;

struct BasicAuthenticator {
    credentials: Credentials,
}
impl Authenticator for BasicAuthenticator {
    fn authorize<'a>(&'a self, request: AuthRequest<'a>) -> BoxFuture<'a, AuthDecision> {
        Box::pin(async move {
            if request
                .authorization
                .is_some_and(|value| verify_basic(value, &self.credentials))
            {
                AuthDecision::Allow
            } else {
                AuthDecision::Challenge {
                    realm: "camera".into(),
                }
            }
        })
    }
}

fn description() -> MediaDescription {
    MediaDescription {
        session_name: Arc::from("secured"),
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Video,
            codec: Codec::H264,
            clock_rate: 90_000,
            channels: None,
            payload_type: 96,
            fmtp: None,
            control: Arc::from("trackID=0"),
            crypto: None,
        }],
        connection: None,
    }
}

#[tokio::test]
async fn credentials_protect_describe_setup_and_play() {
    let credentials = Credentials {
        username: "operator".into(),
        password: "secret".into(),
    };
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    config.authenticator = Arc::new(BasicAuthenticator {
        credentials: credentials.clone(),
    });
    let server = RtspServer::bind(config).await.unwrap();
    let mount = server
        .register_mount(
            Arc::from("/secured"),
            MountConfig::Local {
                description: description(),
            },
        )
        .await
        .unwrap();
    let url = url::Url::parse(&format!("rtsp://{}/secured", server.local_addr())).unwrap();
    let unauthorized = RtspClient::connect(url.clone(), ClientOptions::default()).await;
    assert!(matches!(unauthorized, Err(rtflow::Error::Unauthorized)));
    let mut session = RtspClient::connect(
        url,
        ClientOptions {
            credentials: Some(credentials),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let publisher = mount.publisher().unwrap();
    publisher
        .try_push(AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: std::time::Duration::ZERO,
            dts: None,
            rtp_timestamp: 1,
            is_keyframe: true,
            data: bytes::Bytes::from_static(&[0x65, 1]),
        })
        .unwrap();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(1), session.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(frame.data, bytes::Bytes::from_static(&[0, 0, 0, 1, 0x65, 1]));
    server.shutdown().await.unwrap();
}
