use super::helpers::*;
use super::*;

/// ANNOUNCE: validate the publisher SDP and record its fmtp parameters.
pub(super) async fn handle_announce(
    req: &rtsp::Request,
    state: &Arc<State>,
    writer: &ConnWriter,
    has_announced: &mut bool,
    cseq: String,
) -> Result<()> {
    let Some((_path, mount)) = registered_mount_for_uri(state, &req.uri).await else {
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&simple_response(
                404,
                "Not Found",
                cseq,
            )))
            .await?;
        return Ok(());
    };
    if !matches!(mount.config, MountConfig::Publisher { .. }) {
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&simple_response(
                405,
                "Method Not Allowed",
                cseq,
            )))
            .await?;
        return Ok(());
    }
    let content_type = req
        .headers
        .get("content-type")
        .map(|value| value.split(';').next().unwrap_or(value).trim());
    if !content_type.is_some_and(|value| value.eq_ignore_ascii_case("application/sdp")) {
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&simple_response(
                415,
                "Unsupported Media Type",
                cseq,
            )))
            .await?;
        return Ok(());
    }
    let announced = match sdp::parse(&req.body) {
        Ok(description) => description,
        Err(_) => {
            writer
                .lock()
                .await
                .write_all(&rtsp::encode_response(&simple_response(
                    400,
                    "Bad Request",
                    cseq,
                )))
                .await?;
            return Ok(());
        }
    };
    let expected = mount.description_snapshot();
    let fmtp = match announced_fmtp(&expected, &announced) {
        Ok(fmtp) => fmtp,
        Err(_) => {
            writer
                .lock()
                .await
                .write_all(&rtsp::encode_response(&simple_response(
                    400,
                    "Bad Request",
                    cseq,
                )))
                .await?;
            return Ok(());
        }
    };
    mount
        .startup_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    *has_announced = true;
    *mount
        .announce_fmtp
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = fmtp;
    writer
        .lock()
        .await
        .write_all(&rtsp::encode_response(&simple_response(200, "OK", cseq)))
        .await?;
    Ok(())
}
