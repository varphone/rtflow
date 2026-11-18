//! Admin-driven synthetic test streams.
//!
//! The manager only runs a fixed ffmpeg argument template against local
//! listener addresses; it never accepts a remote URL or a free-form command.
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

const STDERR_LINES: usize = 50;
const JOB_RETENTION: Duration = Duration::from_secs(600);

#[derive(Clone, Debug)]
pub struct TestingConfig {
    pub enabled: bool,
    pub ffmpeg: PathBuf,
    pub max_jobs: usize,
    pub default_duration: Duration,
    pub max_duration: Duration,
    pub mount_prefix: String,
    pub rtmp_base: String,
    pub rtsp_base: String,
    pub srt_base: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TestKind {
    Rtmp,
    RtspTcp,
    RtspUdp,
    Srt,
}

impl TestKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Rtmp => "rtmp",
            Self::RtspTcp => "rtsp_tcp",
            Self::RtspUdp => "rtsp_udp",
            Self::Srt => "srt",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StartTestRequest {
    pub kind: Option<TestKind>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub audio: bool,
    pub duration_secs: u64,
}

impl Default for StartTestRequest {
    fn default() -> Self {
        Self {
            kind: None,
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 2500,
            audio: true,
            duration_secs: 60,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct TestUrls {
    pub rtmp: String,
    pub rtsp: String,
    pub hls: String,
    pub dash: String,
    pub whep: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct TestJobView {
    pub id: u64,
    pub kind: &'static str,
    pub mount: String,
    pub state: String,
    pub pid: Option<u32>,
    pub started_ms: u128,
    pub duration_secs: u64,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub stderr: Vec<String>,
    pub urls: TestUrls,
}

#[derive(Clone, Debug, Serialize)]
pub struct FfmpegStatus {
    pub available: bool,
    pub version: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TestingView {
    pub enabled: bool,
    pub ffmpeg: FfmpegStatus,
    pub max_jobs: usize,
    pub default_duration_secs: u64,
    pub max_duration_secs: u64,
    pub jobs: Vec<TestJobView>,
}

struct Job {
    id: u64,
    kind: TestKind,
    mount: String,
    state: String,
    pid: Option<u32>,
    started: Instant,
    duration: Duration,
    cancel: CancellationToken,
    stderr: Arc<Mutex<VecDeque<String>>>,
    exit_code: Option<i32>,
    error: Option<String>,
}

pub struct TestJobManager {
    config: TestingConfig,
    jobs: Mutex<HashMap<u64, Arc<Mutex<Job>>>>,
    next_id: AtomicU64,
    ffmpeg_status: Mutex<Option<FfmpegStatus>>,
}

pub fn testing_config(config: &crate::config::Config) -> TestingConfig {
    let port = |addr: std::net::SocketAddr| addr.port();
    TestingConfig {
        enabled: config.testing.enabled,
        ffmpeg: config.testing.ffmpeg.clone(),
        max_jobs: config.testing.max_jobs,
        default_duration: config.testing.default_duration,
        max_duration: config.testing.max_duration,
        mount_prefix: config.testing.mount_prefix.clone(),
        rtmp_base: config
            .testing
            .rtmp_base
            .clone()
            .unwrap_or_else(|| format!("rtmp://127.0.0.1:{}", port(config.rtmp.listen))),
        rtsp_base: config
            .testing
            .rtsp_base
            .clone()
            .unwrap_or_else(|| format!("rtsp://127.0.0.1:{}", port(config.rtsp.listen))),
        srt_base: config
            .testing
            .srt_base
            .clone()
            .unwrap_or_else(|| format!("srt://127.0.0.1:{}", port(config.srt.listen))),
    }
}

impl TestJobManager {
    pub fn new(config: TestingConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            jobs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            ffmpeg_status: Mutex::new(None),
        })
    }

    pub fn view(&self) -> TestingView {
        let mut jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        jobs.retain(|_, job| {
            let job = job
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            !matches!(job.state.as_str(), "stopped" | "failed")
                || now.duration_since(job.started) < JOB_RETENTION
        });
        let mut items: Vec<_> = jobs.values().map(|job| self.job_view(job)).collect();
        items.sort_by(|left, right| left.id.cmp(&right.id));
        TestingView {
            enabled: self.config.enabled,
            ffmpeg: self.ffmpeg_status(),
            max_jobs: self.config.max_jobs,
            default_duration_secs: self.config.default_duration.as_secs().max(1),
            max_duration_secs: self.config.max_duration.as_secs().max(1),
            jobs: items,
        }
    }

    pub fn start(&self, body: serde_json::Value) -> Result<TestJobView, String> {
        if !self.config.enabled {
            return Err("testing is disabled in the server configuration".into());
        }
        let mut request: StartTestRequest =
            serde_json::from_value(body).map_err(|error| format!("invalid request: {error}"))?;
        let kind = request
            .kind
            .ok_or_else(|| "missing test kind".to_string())?;
        request.width = request.width.max(1);
        request.height = request.height.max(1);
        request.fps = request.fps.max(1);
        request.bitrate_kbps = request.bitrate_kbps.max(1);
        request.duration_secs = request
            .duration_secs
            .max(1)
            .min(self.config.max_duration.as_secs().max(1));
        validate(&request)?;

        let active = {
            let jobs = self
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            jobs.values()
                .filter(|job| {
                    let job = job
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    matches!(job.state.as_str(), "starting" | "running" | "stopping")
                })
                .count()
        };
        if active >= self.config.max_jobs {
            return Err(format!(
                "too many test jobs (limit {})",
                self.config.max_jobs
            ));
        }

        let status = self.ffmpeg_status();
        if !status.available {
            return Err(status
                .error
                .unwrap_or_else(|| "ffmpeg is unavailable".into()));
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mount = format!(
            "{}-{}-{}",
            self.config.mount_prefix.trim_end_matches('/'),
            kind.as_str(),
            id
        );
        let mut command = self.build_command(kind, &mount, &request)?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| format!("ffmpeg spawn failed: {error}"))?;
        let pid = child.id();
        let stderr = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_LINES)));
        if let Some(stream) = child.stderr.take() {
            let stderr = stderr.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut ring = stderr
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if ring.len() >= STDERR_LINES {
                        ring.pop_front();
                    }
                    ring.push_back(line);
                }
            });
        }

        let cancel = CancellationToken::new();
        let job = Arc::new(Mutex::new(Job {
            id,
            kind,
            mount: mount.clone(),
            state: "running".into(),
            pid,
            started: Instant::now(),
            duration: Duration::from_secs(request.duration_secs),
            cancel: cancel.clone(),
            stderr: stderr.clone(),
            exit_code: None,
            error: None,
        }));
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, job.clone());

        let monitor_job = job.clone();
        let duration = Duration::from_secs(request.duration_secs);
        tokio::spawn(async move {
            let status = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = child.kill().await;
                    child.wait().await.ok()
                }
                _ = tokio::time::sleep(duration) => {
                    let _ = child.kill().await;
                    child.wait().await.ok()
                }
                status = child.wait() => status.ok(),
            };
            let mut job = monitor_job
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            job.exit_code = status.and_then(|status| status.code());
            if job.state != "stopping" {
                job.state = if job.exit_code == Some(0) {
                    "stopped"
                } else {
                    "failed"
                }
                .into();
            } else {
                job.state = "stopped".into();
            }
        });

        Ok(self.job_view(&job))
    }

    pub fn stop(&self, id: &str) -> Result<(), String> {
        let id: u64 = id.parse().map_err(|_| "invalid job id".to_string())?;
        let jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(job) = jobs.get(&id).cloned() else {
            return Err("test job not found".into());
        };
        let mut job = job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(job.state.as_str(), "stopped" | "failed") {
            return Ok(());
        }
        job.state = "stopping".into();
        job.cancel.cancel();
        Ok(())
    }

    pub fn shutdown(&self) {
        let jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for job in jobs.values() {
            let job = job
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            job.cancel.cancel();
        }
    }

    fn job_view(&self, job: &Arc<Mutex<Job>>) -> TestJobView {
        let job = job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let started_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .saturating_sub(job.started.elapsed().as_millis());
        let stderr = job
            .stderr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        TestJobView {
            id: job.id,
            kind: job.kind.as_str(),
            mount: job.mount.clone(),
            state: job.state.clone(),
            pid: job.pid,
            started_ms,
            duration_secs: job.duration.as_secs(),
            exit_code: job.exit_code,
            error: job.error.clone(),
            stderr: stderr.iter().cloned().collect(),
            urls: self.urls(&job.mount),
        }
    }

    fn urls(&self, mount: &str) -> TestUrls {
        let relative = mount.trim_start_matches('/');
        TestUrls {
            rtmp: format!("{}{}", self.config.rtmp_base.trim_end_matches('/'), mount),
            rtsp: format!("{}{}", self.config.rtsp_base.trim_end_matches('/'), mount),
            hls: format!("/hls/{relative}/index.m3u8"),
            dash: format!("/dash/{relative}/manifest.mpd"),
            whep: format!("/whep/{relative}"),
        }
    }

    fn build_command(
        &self,
        kind: TestKind,
        mount: &str,
        request: &StartTestRequest,
    ) -> Result<tokio::process::Command, String> {
        let size = format!("{}x{}", request.width, request.height);
        let fps = request.fps.to_string();
        let gop = request.fps.max(1).to_string();
        let bitrate = format!("{}k", request.bitrate_kbps);
        let buffer = format!("{}k", request.bitrate_kbps.saturating_mul(2));
        let duration = request.duration_secs.to_string();
        let mut command = tokio::process::Command::new(&self.config.ffmpeg);
        command.args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-nostdin",
            "-re",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size={size}:rate={fps}"),
        ]);
        if request.audio {
            command.args([
                "-re",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=1000:sample_rate=48000",
            ]);
        }
        command.args([
            "-t",
            &duration,
            "-map",
            "0:v:0",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-x264-params",
            "repeat-headers=1",
            "-pix_fmt",
            "yuv420p",
            "-b:v",
            &bitrate,
            "-maxrate",
            &bitrate,
            "-bufsize",
            &buffer,
            "-g",
            &gop,
            "-keyint_min",
            &gop,
            "-sc_threshold",
            "0",
        ]);
        if request.audio {
            command.args([
                "-map", "1:a:0", "-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "2",
            ]);
        } else {
            command.arg("-an");
        }
        match kind {
            TestKind::Rtmp => {
                command.args([
                    "-f",
                    "flv",
                    &format!("{}{}", self.config.rtmp_base.trim_end_matches('/'), mount),
                ]);
            }
            TestKind::RtspTcp | TestKind::RtspUdp => {
                command.args([
                    "-rtsp_transport",
                    if kind == TestKind::RtspTcp {
                        "tcp"
                    } else {
                        "udp"
                    },
                    "-muxdelay",
                    "0",
                    "-f",
                    "rtsp",
                    &format!("{}{}", self.config.rtsp_base.trim_end_matches('/'), mount),
                ]);
            }
            TestKind::Srt => {
                let stream_id = mount.trim_start_matches('/');
                let separator = if self.config.srt_base.contains('?') {
                    '&'
                } else {
                    '?'
                };
                command.args([
                    "-f",
                    "mpegts",
                    &format!(
                        "{}{separator}mode=caller&streamid={stream_id}",
                        self.config.srt_base
                    ),
                ]);
            }
        }
        Ok(command)
    }

    fn ffmpeg_status(&self) -> FfmpegStatus {
        if !self.config.enabled {
            return FfmpegStatus {
                available: false,
                version: None,
                error: Some("testing is disabled".into()),
            };
        }
        if let Some(status) = self
            .ffmpeg_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return status;
        }
        let status = match std::process::Command::new(&self.config.ffmpeg)
            .arg("-version")
            .output()
        {
            Ok(output) if output.status.success() => {
                let version = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("ffmpeg")
                    .to_owned();
                FfmpegStatus {
                    available: true,
                    version: Some(version),
                    error: None,
                }
            }
            Ok(output) => FfmpegStatus {
                available: false,
                version: None,
                error: Some(format!("ffmpeg exited with {}", output.status)),
            },
            Err(error) => FfmpegStatus {
                available: false,
                version: None,
                error: Some(format!(
                    "failed to run {}: {error}",
                    self.config.ffmpeg.display()
                )),
            },
        };
        *self
            .ffmpeg_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(status.clone());
        status
    }
}

