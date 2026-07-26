use tokio::io::AsyncWrite;
use tokio::sync::broadcast;

use super::mux::FlvMuxer;
use crate::{
    Result,
    http::{HttpRequest, ResponseWriter},
    media::{AccessUnit, MediaDescription},
};

/// Serve a live FLV stream over HTTP chunked response.
/// Returns the writer back for potential keep-alive reuse.
pub async fn serve_flv_stream<W: AsyncWrite + Unpin>(
    _req: HttpRequest,
    mut writer: ResponseWriter<W>,
    description: MediaDescription,
    mut receiver: broadcast::Receiver<AccessUnit>,
) -> Result<ResponseWriter<W>> {
    let mut muxer = FlvMuxer::new(&description);

    // Try to extract sequence headers from initial frames
    let mut video_seq_pending = true;
    let mut audio_seq_pending = true;

    // Send FLV header
    let flv_header = muxer.flv_header();

    writer
        .write_chunked_headers("video/x-flv", &[])
        .await?;
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
        if video_seq_pending && unit.is_keyframe {
            match unit.codec {
                crate::media::Codec::H264 | crate::media::Codec::H265 => {
                    if let Some(seq) = FlvMuxer::build_avc_sequence_header(&unit.data) {
                        muxer.set_avc_sequence_header(seq);
                    }
                    video_seq_pending = false;
                }
                _ => {}
            }
        }
        if audio_seq_pending && unit.codec == crate::media::Codec::Aac {
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
