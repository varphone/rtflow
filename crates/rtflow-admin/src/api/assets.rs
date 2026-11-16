//! Embedded admin SPA assets.
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
