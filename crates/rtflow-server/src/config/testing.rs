//! Admin-driven test-stream settings.
use super::*;
use std::path::PathBuf;

/// Synthetic test-source configuration. Disabled by default: starting
/// processes must be an explicit operator decision.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct TestingSection {
    pub enabled: bool,
    /// ffmpeg executable (absolute path or PATH lookup).
    pub ffmpeg: PathBuf,
    /// Maximum concurrent test jobs.
    pub max_jobs: usize,
    /// Default stream duration.
    #[serde(with = "humantime_serde")]
    pub default_duration: Duration,
    /// Hard upper bound for one job.
    #[serde(with = "humantime_serde")]
    pub max_duration: Duration,
    /// Test mounts are `{mount_prefix}-{kind}-{id}`.
    pub mount_prefix: String,
    /// Optional public bases used for returned playback URLs.
    pub rtmp_base: Option<String>,
    pub rtsp_base: Option<String>,
    pub srt_base: Option<String>,
}

impl Default for TestingSection {
    fn default() -> Self {
        Self {
            enabled: false,
            ffmpeg: PathBuf::from("ffmpeg"),
            max_jobs: 2,
            default_duration: Duration::from_secs(60),
            max_duration: Duration::from_secs(600),
            mount_prefix: "/live/test".into(),
            rtmp_base: None,
            rtsp_base: None,
            srt_base: None,
        }
    }
}

impl TestingSection {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.enabled && self.ffmpeg.as_os_str().is_empty() {
            return Err(ConfigError::Invalid("testing.ffmpeg is empty".into()));
        }
        if self.max_jobs == 0 {
            return Err(ConfigError::Invalid("testing.max_jobs is zero".into()));
        }
        if self.default_duration.is_zero() {
            return Err(ConfigError::Invalid(
                "testing.default_duration is zero".into(),
            ));
        }
        if self.max_duration.is_zero() {
            return Err(ConfigError::Invalid("testing.max_duration is zero".into()));
        }
        if self.default_duration > self.max_duration {
            return Err(ConfigError::Invalid(
                "testing.default_duration exceeds max_duration".into(),
            ));
        }
        if !self.mount_prefix.starts_with('/')
            || self.mount_prefix.ends_with('/')
            || self.mount_prefix.contains("..")
            || !self
                .mount_prefix
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '-' | '_'))
        {
            return Err(ConfigError::Invalid(
                "testing.mount_prefix must be a clean absolute path".into(),
            ));
        }
        Ok(())
    }
}
