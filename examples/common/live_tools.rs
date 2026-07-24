use super::TransportArg;
use std::{
    fmt, io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    str::FromStr,
};
use tokio::process::{Child, Command};
use url::Url;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoSize {
    pub width: u32,
    pub height: u32,
}

impl VideoSize {
    pub fn validate(self) -> Result<Self, String> {
        if self.width == 0 || self.height == 0 {
            return Err("video dimensions must be greater than zero".into());
        }
        if !self.width.is_multiple_of(2) || !self.height.is_multiple_of(2) {
            return Err("H.264 yuv420p dimensions must be even".into());
        }
        Ok(self)
    }
}

impl fmt::Display for VideoSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}x{}", self.width, self.height)
    }
}

impl FromStr for VideoSize {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (width, height) = value
            .split_once(['x', 'X'])
            .ok_or_else(|| "video size must use WIDTHxHEIGHT syntax".to_owned())?;
        Self {
            width: width
                .parse()
                .map_err(|_| format!("invalid video width: {width}"))?,
            height: height
                .parse()
                .map_err(|_| format!("invalid video height: {height}"))?,
        }
        .validate()
    }
}

#[derive(Clone, Debug)]
pub struct FfmpegLiveConfig {
    pub executable: PathBuf,
    pub url: Url,
    pub transport: TransportArg,
    pub size: VideoSize,
    pub fps: u32,
    pub gop: u32,
    pub video_bitrate_kbps: u32,
    pub audio: bool,
    pub tone_hz: u32,
}

impl FfmpegLiveConfig {
    pub fn validate(&self) -> Result<(), String> {
        self.size.validate()?;
        if self.fps == 0 {
            return Err("--fps must be greater than zero".into());
        }
        if self.gop == 0 {
            return Err("--gop must be greater than zero".into());
        }
        if self.video_bitrate_kbps == 0 {
            return Err("--video-bitrate-kbps must be greater than zero".into());
        }
        if self.audio && self.tone_hz == 0 {
            return Err("--tone-hz must be greater than zero when audio is enabled".into());
        }
        Ok(())
    }

    pub fn command(&self) -> Result<Command, String> {
        self.validate()?;
        let size = self.size.to_string();
        let fps = self.fps.to_string();
        let gop = self.gop.to_string();
        let bitrate = format!("{}k", self.video_bitrate_kbps);
        let buffer_size = format!("{}k", self.video_bitrate_kbps.saturating_mul(2));
        let mut command = Command::new(&self.executable);
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
        if self.audio {
            command.args([
                "-re",
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency={}:sample_rate=48000", self.tone_hz),
            ]);
        }
        command.args([
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
            &buffer_size,
            "-g",
            &gop,
            "-keyint_min",
            &gop,
            "-sc_threshold",
            "0",
        ]);
        if self.audio {
            command.args([
                "-map", "1:a:0", "-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "2",
            ]);
        } else {
            command.arg("-an");
        }
        command.args([
            "-rtsp_transport",
            self.transport.as_rtsp_name(),
            "-muxdelay",
            "0",
            "-f",
            "rtsp",
            self.url.as_str(),
        ]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        Ok(command)
    }
}

#[derive(Clone, Debug)]
pub struct GstreamerPlayConfig {
    pub executable: PathBuf,
    pub inspect_executable: PathBuf,
    pub url: Url,
    pub transport: TransportArg,
    pub latency_ms: u32,
    pub audio: bool,
    pub headless: bool,
}

impl GstreamerPlayConfig {
    pub fn required_elements(&self) -> Vec<&'static str> {
        let mut elements = vec!["rtspsrc", "queue", "rtph264depay", "h264parse"];
        elements.push("avdec_h264");
        if self.headless {
            elements.push("fakesink");
        } else {
            elements.extend(["videoconvert", "autovideosink"]);
        }
        if self.audio {
            elements.extend(["rtpmp4gdepay", "aacparse"]);
            elements.push("avdec_aac");
            if !self.headless {
                elements.extend(["audioconvert", "audioresample", "autoaudiosink"]);
            }
        }
        elements
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command.args([
            "-e",
            "rtspsrc",
            &format!("location={}", self.url),
            &format!("protocols={}", self.transport.as_rtsp_name()),
            &format!("latency={}", self.latency_ms),
            "name=source",
            "source.",
            "!",
            "queue",
            "!",
            "application/x-rtp,media=video,encoding-name=H264",
            "!",
            "rtph264depay",
            "!",
            "h264parse",
            "config-interval=-1",
            "!",
        ]);
        if self.headless {
            command.args(["avdec_h264", "!", "fakesink", "sync=false"]);
        } else {
            command.args([
                "avdec_h264",
                "!",
                "videoconvert",
                "!",
                "autovideosink",
                "sync=true",
            ]);
        }
        if self.audio {
            command.args([
                "source.",
                "!",
                "queue",
                "!",
                "application/x-rtp,media=audio,encoding-name=MPEG4-GENERIC",
                "!",
                "rtpmp4gdepay",
                "!",
                "aacparse",
                "!",
            ]);
            if self.headless {
                command.args(["avdec_aac", "!", "fakesink", "sync=false", "async=false"]);
            } else {
                command.args([
                    "avdec_aac",
                    "!",
                    "audioconvert",
                    "!",
                    "audioresample",
                    "!",
                    "autoaudiosink",
                    "sync=false",
                    "async-handling=true",
                ]);
            }
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        command
    }
}

pub struct ManagedChild {
    name: &'static str,
    child: Child,
}

impl ManagedChild {
    pub fn spawn(name: &'static str, command: &mut Command) -> io::Result<Self> {
        Ok(Self {
            name,
            child: command.spawn()?,
        })
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    pub async fn terminate(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                let _ = self.child.kill().await;
            }
            Err(error) => {
                tracing::warn!(process = self.name, %error, "failed to query child process");
            }
        }
        let _ = self.child.wait().await;
    }
}

pub fn require_ffmpeg(executable: &Path, audio: bool) -> Result<(), String> {
    let output = std::process::Command::new(executable)
        .args(["-hide_banner", "-encoders"])
        .output()
        .map_err(|error| format!("failed to run {}: {error}", executable.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} could not list encoders (status {})",
            executable.display(),
            output.status
        ));
    }
    let encoders = String::from_utf8_lossy(&output.stdout);
    if !encoders.contains("libx264") {
        return Err(format!(
            "{} does not provide the required libx264 encoder",
            executable.display()
        ));
    }
    if audio && !encoders.contains("aac") {
        return Err(format!(
            "{} does not provide the required AAC encoder",
            executable.display()
        ));
    }
    Ok(())
}

pub fn require_gstreamer(config: &GstreamerPlayConfig) -> Result<(), String> {
    for element in config.required_elements() {
        let status = std::process::Command::new(&config.inspect_executable)
            .arg(element)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| {
                format!(
                    "failed to run {}: {error}",
                    config.inspect_executable.display()
                )
            })?;
        if !status.success() {
            return Err(format!(
                "GStreamer element {element} is unavailable; install the matching plugin package"
            ));
        }
    }
    Ok(())
}
