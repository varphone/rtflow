use tokio::io::AsyncWrite;
use tokio::sync::broadcast;

use super::mux::FlvMuxer;
use crate::{
    Result,
    http::{HttpRequest, ResponseWriter},
    media::{AccessUnit, Codec, MediaDescription},
};

/// Serve a live FLV stream over HTTP.
///
/// Uses raw bytes (no chunked encoding) with `Connection: close`.
/// The stream ends when the client disconnects or the source stops.
pub async fn serve_flv_stream<W: AsyncWrite + Unpin>(
    _req: HttpRequest,
    mut writer: ResponseWriter<W>,
    description: MediaDescription,
    mut receiver: broadcast::Receiver<AccessUnit>,
) -> Result<ResponseWriter<W>> {
    let mut muxer = FlvMuxer::new(&description);

    let video_codec = description
        .tracks
        .iter()
        .find(|t| t.kind == crate::media::MediaKind::Video)
        .map(|t| t.codec);

    let mut video_seq_pending = video_codec.is_some();
    let mut audio_seq_pending = description.tracks.iter().any(|t| t.codec == Codec::Aac);

    // Send HTTP headers — no Transfer-Encoding: chunked.
    // We write raw FLV bytes directly; the stream ends on Connection: close.
    writer.write_headers(200, &[
        ("Content-Type", "video/x-flv"),
        ("Cache-Control", "no-cache, no-store"),
        ("Connection", "close"),
        ("Access-Control-Allow-Origin", "*"),
    ]).await?;
    // Write FLV file header immediately
    writer.write_raw(&muxer.flv_header()).await?;
    writer.flush().await?;

    // Stream loop: receive frames from publisher, encode to FLV, write raw
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
            if let Err(e) = writer.write_raw(&tags).await {
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
