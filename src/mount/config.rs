use super::*;

// ---------------------------------------------------------------------------
// MountConfig
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub enum MountConfig {
    Local {
        description: MediaDescription,
    },
    Publisher {
        description: MediaDescription,
        allow_replace_after_disconnect: bool,
    },
    #[cfg(feature = "server")]
    Proxy {
        upstream: Box<UpstreamConfig>,
    },
    #[cfg(feature = "file-storage")]
    Vod {
        source: Arc<dyn crate::media::vod::VodSource>,
    },
}

// ---------------------------------------------------------------------------
// PushResult / StreamStats / MountSnapshot
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PushResult {
    Accepted,
    DroppedNoSubscribers,
    DroppedQueueFull,
}

// ---------------------------------------------------------------------------
// description_for helper
// ---------------------------------------------------------------------------

pub(crate) fn description_for(config: &MountConfig) -> Result<MediaDescription> {
    match config {
        MountConfig::Local { description } | MountConfig::Publisher { description, .. } => {
            Ok(description.clone())
        }
        #[cfg(feature = "server")]
        MountConfig::Proxy { upstream } => upstream.cached_description.clone().ok_or_else(|| {
            Error::Rtsp("proxy mount requires cached SDP description before serving".into())
        }),
        #[cfg(feature = "file-storage")]
        MountConfig::Vod { source } => Ok(source.description().clone()),
    }
}
