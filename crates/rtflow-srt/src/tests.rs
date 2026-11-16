use super::session::parse_stream_id_string;
use super::*;
use std::time::Duration;

#[tokio::test]
async fn server_binds_and_shuts_down() {
    let mounts = Arc::new(MountRegistry::new(10));
    let config = SrtConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        ..Default::default()
    };
    let server = SrtServer::with_config(config, mounts).await.unwrap();
    let addr = server.local_addr().unwrap();
    assert_ne!(addr.port(), 0);

    let cancel = server.cancellation_token();
    let handle = tokio::spawn(server.run());

    tokio::time::sleep(Duration::from_millis(10)).await;
    cancel.cancel();

    handle.await.unwrap().unwrap();
}

#[test]
fn stream_id_resource_is_extracted() {
    assert_eq!(
        parse_stream_id_string("#!::u=publisher,r=live/cam1"),
        "live/cam1"
    );
    assert_eq!(
        parse_stream_id_string("live/cam?token=abc"),
        "live/cam?token=abc"
    );
    assert_eq!(
        parse_stream_id_string("#!::r=live/cam?key=x,h=host"),
        "live/cam?key=x"
    );
    assert_eq!(parse_stream_id_string("#!::u=user"), "u=user");
}
