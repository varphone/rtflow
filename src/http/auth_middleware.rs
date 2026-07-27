//! HTTP token authentication middleware.
//!
//! Wraps an existing [`Handler`] to enforce HMAC token verification
//! before delegating to the inner handler.
use super::router::Handler;
use super::response::ResponseWriter;
use crate::token_auth::TokenConfig;
use std::sync::Arc;

/// Wrap a handler with HMAC token authentication.
///
/// Requests must include a valid token in the query parameter configured
/// by `TokenConfig::param_name`. Invalid or missing tokens receive a
/// 403 Forbidden response with a JSON error body.
pub fn with_token_auth(handler: Handler, config: TokenConfig) -> Handler {
    Arc::new(move |req, writer| {
        let handler = handler.clone();
        let config = config.clone();
        Box::pin(async move {
            // Extract path without query for verification
            let path = req.path.split('?').next().unwrap_or(&req.path);

            // Look for token in query parameters
            let token = req.query.get(&config.param_name).map(String::as_str);

            match token {
                Some(token) if crate::token_auth::verify_token(&config, path, token) => {
                    handler(req, writer).await
                }
                _ => {
                    write_forbidden(writer).await
                }
            }
        })
    })
}

/// Wrap a handler with token auth, but bypass authentication for specific paths.
///
/// Useful for exempting `/health`, `/metrics`, or player pages from auth.
pub fn with_token_auth_bypass(handler: Handler, config: TokenConfig, bypass: Vec<String>) -> Handler {
    Arc::new(move |req, writer| {
        let handler = handler.clone();
        let config = config.clone();
        let bypass = bypass.clone();
        Box::pin(async move {
            let path = req.path.split('?').next().unwrap_or(&req.path);

            // Check bypass list
            if bypass.iter().any(|b| path.starts_with(b)) {
                return handler(req, writer).await;
            }

            let token = req.query.get(&config.param_name).map(String::as_str);
            match token {
                Some(token) if crate::token_auth::verify_token(&config, path, token) => {
                    handler(req, writer).await
                }
                _ => {
                    write_forbidden(writer).await
                }
            }
        })
    })
}

async fn write_forbidden<W: tokio::io::AsyncWrite + Unpin>(
    mut writer: ResponseWriter<W>,
) -> crate::Result<ResponseWriter<W>> {
    let body = r#"{"error":"forbidden","message":"valid token required"}"#;
    writer
        .write_body(403, "application/json", body.as_bytes())
        .await?;
    Ok(writer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{HttpRequest, Method};
    use bytes::Bytes;
    use std::collections::HashMap;

    fn test_config() -> TokenConfig {
        TokenConfig::new(b"test-secret".to_vec())
    }

    fn make_request(path: &str, query: HashMap<String, String>) -> HttpRequest {
        HttpRequest {
            method: Method::Get,
            path: path.to_string(),
            query,
            headers: vec![],
            body: Bytes::new(),
            path_params: vec![],
        }
    }

    fn ok_handler() -> Handler {
        Arc::new(|_req, mut writer: ResponseWriter<tokio::net::tcp::OwnedWriteHalf>| {
            Box::pin(async move {
                writer.write_body(200, "text/plain", b"OK").await?;
                Ok(writer)
            })
        })
    }

    #[test]
    fn with_token_auth_creates_handler() {
        let handler = ok_handler();
        let config = test_config();
        let _wrapped = with_token_auth(handler, config);
        // Just verify it compiles and creates without panic
    }

    #[test]
    fn with_token_auth_bypass_creates_handler() {
        let handler = ok_handler();
        let config = test_config();
        let bypass = vec!["/health".to_string(), "/metrics".to_string()];
        let _wrapped = with_token_auth_bypass(handler, config, bypass);
    }
}
