use std::collections::VecDeque;
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
    startup: Vec<AccessUnit>,
    fmtp_source: crate::server::MountHandle,
) -> Result<ResponseWriter<W>> {
    let mut muxer = FlvMuxer::new(&description);

    let video_codec = description
        .tracks
        .iter()
        .find(|t| t.kind == crate::media::MediaKind::Video)
        .map(|t| t.codec);
    let mut video_seq_pending = video_codec.is_some();
    let mut audio_seq_pending = description.tracks.iter().any(|t| t.codec == Codec::Aac);

    // fmtp only changes on ANNOUNCE; take one snapshot and refresh it only
    // while sequence headers are still pending (instead of locking and
    // cloning the map on every frame).
    let mut fmtp_snapshot = fmtp_source.announce_fmtp();
    configure_sequence_headers(
        &mut muxer,
        &description,
        &fmtp_snapshot,
        &fmtp_source,
        &mut video_seq_pending,
        &mut audio_seq_pending,
    );

    // Cached audio may predate the cached video keyframe.  Start both tracks at
    // that keyframe so their normalized timestamps remain monotonic.
    let startup_keyframe_pts = startup
        .iter()
        .find(|unit| matches!(unit.codec, Codec::H264 | Codec::H265) && unit.is_keyframe)
        .map(|unit| unit.pts);

    writer
        .write_headers(
            200,
            &[
                ("Content-Type", "video/x-flv"),
                ("Cache-Control", "no-cache, no-store"),
                ("Connection", "close"),
                ("Access-Control-Allow-Origin", "*"),
            ],
        )
        .await?;
    writer.write_raw(&muxer.flv_header()).await?;
    // onMetaData script tag: legacy Flash-era/Android players need it to
    // bootstrap the stream.
    if let Some(metadata) = muxer.metadata_tag() {
        writer.write_raw(&metadata).await?;
    }
    writer.flush().await?;

    let startup: Vec<AccessUnit> = startup_keyframe_pts
        .map(|keyframe_pts| {
            startup
                .into_iter()
                .filter(|unit| unit.pts >= keyframe_pts)
                .collect()
        })
        .unwrap_or_default();
    // Skip broadcast replays of frames the (filtered) startup snapshot
    // already delivered.
    let startup_keys = crate::mount::startup_dedup_keys(&startup);
    let mut startup = VecDeque::from(startup);
    let mut awaiting_video_keyframe = video_codec.is_some();
    // A CRA picture is a valid random-access point, but it can be followed by
    // RASL pictures which still reference pictures from before the CRA. Those
    // leading pictures must not be sent to a newly joined decoder.
    let mut dropping_h265_cra_leading_pictures = false;
    loop {
        let unit = if let Some(unit) = startup.pop_front() {
            unit
        } else {
            match receiver.recv().await {
                Ok(unit) => unit,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!("http-flv lagged {n}; waiting for a keyframe");
                    muxer.reset_after_discontinuity();
                    video_seq_pending = video_codec.is_some();
                    audio_seq_pending = description.tracks.iter().any(|t| t.codec == Codec::Aac);
                    awaiting_video_keyframe = video_codec.is_some();
                    dropping_h265_cra_leading_pictures = false;
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        };
        // Skip broadcast replays of frames the startup snapshot already
        // delivered (duplicate keyframes break FLV timestamp continuity).
        if startup_keys.contains(&(unit.track_id, unit.rtp_timestamp)) {
            continue;
        }
        let is_video = matches!(unit.codec, Codec::H264 | Codec::H265);
        if awaiting_video_keyframe {
            if !is_video || !unit.is_keyframe {
                continue;
            }
            awaiting_video_keyframe = false;
            dropping_h265_cra_leading_pictures = false;
        }
        if dropping_h265_cra_leading_pictures {
            if !is_video {
                continue;
            }
            match h265_vcl_nal_type(&unit.data) {
                Some(8 | 9) => continue,
                Some(_) => dropping_h265_cra_leading_pictures = false,
                None => continue,
            }
        }
        // CRA is a random-access point, hence always a keyframe; skip the
        // full-frame scan on every other frame.
        let starts_h265_cra = unit.is_keyframe
            && unit.codec == Codec::H265
            && h265_random_access_is_cra(&unit.data);
        if video_seq_pending || audio_seq_pending {
            configure_sequence_headers(
                &mut muxer,
                &description,
                &fmtp_snapshot,
                &fmtp_source,
                &mut video_seq_pending,
                &mut audio_seq_pending,
            );
            if video_seq_pending || audio_seq_pending {
                // fmtp may still arrive via a later ANNOUNCE; refresh the
                // snapshot instead of locking and cloning it every frame.
                fmtp_snapshot = fmtp_source.announce_fmtp();
            }
        }
        if video_seq_pending && unit.is_keyframe && is_video {
            let seq = match unit.codec {
                Codec::H264 => FlvMuxer::build_avc_sequence_header(&unit.data),
                Codec::H265 => FlvMuxer::build_h265_sequence_header(&unit.data),
                _ => None,
            };
            if let Some(seq) = seq {
                muxer.set_video_sequence_header(seq);
                video_seq_pending = false;
            }
        }
        if audio_seq_pending && unit.codec == Codec::Aac {
            audio_seq_pending = false;
        }

        let tags = muxer.encode(&unit);
        if starts_h265_cra {
            dropping_h265_cra_leading_pictures = true;
        }
        if !tags.is_empty() {
            if let Err(e) = writer.write_raw(&tags).await {
                tracing::debug!("flv write: {e}");
                break;
            }
            if let Err(e) = writer.flush().await {
                tracing::debug!("flv flush: {e}");
                break;
            }
        }
    }
    Ok(writer)
}

fn configure_sequence_headers(
    muxer: &mut FlvMuxer,
    description: &MediaDescription,
    announce_fmtp: &std::collections::HashMap<u8, std::sync::Arc<str>>,
    mount: &crate::mount::MountHandle,
    video_seq_pending: &mut bool,
    audio_seq_pending: &mut bool,
) {
    for track in &description.tracks {
        let effective = announce_fmtp
            .get(&track.id)
            .map(|value| value.as_ref())
            .or(track.fmtp.as_deref());
        let Some(fmtp) = effective else { continue };

        match track.codec {
            Codec::H264 if *video_seq_pending => {
                let (sps, pps) = FlvMuxer::parse_h264_sprop_from_fmtp(fmtp);
                if let (Some(sps), Some(pps)) = (sps, pps)
                    && let Some(seq) = FlvMuxer::build_avc_sequence_header_from_nals(&sps, &pps)
                {
                    muxer.set_video_sequence_header(seq);
                    *video_seq_pending = false;
                }
            }
            Codec::H265 if *video_seq_pending => {
                let (vps, sps, pps) = FlvMuxer::parse_h265_sprop_from_fmtp(fmtp);
                if let (Some(vps), Some(sps), Some(pps)) = (vps, sps, pps)
                    && let Some(seq) =
                        FlvMuxer::build_h265_sequence_header_from_nals(&vps, &sps, &pps)
                {
                    muxer.set_video_sequence_header(seq);
                    *video_seq_pending = false;
                }
            }
            Codec::Aac if *audio_seq_pending => {
                if let Some(seq) = FlvMuxer::parse_aac_config_from_fmtp(fmtp) {
                    muxer.set_aac_sequence_header(seq);
                    *audio_seq_pending = false;
                }
            }
            Codec::Aac if *audio_seq_pending => {
                // Fallback: RTMP publishers store the raw AudioSpecificConfig
                // on the mount even without an ANNOUNCE fmtp.
                if let Ok(headers) = mount.inner.rtmp_seq_headers.lock()
                    && let Some(seq) = headers.get(&track.id)
                {
                    muxer.set_aac_sequence_header(seq.clone());
                    *audio_seq_pending = false;
                }
            }
            _ => {}
        }
    }
}

fn h265_random_access_is_cra(data: &[u8]) -> bool {
    h265_nal_types(data)
        .into_iter()
        .any(|nal_type| nal_type == 21)
}

fn h265_vcl_nal_type(data: &[u8]) -> Option<u8> {
    h265_nal_types(data)
        .into_iter()
        .find(|nal_type| *nal_type <= 31)
}

fn h265_nal_types(data: &[u8]) -> Vec<u8> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        let length = if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else if index + 4 <= data.len() && data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else {
            index += 1;
            continue;
        };
        starts.push(index + length);
        index += length;
    }
    if starts.is_empty() {
        return data
            .first()
            .copied()
            .into_iter()
            .map(|byte| (byte >> 1) & 0x3f)
            .collect::<Vec<_>>();
    }
    starts
        .into_iter()
        .filter_map(|start| data.get(start).copied())
        .map(|byte| (byte >> 1) & 0x3f)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_cra_and_rasl_access_units() {
        let cra = [0, 0, 0, 1, 21 << 1, 1];
        let rasl = [0, 0, 0, 1, 9 << 1, 1];
        assert!(h265_random_access_is_cra(&cra));
        assert_eq!(h265_vcl_nal_type(&rasl), Some(9));
    }
}