fn validate(request: &StartTestRequest) -> Result<(), String> {
    if request.width < 320
        || request.height < 180
        || request.width > 1920
        || request.height > 1080
        || !request.width.is_multiple_of(2)
        || !request.height.is_multiple_of(2)
    {
        return Err("resolution must be even and within 320x180..1920x1080".into());
    }
    if !(1..=60).contains(&request.fps) {
        return Err("fps must be 1..=60".into());
    }
    if !(64..=12_000).contains(&request.bitrate_kbps) {
        return Err("bitrate must be 64..=12000 kbps".into());
    }
    if request.duration_secs == 0 {
        return Err("duration must be greater than zero".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(enabled: bool) -> Arc<TestJobManager> {
        TestJobManager::new(TestingConfig {
            enabled,
            ffmpeg: PathBuf::from("ffmpeg"),
            max_jobs: 1,
            default_duration: Duration::from_secs(10),
            max_duration: Duration::from_secs(60),
            mount_prefix: "/live/test".into(),
            rtmp_base: "rtmp://127.0.0.1:1935".into(),
            rtsp_base: "rtsp://127.0.0.1:8554".into(),
            srt_base: "srt://127.0.0.1:9000".into(),
        })
    }

    #[test]
    fn disabled_manager_rejects_start() {
        let manager = manager(false);
        assert!(manager.start(serde_json::json!({"kind":"rtmp"})).is_err());
        assert!(!manager.view().enabled);
    }

    #[test]
    fn request_validation_rejects_bad_geometry() {
        let request = StartTestRequest {
            width: 641,
            ..Default::default()
        };
        assert!(validate(&request).is_err());
    }

    #[test]
    fn command_contains_local_targets_and_no_shell() {
        let manager = manager(true);
        let request = StartTestRequest {
            kind: Some(TestKind::RtspTcp),
            width: 640,
            height: 360,
            fps: 30,
            bitrate_kbps: 1200,
            audio: false,
            duration_secs: 5,
        };
        let command = manager
            .build_command(TestKind::RtspTcp, "/live/test-rtsp_tcp-1", &request)
            .unwrap();
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter()
                .any(|arg| arg == "rtsp://127.0.0.1:8554/live/test-rtsp_tcp-1")
        );
        assert!(args.iter().any(|arg| arg == "tcp"));
        assert!(
            args.iter()
                .any(|arg| arg == "testsrc2=size=640x360:rate=30")
        );
    }

    #[test]
    fn urls_are_relative_for_http_outputs() {
        let manager = manager(true);
        let urls = manager.urls("/live/test-rtmp-1");
        assert_eq!(
            urls.rtsp,
            "rtsp://127.0.0.1:8554/live/test-rtmp-1".to_string()
        );
        assert_eq!(urls.whep, "/whep/live/test-rtmp-1");
    }
}
