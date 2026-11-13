use super::*;
#[test]
fn static_g711_and_control_resolution() {
    let parsed = parse(b"v=0\r\ns=g711\r\nm=audio 0 RTP/AVP 8\r\na=control:audio\r\n").unwrap();
    assert_eq!(parsed.tracks[0].codec, Codec::Pcma);
    assert_eq!(parsed.tracks[0].clock_rate, 8_000);
    assert_eq!(
        control_uri("rtsp://camera/live", "audio").unwrap(),
        "rtsp://camera/live/audio"
    );
    assert_eq!(
        control_uri("rtsp://camera/live/", "rtsp://other/track").unwrap(),
        "rtsp://other/track"
    );
}

#[test]
fn vp8_vp9_rtpmap_parse() {
    let sdp =
        b"v=0\r\ns=vp\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 VP8/90000\r\na=control:trackID=0\r\n";
    assert_eq!(parse(sdp).unwrap().tracks[0].codec, Codec::Vp8);
    let sdp =
        b"v=0\r\ns=vp\r\nm=video 0 RTP/AVP 97\r\na=rtpmap:97 VP9/90000\r\na=control:trackID=0\r\n";
    assert_eq!(parse(sdp).unwrap().tracks[0].codec, Codec::Vp9);
}

#[test]
fn track_limit_is_enforced() {
    let mut sdp = String::from("v=0\r\ns=many\r\n");
    for i in 0..3 {
        sdp.push_str(&format!(
            "m=video 0 RTP/AVP {}\r\na=rtpmap:{} H264/90000\r\na=control:trackID={}\r\n",
            96 + i,
            96 + i,
            i
        ));
    }
    assert!(parse_with_limits(sdp.as_bytes(), SdpLimits { max_tracks: 2 }).is_err());
    assert_eq!(
        parse_with_limits(sdp.as_bytes(), SdpLimits { max_tracks: 3 })
            .unwrap()
            .tracks
            .len(),
        3
    );
}

#[test]
fn sdp_attributes_and_ipv6_encoding() {
    let input = b"v=0\r\ns=attrs\r\nm=video 0 RTP/AVP 96 97\r\na=rtpmap:97 H265/90000\r\na=framerate:25\r\na=control:trackID=0\r\n";
    let parsed = parse(input).unwrap();
    assert_eq!(parsed.tracks[0].codec, Codec::H265);
    assert_eq!(parsed.tracks[0].payload_type, 97);
    assert_eq!(parsed.tracks[0].frame_rate, Some((25, 1)));
    let encoded = encode_to(&parsed, Some("::1".parse().unwrap()));
    assert!(encoded.contains("c=IN IP6 ::"));
    assert!(encoded.contains("a=framerate:25"));
}

#[test]
fn savp_profile_and_ptime_are_tolerated() {
    let input = b"v=0\r\ns=secure\r\nm=video 0 RTP/SAVP 96\r\na=rtpmap:96 H264/90000\r\na=ptime:20\r\na=control:trackID=0\r\n";
    let parsed = parse(input).unwrap();
    assert_eq!(parsed.tracks[0].codec, Codec::H264);
}

#[test]
fn direction_ptime_rtcp_and_bandwidth_round_trip() {
    let input = b"v=0\r\ns=attrs\r\nm=audio 0 RTP/AVP 96\r\na=rtpmap:96 PCMA/8000/1\r\nb=AS:128\r\na=sendonly\r\na=ptime:20\r\na=rtcp:5005 IN IP4 192.0.2.9\r\na=control:trackID=0\r\n";
    let parsed = parse(input).unwrap();
    let track = &parsed.tracks[0];
    assert_eq!(track.direction, Some(SdpDirection::SendOnly));
    assert_eq!(track.ptime, Some(20));
    assert_eq!(
        track.rtcp,
        Some(RtcpAddress {
            port: 5005,
            address: Some("192.0.2.9".parse().unwrap())
        })
    );
    assert_eq!(track.bandwidth_as, Some(128));
    let encoded = encode(&parsed);
    assert!(encoded.contains("b=AS:128"), "{encoded}");
    assert!(encoded.contains("a=sendonly"), "{encoded}");
    assert!(encoded.contains("a=ptime:20"), "{encoded}");
    assert!(
        encoded.contains("a=rtcp:5005 IN IP4 192.0.2.9"),
        "{encoded}"
    );
    let reparsed = parse(encoded.as_bytes()).unwrap();
    assert_eq!(reparsed.tracks[0].direction, Some(SdpDirection::SendOnly));
}

