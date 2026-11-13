use super::helpers::*;
use super::*;

/// Reject malformed, unauthorized or out-of-state requests before dispatch.
///
/// Returns `Ok(true)` when the request may proceed; `Ok(false)` when a
/// response was already written (the caller keeps looping).
#[allow(clippy::too_many_arguments)]
pub(super) async fn gate_request(
    req: &rtsp::Request,
    peer: SocketAddr,
    state: &Arc<State>,
    writer: &ConnWriter,
    session_id: &str,
    phase: ConnectionPhase,
    has_announced: bool,
    cseq: String,
) -> Result<bool> {
    if req.method == Method::Unsupported {
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&simple_response(
                501,
                "Not Implemented",
                cseq,
            )))
            .await?;
        return Ok(false);
    }
    // RFC 2326 §12.32: both Require and Proxy-Require must be answered with
    // 551 when the option is not implemented.
    for header in ["require", "proxy-require"] {
        if let Some(require) = req
            .headers
            .get(header)
            .filter(|value| !value.trim().is_empty())
        {
            let mut response = simple_response(551, "Option Not Supported", cseq);
            response
                .headers
                .insert("unsupported".into(), require.clone());
            writer
                .lock()
                .await
                .write_all(&rtsp::encode_response(&response))
                .await?;
            return Ok(false);
        }
    }
    if let Some(response) = authorize_request(req, peer, state, cseq.clone()).await {
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&response))
            .await?;
        return Ok(false);
    }
    let provided_session = req
        .headers
        .get("session")
        .map(|value| value.split(';').next().unwrap_or(value).trim());
    let invalid_session = if req.method == Method::Setup {
        provided_session.is_some_and(|value| value != session_id)
    } else if matches!(
        req.method,
        Method::Play | Method::Pause | Method::Record | Method::Teardown
    ) {
        provided_session != Some(session_id)
    } else if req.method == Method::GetParameter {
        // RFC 2326 allows a session-less GET_PARAMETER (server
        // capabilities query); only reject a mismatched one.
        provided_session.is_some_and(|value| value != session_id)
    } else {
        false
    };
    if invalid_session {
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&simple_response(
                454,
                "Session Not Found",
                cseq,
            )))
            .await?;
        return Ok(false);
    }
    let valid_phase = match req.method {
        Method::Announce => phase == ConnectionPhase::Init && !has_announced,
        Method::Setup => matches!(phase, ConnectionPhase::Init | ConnectionPhase::Ready),
        Method::Play => matches!(phase, ConnectionPhase::Ready | ConnectionPhase::Paused),
        Method::Pause => phase == ConnectionPhase::Playing,
        Method::Record => phase == ConnectionPhase::Ready,
        _ => true,
    };
    if !valid_phase {
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&simple_response(
                455,
                "Method Not Valid in This State",
                cseq,
            )))
            .await?;
        return Ok(false);
    }
    Ok(true)
}
