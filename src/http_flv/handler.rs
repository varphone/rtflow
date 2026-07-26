use tokio::io::AsyncWrite;
use tokio::sync::broadcast;

use super::mux::FlvMuxer;
use crate::{
    Result,
    http::{HttpRequest, ResponseWriter},
    media::{AccessUnit, Codec, MediaDescription},
};

/// Serve a live FLV stream over HTTP.
pub async fn serve_flv_stream<W: AsyncWrite + Unpin>(
    _req: HttpRequest,
    mut writer: ResponseWriter<W>,
    description: MediaDescription,
    mut receiver: broadcast::Receiver<AccessUnit>,
    announce_fmtp: std::collections::HashMap<u8, std::sync::Arc<str>>,
) -> Result<ResponseWriter<W>> {
    let mut muxer = FlvMuxer::new(&description);

    let video_codec = description.tracks.iter().find(|t| t.kind == crate::media::MediaKind::Video).map(|t| t.codec);
    let mut video_seq_pending = video_codec.is_some();
    let mut audio_seq_pending = description.tracks.iter().any(|t| t.codec == Codec::Aac);

    // Extract sequence headers from ANNOUNCE fmtp for H.265
    if video_seq_pending {
        for track in &description.tracks {
            if matches!(track.codec, Codec::H264 | Codec::H265) {
                let effective = track.fmtp.as_deref().or_else(|| announce_fmtp.get(&track.id).map(|v| v.as_ref()));
                if let Some(fmtp) = effective {
                    let seq = match track.codec {
                        Codec::H264 => {
                            let (s,p) = FlvMuxer::parse_h264_sprop_from_fmtp(fmtp);
                            if let (Some(s),Some(p)) = (s,p) { FlvMuxer::build_avc_sequence_header_from_nals(&s,&p) } else { None }
                        }
                        Codec::H265 => {
                            let (v,s,p) = FlvMuxer::parse_h265_sprop_from_fmtp(fmtp);
                            if let (Some(v),Some(s),Some(p)) = (v,s,p) { FlvMuxer::build_h265_sequence_header_from_nals(&v,&s,&p) } else { None }
                        }
                        _ => None,
                    };
                    if let Some(seq) = seq { muxer.set_video_sequence_header(seq); video_seq_pending = false; }
                }
            }
        }
    }

    writer.write_headers(200, &[
        ("Content-Type", "video/x-flv"),
        ("Cache-Control", "no-cache, no-store"),
        ("Connection", "close"),
        ("Access-Control-Allow-Origin", "*"),
    ]).await?;
    writer.write_raw(&muxer.flv_header()).await?;
    writer.flush().await?;

    loop {
        let unit = match receiver.recv().await {
            Ok(u) => u,
            Err(broadcast::error::RecvError::Lagged(n)) => { tracing::debug!("http-flv lagged {n}"); continue; }
            Err(broadcast::error::RecvError::Closed) => break,
        };
        let is_video = matches!(unit.codec, Codec::H264 | Codec::H265);
        if video_seq_pending && unit.is_keyframe && is_video {
            let seq = match unit.codec {
                Codec::H264 => FlvMuxer::build_avc_sequence_header(&unit.data),
                Codec::H265 => FlvMuxer::build_h265_sequence_header(&unit.data),
                _ => None,
            };
            if let Some(seq) = seq { muxer.set_video_sequence_header(seq); }
            video_seq_pending = false;
        }
        if audio_seq_pending && unit.codec == Codec::Aac { audio_seq_pending = false; }

        let tags = muxer.encode(&unit);
        if !tags.is_empty() {
            if let Err(e) = writer.write_raw(&tags).await { tracing::debug!("flv write: {e}"); break; }
            if let Err(e) = writer.flush().await { tracing::debug!("flv flush: {e}"); break; }
        }
    }
    Ok(writer)
}
