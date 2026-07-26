use tokio::io::AsyncWrite;
use tokio::sync::broadcast;

use super::mux::FlvMuxer;
use crate::{
    Result,
    http::{HttpRequest, ResponseWriter},
    media::{AccessUnit, Codec, MediaDescription},
};

/// Serve a live FLV stream over HTTP chunked response.
pub async fn serve_flv_stream<W: AsyncWrite + Unpin>(
    _req: HttpRequest,
    mut writer: ResponseWriter<W>,
    description: MediaDescription,
    mut receiver: broadcast::Receiver<AccessUnit>,
) -> Result<ResponseWriter<W>> {
    let mut muxer = FlvMuxer::new(&description);

    // Detect video codec from description for sequence header building
    let video_codec = description
        .tracks
        .iter()
        .find(|t| t.kind == crate::media::MediaKind::Video)
        .map(|t| t.codec);

    let mut video_seq_pending = video_codec.is_some();
    let mut audio_seq_pending = description.tracks.iter().any(|t| t.codec == Codec::Aac);

    // Send FLV header
    let flv_header = muxer.flv_header();
    writer.write_headers(200, &[
        ("Content-Type", "video/x-flv"),
        ("Transfer-Encoding", "chunked"),
        ("Cache-Control", "no-cache, no-store"),
        ("Connection", "close"),
        ("Access-Control-Allow-Origin", "*"),
    ]).await?;
    writer.write_chunk(&flv_header).await?;
    writer.flush().await?;

    // Stream loop
    loop {
        let unit = match receiver.recv().await {
            Ok(u) => u,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::debug!("http-flv subscriber lagged {n} frames");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };

        // On first video keyframe, build sequence header
        let is_video = matches!(unit.codec, Codec::H264 | Codec::H265);
        if video_seq_pending && unit.is_keyframe && is_video {
            let seq = match unit.codec {
                Codec::H264 => FlvMuxer::build_avc_sequence_header(&unit.data),
                Codec::H265 => FlvMuxer::build_h265_sequence_header(&unit.data),
                _ => None,
            };
            if let Some(seq) = seq {
                muxer.set_video_sequence_header(seq);
            }
            video_seq_pending = false;
        }
        if audio_seq_pending && unit.codec == Codec::Aac {
            audio_seq_pending = false;
        }

        let tags = muxer.encode(&unit);
        if !tags.is_empty() {
            if let Err(e) = writer.write_chunk(&tags).await {
                tracing::debug!("http-flv write error: {e}");
                break;
            }
            if let Err(e) = writer.flush().await {
                tracing::debug!("http-flv flush error: {e}");
                break;
            }
        }
    }

    Ok(writer)
}
