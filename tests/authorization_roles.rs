mod support;

use futures::future::BoxFuture;
use rtsp::{
    AccessRole, AuthDecision, Authenticator, Codec, MountConfig, RtspServer, ServerConfig,
    auth::AuthRequest,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use support::{RawRtsp, description, wait_until};

struct RoleRecorder {
    roles: Arc<Mutex<Vec<(rtsp::rtsp::Method, AccessRole)>>>,
}

impl Authenticator for RoleRecorder {
    fn authorize<'a>(&'a self, request: AuthRequest<'a>) -> BoxFuture<'a, AuthDecision> {
        self.roles
            .lock()
            .unwrap()
            .push((request.method, request.role));
        Box::pin(async { AuthDecision::Allow })
    }
}

#[tokio::test]
async fn setup_role_tracks_the_target_mount_kind() {
    let roles = Arc::new(Mutex::new(Vec::new()));
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    config.authenticator = Arc::new(RoleRecorder {
        roles: roles.clone(),
    });
    let server = RtspServer::bind(config).await.unwrap();
    server
        .register_mount(
            Arc::from("/read"),
            MountConfig::Local {
                description: description("read", &[Codec::H264]),
            },
        )
        .await
        .unwrap();
    server
        .register_mount(
            Arc::from("/publish"),
            MountConfig::Publisher {
                description: description("publish", &[Codec::H264]),
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();

    let mut read = RawRtsp::connect(server.local_addr()).await;
    let read_base = format!("rtsp://{}/read", server.local_addr());
    assert_eq!(
        read.request(
            "SETUP",
            &format!("{read_base}/trackID=0"),
            "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n",
        )
        .await
        .status,
        200
    );
    let mut publish = RawRtsp::connect(server.local_addr()).await;
    let publish_base = format!("rtsp://{}/publish", server.local_addr());
    assert_eq!(
        publish
            .request(
                "SETUP",
                &format!("{publish_base}/trackID=0"),
                "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n",
            )
            .await
            .status,
        200
    );
    wait_until(Duration::from_secs(1), || roles.lock().unwrap().len() >= 2).await;
    {
        let roles = roles.lock().unwrap();
        assert!(roles.contains(&(rtsp::rtsp::Method::Setup, AccessRole::Read)));
        assert!(roles.contains(&(rtsp::rtsp::Method::Setup, AccessRole::Publish)));
    }
    server.shutdown().await.unwrap();
}
