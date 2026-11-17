//! Minimal ONVIF discovery and stream URI resolution.
//!
//! WS-Discovery finds NetworkVideoTransmitter devices on the LAN; SOAP calls
//! then read device information, the media service endpoint and each profile's
//! RTSP stream URI. HTTP Basic and Digest authentication are supported.
//!
//! The crate is standalone; the `rtflow` facade re-exports it as
//! `rtflow::onvif` behind the `onvif` feature, and `rtflow-server` enables that
//! feature with `--features onvif`.
use base64::Engine as _;
use md5::{Digest, Md5};
use quick_xml::Reader;
use quick_xml::events::Event;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::net::UdpSocket;

const DISCOVERY_ADDR: &str = "239.255.255.250:3702";
const DEVICE_INFO_ACTION: &str = "http://www.onvif.org/ver10/device/wsdl/GetDeviceInformation";
const CAPABILITIES_ACTION: &str = "http://www.onvif.org/ver10/device/wsdl/GetCapabilities";
const GET_PROFILES_ACTION: &str = "http://www.onvif.org/ver10/media/wsdl/GetProfiles";
const GET_STREAM_URI_ACTION: &str = "http://www.onvif.org/ver10/media/wsdl/GetStreamUri";

#[derive(Clone, Debug, Default, Serialize)]
pub struct OnvifDevice {
    pub endpoint: String,
    pub xaddr: String,
    pub types: String,
    pub scopes: String,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DeviceInformation {
    pub manufacturer: String,
    pub model: String,
    pub firmware_version: String,
    pub serial_number: String,
    pub hardware_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct OnvifProfile {
    pub token: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OnvifDetails {
    pub device: DeviceInformation,
    pub media_xaddr: String,
    pub profiles: Vec<OnvifProfile>,
}

#[derive(Clone, Debug, Default)]
struct Credentials {
    username: String,
    password: String,
}

/// Discover ONVIF NetworkVideoTransmitter devices via WS-Discovery.
pub async fn discover(timeout: Duration) -> Result<Vec<OnvifDevice>, String> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|error| format!("bind discovery socket: {error}"))?;
    let _ = socket.set_multicast_ttl_v4(2);
    let probe = probe_message();
    socket
        .send_to(probe.as_bytes(), DISCOVERY_ADDR)
        .await
        .map_err(|error| format!("send WS-Discovery probe: {error}"))?;

    let deadline = tokio::time::Instant::now() + timeout;
    let mut buffer = vec![0u8; 65_535];
    let mut devices = Vec::new();
    let mut seen = HashSet::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, socket.recv_from(&mut buffer)).await {
            Ok(Ok((len, _peer))) => {
                let xml = String::from_utf8_lossy(&buffer[..len]);
                for device in parse_probe_matches(&xml) {
                    let key = if device.xaddr.is_empty() {
                        device.endpoint.clone()
                    } else {
                        device.xaddr.clone()
                    };
                    if !key.is_empty() && seen.insert(key) {
                        devices.push(device);
                    }
                }
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
    devices.sort_by(|left, right| left.xaddr.cmp(&right.xaddr));
    Ok(devices)
}

/// Read device information, profiles and RTSP URIs from one ONVIF device.
pub async fn inspect(
    xaddr: &str,
    username: Option<String>,
    password: Option<String>,
) -> Result<OnvifDetails, String> {
    let xaddr = xaddr.trim();
    if !xaddr.starts_with("http://") && !xaddr.starts_with("https://") {
        return Err("ONVIF xaddr must be an http(s) URL".into());
    }
    let credentials = username
        .filter(|value| !value.is_empty())
        .map(|username| Credentials {
            username,
            password: password.unwrap_or_default(),
        });
    let client = SoapClient::new()?;

    let info_xml = client
        .call(
            xaddr,
            DEVICE_INFO_ACTION,
            "<tds:GetDeviceInformation xmlns:tds=\"http://www.onvif.org/ver10/device/wsdl\"/>",
            credentials.as_ref(),
        )
        .await?;
    let device = parse_device_information(&info_xml);

    let media_xaddr = match client
        .call(
            xaddr,
            CAPABILITIES_ACTION,
            "<tds:GetCapabilities xmlns:tds=\"http://www.onvif.org/ver10/device/wsdl\"><tds:Category>Media</tds:Category></tds:GetCapabilities>",
            credentials.as_ref(),
        )
        .await
    {
        Ok(xml) => parse_media_xaddr(&xml).unwrap_or_else(|| derive_media_xaddr(xaddr)),
        Err(_) => derive_media_xaddr(xaddr),
    };

    let profiles_xml = client
        .call(
            &media_xaddr,
            GET_PROFILES_ACTION,
            "<trt:GetProfiles xmlns:trt=\"http://www.onvif.org/ver10/media/wsdl\"/>",
            credentials.as_ref(),
        )
        .await?;
    let mut profiles = parse_profiles(&profiles_xml);
    for profile in &mut profiles {
        let body = format!(
            "<trt:GetStreamUri xmlns:trt=\"http://www.onvif.org/ver10/media/wsdl\" xmlns:tt=\"http://www.onvif.org/ver10/schema\"><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream><tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>{}</trt:ProfileToken></trt:GetStreamUri>",
            xml_escape(&profile.token)
        );
        match client
            .call(
                &media_xaddr,
                GET_STREAM_URI_ACTION,
                &body,
                credentials.as_ref(),
            )
            .await
        {
            Ok(xml) => profile.uri = parse_stream_uri(&xml),
            Err(error) => profile.error = Some(error),
        }
    }

    Ok(OnvifDetails {
        device,
        media_xaddr,
        profiles,
    })
}

struct SoapClient {
    client: reqwest::Client,
}

impl SoapClient {
    fn new() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| format!("build ONVIF HTTP client: {error}"))?;
        Ok(Self { client })
    }

