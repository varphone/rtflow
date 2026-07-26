#[cfg(feature = "http-flv")]
#[cfg(feature = "file-storage")]
#[tokio::test]
async fn file_source_h265_to_http_flv() {
    use rtflow::http::{HttpConfig, HttpServer, Method};
    use rtflow::http_flv::create_flv_handler;
    use rtflow::{FileSource, FileSourceOptions, MountConfig, RtspServer, ServerConfig};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let file_path = "/home/devel/视频/video-h265.mkv";
    if !std::path::Path::new(file_path).exists() {
        eprintln!("Skipping test: {file_path} not found");
        return;
    }

    let source = FileSource::open_async(
        file_path,
        FileSourceOptions {
            loop_mode: rtflow::LoopMode::Forever,
            playback_rate: 1.0,
            pause_when_idle: false,
            prefetch: 8,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let description = source.description().clone();
    println!("description: {description:#?}");

    // RTSP server
    let rtsp_server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = rtsp_server
        .register_mount(
            std::sync::Arc::from("/test"),
            MountConfig::Local {
                description: description.clone(),
            },
        )
        .await
        .unwrap();

    let publisher = mount.publisher().unwrap();

    // HTTP server
    let http_config = HttpConfig::new("127.0.0.1:0".parse().unwrap());
    let mut http_server = HttpServer::bind(http_config).await.unwrap();
    let http_addr = http_server.local_addr();

    let handler = create_flv_handler(&mount);
    http_server.add_route(Method::Get, "/live/test.flv", handler).await;
    http_server.run().await.unwrap();

    // Start file playback in background
    let _playback = source.play(publisher);

    // Give it a moment to start
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Connect to HTTP-FLV
    let mut stream = tokio::net::TcpStream::connect(http_addr).await.unwrap();
    stream
        .write_all(b"GET /live/test.flv HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();

    // Read response
    let mut buf = vec![0u8; 65536];
    let mut total = Vec::new();
    let mut read_count = 0;
    loop {
        match tokio::time::timeout(Duration::from_millis(2000), stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                total.extend_from_slice(&buf[..n]);
                read_count += 1;
                println!("Read {n} bytes (total {})", total.len());
                if read_count >= 10 || total.len() > 500_000 {
                    break;
                }
            }
            Ok(Err(e)) => {
                println!("Read error: {e}");
                break;
            }
            Err(_) => {
                println!("Read timeout after {} reads, {} total bytes", read_count, total.len());
                break;
            }
        }
    }

    println!("Total bytes received: {}", total.len());

    // Find HTTP header boundary
    if let Some(body_start) = total.windows(4).position(|w| w == b"\r\n\r\n") {
        let body_start = body_start + 4;
        let flv_data = &total[body_start..];
        println!("FLV data starts at offset {body_start}, length {}", flv_data.len());

        // Check FLV header
        if flv_data.len() >= 13 {
            assert_eq!(&flv_data[..3], b"FLV", "FLV signature");
            let flags = flv_data[4];
            println!("FLV flags: {flags:#04b} (video={}, audio={})", flags & 1 != 0, flags & 4 != 0);

            // Parse first few tags
            let mut pos = 13; // after FLV header + PreviousTagSize0
            let mut tag_count = 0;
            while pos + 15 < flv_data.len() && tag_count < 10 {
                let tag_type = flv_data[pos];
                let data_size = ((flv_data[pos + 1] as usize) << 16)
                    | ((flv_data[pos + 2] as usize) << 8)
                    | (flv_data[pos + 3] as usize);
                let timestamp = ((flv_data[pos + 4] as u32))
                    | ((flv_data[pos + 5] as u32) << 8)
                    | ((flv_data[pos + 6] as u32) << 16)
                    | ((flv_data[pos + 7] as u32) << 24);

                let body_start = pos + 11;
                let body = &flv_data[body_start..body_start + data_size];

                let tag_name = match tag_type {
                    0x08 => "audio",
                    0x09 => "video",
                    0x12 => "script",
                    _ => "unknown",
                };

                if tag_type == 0x09 {
                    let first_byte = body[0];
                    let is_ex = (first_byte & 0x80) != 0;
                    let frame_type = (first_byte >> 4) & 0x07;
                    let packet_type = first_byte & 0x0f;
                    if is_ex {
                        let fourcc = &body[1..5];
                        let fourcc_str = std::str::from_utf8(fourcc).unwrap_or("???");
                        println!(
                            "  Tag #{tag_count}: {tag_name}, size={data_size}, ts={timestamp}, "
                        );
                        println!(
                            "    IsEx=1, FrameType={frame_type}, PacketType={packet_type}, FourCC={fourcc_str}"
                        );
                    } else {
                        // Traditional AVC
                        let avc_packet_type = body[1];
                        let composition_time = ((body[2] as i32) << 16)
                            | ((body[3] as i32) << 8)
                            | (body[4] as i32);
                        println!(
                            "  Tag #{tag_count}: {tag_name}, size={data_size}, ts={timestamp}, "
                        );
                        println!(
                            "    FrameType={frame_type}, CodecId={}, AVCPacketType={avc_packet_type}, CTS={composition_time}",
                            first_byte & 0x0f
                        );
                    }
                } else {
                    println!("  Tag #{tag_count}: {tag_name}, size={data_size}, ts={timestamp}");
                }

                pos = body_start + data_size + 4; // +4 for PreviousTagSize
                tag_count += 1;
            }
        }
    } else {
        println!("No HTTP body found in response!");
        println!("Response:\n{}", String::from_utf8_lossy(&total[..total.len().min(500)]));
    }

    http_server.shutdown();
}
