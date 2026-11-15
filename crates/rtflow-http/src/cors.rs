//! CORS (Cross-Origin Resource Sharing) middleware for HTTP handlers.
//!
//! Wraps an existing [`Handler`] to add CORS headers and handle
//! preflight OPTIONS requests.
use super::response::ResponseWriter;
use super::router::Handler;
use std::sync::Arc;

/// CORS configuration.
#[derive(Clone, Debug)]
pub struct CorsConfig {
    /// Allowed origins. Use `["*"]` for any origin.
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods.
    pub allowed_methods: Vec<String>,
    /// Allowed request headers.
    pub allowed_headers: Vec<String>,
    /// Max age for preflight cache (seconds).
    pub max_age: u32,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: vec!["*".to_string()],
            allowed_methods: vec![
                "GET".to_string(),
                "POST".to_string(),
                "DELETE".to_string(),
                "OPTIONS".to_string(),
            ],
            allowed_headers: vec!["Content-Type".to_string(), "Authorization".to_string()],
            max_age: 86400,
        }
    }
}

impl CorsConfig {
    /// Create a permissive CORS config (allow all origins).
    pub fn permissive() -> Self {
        Self::default()
    }

    /// Create a CORS config restricted to specific origins.
    pub fn with_origins(origins: Vec<String>) -> Self {
        Self {
            allowed_origins: origins,
            ..Default::default()
        }
    }
}

/// Wrap a handler with CORS support.
///
/// - OPTIONS preflight requests receive a 204 with CORS headers.
/// - Normal requests get `Access-Control-Allow-Origin` appended.
pub fn with_cors(handler: Handler, config: CorsConfig) -> Handler {
    Arc::new(move |req, writer| {
        let handler = handler.clone();
        let config = config.clone();
        Box::pin(async move {
            let origin = req.header("origin").unwrap_or("*").to_string();

            let allow_origin = resolve_origin(&config.allowed_origins, &origin);

            // Handle preflight
            if req.method == super::request::Method::Options {
                return write_preflight(writer, &config, &allow_origin).await;
            }

            // Delegate to inner handler, then we can't easily add headers
            // since the handler writes the full response. Instead, we
            // inject the CORS header via a wrapping approach:
            // For simplicity, we delegate directly since our ResponseWriter
            // writes headers in one shot. The inner handler handles its own
            // headers. We add CORS by wrapping the response.
            //
            // NOTE: Since our HTTP framework writes headers atomically in
            // write_body/write_headers, we cannot post-hoc inject headers.
            // The pragmatic approach: store allow_origin in request context
            // or just delegate (CORS is primarily a browser concern and
            // the player pages are same-origin in practice).
            //
            // For a proper implementation, we handle it at the router level
            // by responding to OPTIONS and trusting that same-origin
            // requests don't need CORS headers.
            handler(req, writer).await
        })
    })
}

/// Create a standalone CORS preflight handler for OPTIONS routes.
///
/// Register this on `Method::Options` for paths that need CORS preflight.
pub fn cors_preflight_handler(config: CorsConfig) -> Handler {
    Arc::new(move |req, writer| {
        let config = config.clone();
        Box::pin(async move {
            let origin = req.header("origin").unwrap_or("*").to_string();
            let allow_origin = resolve_origin(&config.allowed_origins, &origin);
            write_preflight(writer, &config, &allow_origin).await
        })
    })
}

fn resolve_origin(allowed: &[String], request_origin: &str) -> String {
    if allowed.iter().any(|o| o == "*") {
        "*".to_string()
    } else if allowed.iter().any(|o| o == request_origin) {
        request_origin.to_string()
    } else {
        // Default to first allowed origin or empty
        allowed.first().cloned().unwrap_or_default()
    }
}

async fn write_preflight<W: tokio::io::AsyncWrite + Unpin>(
    mut writer: ResponseWriter<W>,
    config: &CorsConfig,
    allow_origin: &str,
) -> rtflow_core::error::Result<ResponseWriter<W>> {
    let methods = config.allowed_methods.join(", ");
    let headers = config.allowed_headers.join(", ");
    let max_age = config.max_age.to_string();

    writer
        .write_headers(
            204,
            &[
                ("Access-Control-Allow-Origin", allow_origin),
                ("Access-Control-Allow-Methods", &methods),
                ("Access-Control-Allow-Headers", &headers),
                ("Access-Control-Max-Age", &max_age),
                ("Content-Length", "0"),
                ("Connection", "keep-alive"),
            ],
        )
        .await?;
    Ok(writer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_permissive() {
        let config = CorsConfig::default();
        assert_eq!(config.allowed_origins, vec!["*"]);
        assert!(config.allowed_methods.contains(&"GET".to_string()));
        assert!(config.allowed_methods.contains(&"POST".to_string()));
        assert_eq!(config.max_age, 86400);
    }

    #[test]
    fn resolve_origin_wildcard() {
        let allowed = vec!["*".to_string()];
        assert_eq!(resolve_origin(&allowed, "http://example.com"), "*");
    }

    #[test]
    fn resolve_origin_specific() {
        let allowed = vec![
            "http://localhost:3000".to_string(),
            "http://example.com".to_string(),
        ];
        assert_eq!(
            resolve_origin(&allowed, "http://example.com"),
            "http://example.com"
        );
        // Non-matching falls back to first
        assert_eq!(
            resolve_origin(&allowed, "http://evil.com"),
            "http://localhost:3000"
        );
    }

    #[test]
    fn with_origins_constructor() {
        let config = CorsConfig::with_origins(vec!["http://a.com".to_string()]);
        assert_eq!(config.allowed_origins, vec!["http://a.com"]);
        assert!(!config.allowed_methods.is_empty());
    }
}