    async fn call(
        &self,
        url: &str,
        action: &str,
        body: &str,
        credentials: Option<&Credentials>,
    ) -> Result<String, String> {
        let envelope = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body>{body}</s:Body></s:Envelope>"
        );
        let response = self
            .client
            .post(url)
            .header("Content-Type", "application/soap+xml; charset=utf-8")
            .header("SOAPAction", format!("\"{action}\""))
            .body(envelope.clone())
            .send()
            .await
            .map_err(|error| format!("ONVIF request {action} failed: {error}"))?;
        if response.status().is_success() {
            return response.text().await.map_err(|error| error.to_string());
        }
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Err(format!(
                "ONVIF {action} returned HTTP {}",
                response.status()
            ));
        }
        let challenge = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let Some(credentials) = credentials else {
            return Err("ONVIF device requires authentication".into());
        };
        let authorization = authorization_header(&challenge, "POST", url, credentials)
            .ok_or_else(|| "unsupported ONVIF authentication challenge".to_string())?;
        let response = self
            .client
            .post(url)
            .header("Content-Type", "application/soap+xml; charset=utf-8")
            .header("SOAPAction", format!("\"{action}\""))
            .header(reqwest::header::AUTHORIZATION, authorization)
            .body(envelope)
            .send()
            .await
            .map_err(|error| format!("ONVIF authenticated request {action} failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "ONVIF {action} returned HTTP {} after authentication",
                response.status()
            ));
        }
        response.text().await.map_err(|error| error.to_string())
    }
}

