//! ONVIF inspect flow against a fake SOAP endpoint (feature `onvif`).
#![cfg(feature = "onvif")]
use rtflow_server::onvif;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::test(flavor = "multi_thread")]
async fn inspect_reads_profiles_and_stream_uris() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}");
    let task_base = base.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let base = task_base.clone();
            tokio::spawn(async move {
                let (head, body) = read_request(&mut stream).await;
                let response = if !head.to_ascii_lowercase().contains("authorization:") {
                    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"cam\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else {
                    let payload = soap_response(&base, &body);
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });

    let details = onvif::inspect(
        &format!("{base}/onvif/device_service"),
        Some("admin".into()),
        Some("secret".into()),
    )
    .await
    .unwrap();
    assert_eq!(details.device.manufacturer, "Acme");
    assert_eq!(details.profiles.len(), 2);
    assert_eq!(
        details.profiles[0].uri.as_deref(),
        Some("rtsp://127.0.0.1/stream/main")
    );
    assert_eq!(
        details.profiles[1].uri.as_deref(),
        Some("rtsp://127.0.0.1/stream/sub")
    );
    task.abort();
}

fn soap_response(base: &str, body: &str) -> String {
    let envelope = |inner: &str| {
        format!(
            "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body>{inner}</s:Body></s:Envelope>"
        )
    };
    if body.contains("GetDeviceInformation") {
        envelope(
            "<tds:GetDeviceInformationResponse xmlns:tds=\"http://www.onvif.org/ver10/device/wsdl\"><tds:Manufacturer>Acme</tds:Manufacturer><tds:Model>Cam</tds:Model><tds:FirmwareVersion>1.0</tds:FirmwareVersion><tds:SerialNumber>SN</tds:SerialNumber><tds:HardwareId>HW</tds:HardwareId></tds:GetDeviceInformationResponse>",
        )
    } else if body.contains("GetCapabilities") {
        envelope(&format!(
            "<tds:GetCapabilitiesResponse xmlns:tds=\"http://www.onvif.org/ver10/device/wsdl\"><tds:Capabilities><tt:Media xmlns:tt=\"http://www.onvif.org/ver10/schema\"><tt:XAddr>{base}/onvif/media_service</tt:XAddr></tt:Media></tds:Capabilities></tds:GetCapabilitiesResponse>"
        ))
    } else if body.contains("GetProfiles") {
        envelope(
            "<trt:GetProfilesResponse xmlns:trt=\"http://www.onvif.org/ver10/media/wsdl\"><trt:Profiles token=\"main\"><tt:Name xmlns:tt=\"http://www.onvif.org/ver10/schema\">Main</tt:Name></trt:Profiles><trt:Profiles token=\"sub\"><tt:Name xmlns:tt=\"http://www.onvif.org/ver10/schema\">Sub</tt:Name></trt:Profiles></trt:GetProfilesResponse>",
        )
    } else {
        let uri = if body.contains(">main<") {
            "rtsp://127.0.0.1/stream/main"
        } else {
            "rtsp://127.0.0.1/stream/sub"
        };
        envelope(&format!(
            "<trt:GetStreamUriResponse xmlns:trt=\"http://www.onvif.org/ver10/media/wsdl\"><trt:MediaUri><tt:Uri xmlns:tt=\"http://www.onvif.org/ver10/schema\">{uri}</tt:Uri></trt:MediaUri></trt:GetStreamUriResponse>"
        ))
    }
}

async fn read_request(stream: &mut TcpStream) -> (String, String) {
    let mut buffer = Vec::new();
    let header_end = loop {
        let mut chunk = [0u8; 2048];
        let count = stream.read(&mut chunk).await.unwrap_or(0);
        if count == 0 {
            return (String::from_utf8_lossy(&buffer).into_owned(), String::new());
        }
        buffer.extend_from_slice(&chunk[..count]);
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let body_start = header_end + 4;
    while buffer.len() < body_start + length {
        let mut chunk = [0u8; 2048];
        let count = stream.read(&mut chunk).await.unwrap_or(0);
        if count == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..count]);
    }
    (
        head,
        String::from_utf8_lossy(&buffer[body_start..(body_start + length).min(buffer.len())])
            .into_owned(),
    )
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
