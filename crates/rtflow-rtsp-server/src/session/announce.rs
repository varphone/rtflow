use super::helpers::*;
use super::*;

/// ANNOUNCE: validate the publisher SDP and record its fmtp parameters.
pub(super) async fn handle_announce(
    req: &rtsp::Request,
    state: &Arc<State>,
    writer: &ConnWriter,
    has_announced: &mut bool,
    resources: &mut ConnectionResources,
    cseq: String,
) -> Result<()> {
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
    let (path, mount) = if let Some((path, mount)) = registered_mount_for_uri(state, &req.uri).await
    {
        (path, mount)
    } else if state.config.dynamic_publishers {
        let path = dynamic_publisher_path(&req.uri);
        let Some(path) = path else {
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
        };
        if announced.tracks.is_empty() {
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
        let mut description = announced.clone();
        description.session_name = path.trim_start_matches('/').into();
        let options = rtflow_mount::MountOptions::builder()
            .event_sink(state.config.event_sink.clone())
            .cancel(state.cancel.child_token())
            .startup_cache_config(state.config.startup_cache)
            .timeline(state.config.timeline)
            .build();
        match state.mounts.register_with(
            path.clone().into(),
            MountConfig::Publisher {
                description,
                allow_replace_after_disconnect: true,
            },
            options,
        ) {
            Ok(handle) => {
                resources.dynamic_mounts.push(handle.clone());
                (path, handle.mount().clone())
            }
            Err(rtflow_core::error::Error::Conflict(_)) => {
                let Some((path, mount)) = registered_mount_for_uri(state, &req.uri).await else {
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
                (path, mount)
            }
            Err(_) => {
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&simple_response(
                        453,
                        "Not Enough Bandwidth",
                        cseq,
                    )))
                    .await?;
                return Ok(());
            }
        }
    } else {
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
    let _ = path;
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

/// Normalize and validate the ANNOUNCE path used for a dynamic publisher.
fn dynamic_publisher_path(uri: &str) -> Option<String> {
    let path = uri_path(uri);
    let path = path.trim_end_matches('/');
    if path.is_empty() || path == "/" || path.len() > 1024 {
        return None;
    }
    if path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return None;
    }
    Some(path.to_owned())
}
