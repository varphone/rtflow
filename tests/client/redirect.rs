//! RTSP REDIRECT following.

use rtflow::{
    client::{ClientOptions, RtspClient},
    rtsp::Method,
};
use rtflow_testkit::{RawRtspServer, ServerReply, reply};
use std::time::Duration;

#[tokio::test]
async fn client_follows_same_origin_redirect() {
    let server = RawRtspServer::start(Box::new(|request| {
        if request.uri.ends_with("/start") {
            let mut response = reply(request, 302, "Found");
            response.headers.insert("location".into(), "/final".into());
            return ServerReply::Response(response);
        }
        let mut response = reply(request, 200, "OK");
        if request.method == Method::Describe {
            response.headers.insert(
                "content-type".into(),
                "application/sdp".into(),
            );
            response.headers.insert("content-base".into(), "/final/".into());
            response.body = b"v=0\r\ns=redirect\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:trackID=0\r\n".to_vec();
        } else {
            response
                .headers
                .insert("public".into(), "OPTIONS, DESCRIBE".into());
        }
        ServerReply::Response(response)
    }))
    .await;
    let url = format!("rtsp://{}/start", server.address());
    let description = tokio::time::timeout(
        Duration::from_secs(2),
        RtspClient::describe_str(&url, ClientOptions::default()),
    )
    .await
    .expect("redirect timed out")
    .expect("redirect describe failed");
    assert_eq!(description.tracks.len(), 1);
    let requests = server.requests().await;
    assert!(
        requests
            .iter()
            .any(|request| request.uri.ends_with("/final"))
    );
}