fn authorization_header(
    challenge: &str,
    method: &str,
    url: &str,
    credentials: &Credentials,
) -> Option<String> {
    let (scheme, rest) = challenge.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("basic") {
        let token = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", credentials.username, credentials.password));
        return Some(format!("Basic {token}"));
    }
    if !scheme.eq_ignore_ascii_case("digest") {
        return None;
    }
    let params = parse_auth_params(rest);
    let realm = params.get("realm")?;
    let nonce = params.get("nonce")?;
    let uri = request_uri(url);
    let qop = params.get("qop").map(String::as_str).unwrap_or("");
    let use_qop = qop
        .split(',')
        .any(|value| value.trim().eq_ignore_ascii_case("auth"));
    let ha1 = md5_hex(format!(
        "{}:{realm}:{}",
        credentials.username, credentials.password
    ));
    let ha2 = md5_hex(format!("{method}:{uri}"));
    let cnonce = random_hex(8);
    let nc = "00000001";
    let response = if use_qop {
        md5_hex(format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"))
    } else {
        md5_hex(format!("{ha1}:{nonce}:{ha2}"))
    };
    let mut header = format!(
        "Digest username=\"{}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\"",
        credentials.username
    );
    if use_qop {
        header.push_str(&format!(", qop=auth, nc={nc}, cnonce=\"{cnonce}\""));
    }
    if let Some(opaque) = params.get("opaque") {
        header.push_str(&format!(", opaque=\"{opaque}\""));
    }
    Some(header)
}

fn request_uri(url: &str) -> String {
    url::Url::parse(url)
        .map(|url| {
            let mut path = url.path().to_owned();
            if path.is_empty() {
                path.push('/');
            }
            if let Some(query) = url.query() {
                path.push('?');
                path.push_str(query);
            }
            path
        })
        .unwrap_or_else(|_| "/".into())
}

fn parse_auth_params(value: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut parts = Vec::new();
    for ch in value.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                current.push(ch);
            }
            ',' if !quoted => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    for part in parts {
        if let Some((key, raw)) = part.split_once('=') {
            params.insert(
                key.trim().to_ascii_lowercase(),
                raw.trim().trim_matches('"').to_string(),
            );
        }
    }
    params
}

fn md5_hex(value: impl AsRef<[u8]>) -> String {
    let digest = Md5::digest(value.as_ref());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    let _ = getrandom::getrandom(&mut buffer);
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn probe_message() -> String {
    let message_id = format!(
        "uuid:{}-{}-{}-{}-{}",
        random_hex(4),
        random_hex(2),
        random_hex(2),
        random_hex(2),
        random_hex(6)
    );
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:a=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" xmlns:d=\"http://schemas.xmlsoap.org/ws/2005/04/discovery\" xmlns:dn=\"http://www.onvif.org/ver10/network/wsdl\"><s:Header><a:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</a:Action><a:MessageID>{message_id}</a:MessageID><a:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</a:To></s:Header><s:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></s:Body></s:Envelope>"
    )
}

fn parse_probe_matches(xml: &str) -> Vec<OnvifDevice> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut current: Option<OnvifDevice> = None;
    let mut devices = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let name = event.local_name().as_ref().to_vec();
                if name == b"ProbeMatch" {
                    current = Some(OnvifDevice::default());
                }
                stack.push(name);
            }
            Ok(Event::Text(text)) => {
                let Some(device) = current.as_mut() else {
                    continue;
                };
                let Some(name) = stack.last() else {
                    continue;
                };
                let value = text.unescape().unwrap_or_default().trim().to_owned();
                if value.is_empty() {
                    continue;
                }
                match name.as_slice() {
                    b"Address" => device.endpoint = value,
                    b"XAddrs" => {
                        device.xaddr = value.split_whitespace().next().unwrap_or("").to_string()
                    }
                    b"Types" => device.types = value,
                    b"Scopes" => device.scopes = value,
                    _ => {}
                }
            }
            Ok(Event::End(event)) => {
                let name = event.local_name().as_ref().to_vec();
                if name == b"ProbeMatch"
                    && let Some(mut device) = current.take()
                {
                    device.name = scope_name(&device.scopes);
                    if !device.xaddr.is_empty() || !device.endpoint.is_empty() {
                        devices.push(device);
                    }
                }
                stack.pop();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    devices
}

