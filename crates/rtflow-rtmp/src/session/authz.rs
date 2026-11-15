//! RTMP publish/play authorization: URL token, allow-list and async policy.
use super::RtmpSessionConfig;

pub(crate) fn split_stream_token<'a>(
    stream_name: &'a str,
    token: &rtflow_rtsp::token_auth::TokenConfig,
) -> (&'a str, Option<&'a str>) {
    let Some((name, query)) = stream_name.split_once('?') else {
        return (stream_name, None);
    };
    let value = query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == token.param_name).then_some(value)
    });
    (name, value)
}

pub(crate) fn token_allows(
    token: Option<&rtflow_rtsp::token_auth::TokenConfig>,
    app: &str,
    stream: &str,
    value: Option<&str>,
) -> bool {
    let Some(token) = token else {
        return true;
    };
    let Some(value) = value else {
        return false;
    };
    rtflow_rtsp::token_auth::verify_token(token, &format!("/{app}/{stream}"), value)
}

/// Run the full publish policy: URL token, synchronous allow-list, then the
/// asynchronous policy hook. Returns the denial reason on failure.
pub(crate) async fn authorize_publish(
    config: &RtmpSessionConfig,
    app: &str,
    stream_name: &str,
    token_value: Option<&str>,
) -> Result<(), &'static str> {
    if !token_allows(config.token.as_ref(), app, stream_name, token_value) {
        return Err("invalid token");
    }
    if let Some(auth) = &config.authenticator
        && !auth.authorize_publish(app, stream_name)
    {
        return Err("publish not authorized");
    }
    if let Some(auth) = &config.async_authenticator
        && !auth.authorize_publish(app, stream_name).await
    {
        return Err("publish not authorized");
    }
    Ok(())
}

/// Play counterpart of [`authorize_publish`].
pub(crate) async fn authorize_play(
    config: &RtmpSessionConfig,
    app: &str,
    stream_name: &str,
    token_value: Option<&str>,
) -> Result<(), &'static str> {
    if !token_allows(config.token.as_ref(), app, stream_name, token_value) {
        return Err("invalid token");
    }
    if let Some(auth) = &config.authenticator
        && !auth.authorize_play(app, stream_name)
    {
        return Err("play not authorized");
    }
    if let Some(auth) = &config.async_authenticator
        && !auth.authorize_play(app, stream_name).await
    {
        return Err("play not authorized");
    }
    Ok(())
}