#[test]
fn mp3_and_g726_names_round_trip() {
    for (codec, name) in [
        (Codec::Mp3, "MPA"),
        (Codec::G726_16, "G726-16"),
        (Codec::G726_24, "G726-24"),
        (Codec::G726_32, "G726-32"),
        (Codec::G726_40, "G726-40"),
    ] {
        let track = TrackDescription {
            id: 0,
            kind: MediaKind::Audio,
            codec,
            clock_rate: if codec == Codec::Mp3 { 90_000 } else { 8_000 },
            channels: Some(1),
            payload_type: 96,
            fmtp: None,
            control: Arc::from("trackID=0"),
            width: None,
            height: None,
            frame_rate: None,
            #[cfg(feature = "srtp")]
            crypto: None,
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
        };
        let description = MediaDescription {
            session_name: Arc::from("audio"),
            tracks: vec![track],
            #[cfg(feature = "multicast")]
            connection: None,
        };
        let encoded = encode(&description);
        assert!(encoded.contains(name), "{encoded}");
        let parsed = parse(encoded.as_bytes()).unwrap();
        assert_eq!(parsed.tracks[0].codec, codec);
    }
}

#[test]
fn legacy_and_av1_video_names_round_trip() {
    for (codec, name) in [
        (Codec::Av1, "AV1"),
        (Codec::Jpeg, "JPEG"),
        (Codec::H263, "H263"),
        (Codec::Mpeg4, "MP4V-ES"),
    ] {
        let track = TrackDescription {
            id: 0,
            kind: MediaKind::Video,
            codec,
            clock_rate: 90_000,
            channels: None,
            payload_type: 96,
            fmtp: None,
            control: Arc::from("trackID=0"),
            width: None,
            height: None,
            frame_rate: None,
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            #[cfg(feature = "srtp")]
            crypto: None,
        };
        let description = MediaDescription {
            session_name: Arc::from("video"),
            tracks: vec![track],
            #[cfg(feature = "multicast")]
            connection: None,
        };
        let encoded = encode(&description);
        assert!(encoded.contains(name), "{encoded}");
        assert_eq!(parse(encoded.as_bytes()).unwrap().tracks[0].codec, codec);
    }
}

#[test]
fn two_track_round_trip() {
    let input=b"v=0\r\ns=av\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:trackID=0\r\nm=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000/1\r\na=control:trackID=1\r\n";
    let parsed = parse(input).unwrap();
    assert_eq!(parsed.tracks.len(), 2);
    assert_eq!(parse(encode(&parsed).as_bytes()).unwrap().tracks.len(), 2);
}

#[test]
fn parse_ffmpeg_av1_announce() {
    let text = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=No Name\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\na=tool:libavformat 63.20.101\r\nm=video 0 RTP/AVP 96\r\na=framerate:10\r\na=rtpmap:96 AV1/90000\r\na=fmtp:96 profile=0;level-idx=0;tier=0\r\na=control:streamid=0\r\n";
    let description = parse(text.as_bytes()).expect("parse AV1 ANNOUNCE");
    assert_eq!(description.tracks.len(), 1);
    assert_eq!(description.tracks[0].codec, Codec::Av1);
    assert_eq!(description.tracks[0].clock_rate, 90_000);
    assert_eq!(description.tracks[0].payload_type, 96);
    assert_eq!(
        description.tracks[0].fmtp.as_deref(),
        Some("profile=0;level-idx=0;tier=0")
    );
}

#[cfg(feature = "srtp")]
#[test]
fn crypto_track_is_advertised_as_savp() {
    let track = TrackDescription {
        id: 0,
        kind: MediaKind::Video,
        codec: Codec::H264,
        clock_rate: 90_000,
        channels: None,
        payload_type: 96,
        fmtp: None,
        control: Arc::from("trackID=0"),
        width: None,
        height: None,
        frame_rate: None,
        direction: None,
        ptime: None,
        rtcp: None,
        bandwidth_as: None,
        crypto: Some(crate::media::crypto::SdpCrypto {
            tag: 1,
            crypto_suite: "AES_128_CM_HMAC_SHA1_80".into(),
            key_params: "inline:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            session_params: None,
        }),
    };
    let description = MediaDescription {
        session_name: Arc::from("srtp"),
        tracks: vec![track],
        #[cfg(feature = "multicast")]
        connection: None,
    };
    let encoded = encode(&description);
    assert!(
        encoded.contains("m=video 0 RTP/SAVP 96"),
        "SRTP track must be advertised as SAVP: {encoded}"
    );
    let parsed = parse(encoded.as_bytes()).unwrap();
    assert!(parsed.tracks[0].crypto.is_some());
}