fn scope_name(scopes: &str) -> Option<String> {
    scopes
        .split_whitespace()
        .find_map(|scope| scope.strip_prefix("onvif://www.onvif.org/name/"))
        .map(percent_decode)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &value[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_device_information(xml: &str) -> DeviceInformation {
    let values = collect_texts(xml);
    DeviceInformation {
        manufacturer: values.get("Manufacturer").cloned().unwrap_or_default(),
        model: values.get("Model").cloned().unwrap_or_default(),
        firmware_version: values.get("FirmwareVersion").cloned().unwrap_or_default(),
        serial_number: values.get("SerialNumber").cloned().unwrap_or_default(),
        hardware_id: values.get("HardwareId").cloned().unwrap_or_default(),
    }
}

fn parse_media_xaddr(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut in_media = false;
    let mut result = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let name = event.local_name().as_ref().to_vec();
                if name == b"Media" {
                    in_media = true;
                }
                stack.push(name);
            }
            Ok(Event::Text(text)) => {
                if in_media
                    && stack.last().is_some_and(|name| name == b"XAddr")
                    && let Ok(value) = text.unescape()
                {
                    let value = value.trim();
                    if !value.is_empty() {
                        result = Some(value.to_string());
                        break;
                    }
                }
            }
            Ok(Event::End(event)) => {
                if event.local_name().as_ref() == b"Media" {
                    in_media = false;
                }
                stack.pop();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    result
}

fn parse_profiles(xml: &str) -> Vec<OnvifProfile> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut current: Option<OnvifProfile> = None;
    let mut profiles = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let name = event.local_name().as_ref().to_vec();
                if name == b"Profiles" {
                    let token = event
                        .attributes()
                        .flatten()
                        .find(|attribute| attribute.key.as_ref() == b"token")
                        .map(|attribute| String::from_utf8_lossy(&attribute.value).into_owned())
                        .unwrap_or_default();
                    current = Some(OnvifProfile {
                        token,
                        name: String::new(),
                        uri: None,
                        error: None,
                    });
                }
                stack.push(name);
            }
            Ok(Event::Text(text)) => {
                if let Some(profile) = current.as_mut()
                    && stack.last().is_some_and(|name| name == b"Name")
                    && let Ok(value) = text.unescape()
                {
                    let value = value.trim();
                    if !value.is_empty() && profile.name.is_empty() {
                        profile.name = value.to_string();
                    }
                }
            }
            Ok(Event::End(event)) => {
                if event.local_name().as_ref() == b"Profiles"
                    && let Some(profile) = current.take()
                {
                    profiles.push(profile);
                }
                stack.pop();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    profiles
}

fn parse_stream_uri(xml: &str) -> Option<String> {
    collect_texts(xml).get("Uri").cloned()
}

fn collect_texts(xml: &str) -> HashMap<String, String> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut values = HashMap::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => stack.push(event.local_name().as_ref().to_vec()),
            Ok(Event::Text(text)) => {
                let Some(name) = stack.last() else {
                    continue;
                };
                if let Ok(value) = text.unescape() {
                    let value = value.trim();
                    if !value.is_empty() {
                        values
                            .entry(String::from_utf8_lossy(name).into_owned())
                            .or_insert_with(|| value.to_string());
                    }
                }
            }
            Ok(Event::End(_)) => {
                stack.pop();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    values
}

fn derive_media_xaddr(device_xaddr: &str) -> String {
    let Ok(mut url) = url::Url::parse(device_xaddr) else {
        return device_xaddr.to_string();
    };
    if url.path().ends_with("/device_service") {
        url.set_path(&url.path().replace("/device_service", "/media_service"));
    } else {
        url.set_path("/onvif/media_service");
    }
    url.set_query(None);
    url.to_string()
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROBE_MATCHES: &str = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery">
<s:Body><d:ProbeMatches><d:ProbeMatch>
<a:EndpointReference><a:Address>urn:uuid:1234</a:Address></a:EndpointReference>
<d:Types>dn:NetworkVideoTransmitter</d:Types>
<d:Scopes>onvif://www.onvif.org/name/Front%20Door onvif://www.onvif.org/location/lobby</d:Scopes>
<d:XAddrs>http://192.168.1.10/onvif/device_service</d:XAddrs>
</d:ProbeMatch></d:ProbeMatches></s:Body></s:Envelope>"#;

    #[test]
    fn parses_probe_matches() {
        let devices = parse_probe_matches(PROBE_MATCHES);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].endpoint, "urn:uuid:1234");
        assert_eq!(devices[0].xaddr, "http://192.168.1.10/onvif/device_service");
        assert_eq!(devices[0].name.as_deref(), Some("Front Door"));
        assert!(devices[0].types.contains("NetworkVideoTransmitter"));
    }

    #[test]
    fn parses_device_capabilities_profiles_and_stream_uri() {
        let info = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tds:GetDeviceInformationResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:Manufacturer>Acme</tds:Manufacturer><tds:Model>Cam 1</tds:Model><tds:FirmwareVersion>1.2.3</tds:FirmwareVersion><tds:SerialNumber>SN1</tds:SerialNumber><tds:HardwareId>HW1</tds:HardwareId></tds:GetDeviceInformationResponse></s:Body></s:Envelope>"#;
        let device = parse_device_information(info);
        assert_eq!(device.manufacturer, "Acme");
        assert_eq!(device.model, "Cam 1");

        let caps = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tds:GetCapabilitiesResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:Capabilities><tt:Media xmlns:tt="http://www.onvif.org/ver10/schema"><tt:XAddr>http://192.168.1.10/onvif/media_service</tt:XAddr></tt:Media></tds:Capabilities></tds:GetCapabilitiesResponse></s:Body></s:Envelope>"#;
        assert_eq!(
            parse_media_xaddr(caps).as_deref(),
            Some("http://192.168.1.10/onvif/media_service")
        );

        let profiles = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><trt:GetProfilesResponse xmlns:trt="http://www.onvif.org/ver10/media/wsdl"><trt:Profiles token="main"><tt:Name xmlns:tt="http://www.onvif.org/ver10/schema">Main</tt:Name></trt:Profiles><trt:Profiles token="sub"><tt:Name xmlns:tt="http://www.onvif.org/ver10/schema">Sub</tt:Name></trt:Profiles></trt:GetProfilesResponse></s:Body></s:Envelope>"#;
        let profiles = parse_profiles(profiles);
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].token, "main");
        assert_eq!(profiles[0].name, "Main");

        let uri = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><trt:GetStreamUriResponse xmlns:trt="http://www.onvif.org/ver10/media/wsdl"><trt:MediaUri><tt:Uri xmlns:tt="http://www.onvif.org/ver10/schema">rtsp://192.168.1.10/stream1</tt:Uri></trt:MediaUri></trt:GetStreamUriResponse></s:Body></s:Envelope>"#;
        assert_eq!(
            parse_stream_uri(uri).as_deref(),
            Some("rtsp://192.168.1.10/stream1")
        );
    }

    #[test]
    fn digest_authorization_is_built() {
        let credentials = Credentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        let header = authorization_header(
            r#"Digest realm="camera", nonce="abc", qop="auth""#,
            "POST",
            "http://192.168.1.10/onvif/device_service",
            &credentials,
        )
        .unwrap();
        assert!(header.starts_with("Digest username=\"admin\""));
        assert!(header.contains("realm=\"camera\""));
        assert!(header.contains("qop=auth"));
        let basic = authorization_header(
            "Basic realm=\"camera\"",
            "POST",
            "http://192.168.1.10/onvif/device_service",
            &credentials,
        )
        .unwrap();
        assert_eq!(basic, "Basic YWRtaW46c2VjcmV0");
    }
}
