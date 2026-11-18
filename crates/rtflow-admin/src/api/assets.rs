//! Embedded admin SPA assets.
use rtflow_http::{Handler, ResponseWriter};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

pub(super) struct Asset {
    pub path: &'static str,
    pub bytes: &'static [u8],
    pub content_type: &'static str,
}

macro_rules! asset {
    ($path:literal, $file:literal, $mime:literal) => {
        Asset {
            path: $path,
            bytes: include_bytes!($file),
            content_type: $mime,
        }
    };
}

pub(super) const INDEX_HTML: &str = include_str!("../assets/index.html");
pub(super) const ASSETS: &[Asset] = &[
    asset!("app.css", "../assets/app.css", "text/css; charset=utf-8"),
    asset!(
        "app.js",
        "../assets/app.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "hls.min.js",
        "../assets/hls.min.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "dash.all.min.js",
        "../assets/dash.all.min.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/api.js",
        "../assets/js/api.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/format.js",
        "../assets/js/format.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/icons.js",
        "../assets/js/icons.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/ui.js",
        "../assets/js/ui.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/player.js",
        "../assets/js/player.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/pages/overview.js",
        "../assets/js/pages/overview.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/pages/live.js",
        "../assets/js/pages/live.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/pages/vod.js",
        "../assets/js/pages/vod.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/pages/paths.js",
        "../assets/js/pages/paths.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/pages/keys.js",
        "../assets/js/pages/keys.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/pages/proxies.js",
        "../assets/js/pages/proxies.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/pages/srt.js",
        "../assets/js/pages/srt.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/pages/recordings.js",
        "../assets/js/pages/recordings.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/pages/testing.js",
        "../assets/js/pages/testing.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/pages/system.js",
        "../assets/js/pages/system.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/pages/audit.js",
        "../assets/js/pages/audit.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/forms/path.js",
        "../assets/js/forms/path.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/forms/key.js",
        "../assets/js/forms/key.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/forms/proxy.js",
        "../assets/js/forms/proxy.js",
        "text/javascript; charset=utf-8"
    ),
    asset!(
        "js/forms/srt.js",
        "../assets/js/forms/srt.js",
        "text/javascript; charset=utf-8"
    ),
];

pub(super) fn handler(
    asset: &'static [u8],
    content_type: &'static str,
    cache_control: &'static str,
) -> Handler {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    asset.hash(&mut hasher);
    let etag = format!("\"{:x}\"", hasher.finish());
    Arc::new(move |req, mut writer: ResponseWriter<_>| {
        let etag = etag.clone();
        Box::pin(async move {
            if req.header("if-none-match").map(str::trim) == Some(etag.as_str()) {
                writer
                    .write_headers(
                        304,
                        &[
                            ("ETag", etag.as_str()),
                            ("Cache-Control", cache_control),
                            ("X-Content-Type-Options", "nosniff"),
                            ("Content-Length", "0"),
                            ("Connection", "close"),
                        ],
                    )
                    .await?;
                writer.flush().await?;
                return Ok(writer);
            }
            let length = asset.len().to_string();
            writer
                .write_headers(
                    200,
                    &[
                        ("Content-Type", content_type),
                        ("Content-Length", length.as_str()),
                        ("Cache-Control", cache_control),
                        ("ETag", etag.as_str()),
                        ("X-Content-Type-Options", "nosniff"),
                        ("Connection", "close"),
                    ],
                )
                .await?;
            writer.write_raw(asset).await?;
            writer.flush().await?;
            Ok(writer)
        })
    })
}
