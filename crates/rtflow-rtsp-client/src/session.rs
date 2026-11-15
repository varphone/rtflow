use super::control::ControlCommand;
use super::queue::FrameReceiver;
use super::*;

/// Parameters for a PLAY request (RFC 2326 §10.5).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayRequest {
    /// Optional `Range:` header (NPT/smpte/clock).
    pub range: Option<NptRange>,
    /// Optional `Scale:` header; values below 1 are slow motion.
    pub scale: Option<f64>,
    /// Optional `Speed:` header (transport speed, not media scale).
    pub speed: Option<f64>,
}

impl PlayRequest {
    fn validate(&self) -> Result<()> {
        if self
            .scale
            .is_some_and(|value| !value.is_finite() || value <= 0.0)
        {
            return Err(Error::Limit("PLAY Scale"));
        }
        if self
            .speed
            .is_some_and(|value| !value.is_finite() || value <= 0.0)
        {
            return Err(Error::Limit("PLAY Speed"));
        }
        Ok(())
    }

    fn headers(&self) -> Result<Option<String>> {
        self.validate()?;
        let mut headers = String::new();
        if let Some(range) = self.range {
            headers.push_str(&format!("Range: {}\r\n", range.render()));
        }
        if let Some(scale) = self.scale {
            headers.push_str(&format!("Scale: {scale}\r\n"));
        }
        if let Some(speed) = self.speed {
            headers.push_str(&format!("Speed: {speed}\r\n"));
        }
        Ok((!headers.is_empty()).then_some(headers))
    }
}

pub struct PlaySession {
    pub description: MediaDescription,
    id: SessionId,
    capabilities: ServerCapabilities,
    receiver: FrameReceiver,
    control_tx: Option<mpsc::Sender<ControlCommand>>,
    request_timeout: Duration,
    playing: bool,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
    quality: Arc<std::sync::Mutex<HashMap<u8, RtcpSession>>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct ClientQualitySnapshot {
    pub tracks: BTreeMap<u8, ReceptionSnapshot>,
    pub feedback: BTreeMap<u8, rtflow_rtcp::FeedbackSnapshot>,
}

/// Internal construction bundle so `PlaySession::new` stays readable.
pub(super) struct SessionParts {
    pub(super) description: MediaDescription,
    pub(super) id: SessionId,
    pub(super) capabilities: ServerCapabilities,
    pub(super) receiver: FrameReceiver,
    pub(super) control_tx: Option<mpsc::Sender<ControlCommand>>,
    pub(super) request_timeout: Duration,
    pub(super) cancel: CancellationToken,
    pub(super) tasks: Vec<JoinHandle<()>>,
    pub(super) quality: Arc<std::sync::Mutex<HashMap<u8, RtcpSession>>>,
}

impl PlaySession {
    pub(super) fn new(parts: SessionParts) -> Self {
        Self {
            description: parts.description,
            id: parts.id,
            capabilities: parts.capabilities,
            receiver: parts.receiver,
            control_tx: parts.control_tx,
            request_timeout: parts.request_timeout,
            playing: true,
            cancel: parts.cancel,
            tasks: parts.tasks,
            quality: parts.quality,
        }
    }

    /// Stable identifier for correlating [`ClientEvent`]s and logs.
    pub fn session_id(&self) -> SessionId {
        self.id
    }

    /// Methods/features advertised by the server's OPTIONS response.
    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.capabilities
    }
    pub async fn play(&mut self) -> Result<()> {
        self.play_with(PlayRequest::default()).await
    }

    /// PLAY with an explicit Range/Scale/Speed (RFC 2326 §10.5).
    pub async fn play_with(&mut self, request: PlayRequest) -> Result<()> {
        let headers = request.headers()?;
        self.send_request("PLAY", headers, None).await?;
        self.playing = true;
        tracing::info!(session = %self.id, range = ?request.range, "RTSP client PLAY");
        Ok(())
    }

    pub async fn pause(&mut self) -> Result<()> {
        self.pause_with(None).await
    }

    /// PAUSE with an optional `Range:` header.
    pub async fn pause_with(&mut self, range: Option<NptRange>) -> Result<()> {
        let headers = range.map(|range| format!("Range: {}\r\n", range.render()));
        self.send_request("PAUSE", headers, None).await?;
        self.playing = false;
        tracing::debug!(session = %self.id, "RTSP client PAUSE");
        Ok(())
    }

    /// Receive the next access unit. Media consumption never blocks on the
    /// control plane: keep-alives run in the dedicated control task
    /// (audit RT-7/RT-8) and failures surface as error frames.
    pub async fn next(&mut self) -> Option<Result<AccessUnit>> {
        self.receiver.recv().await
    }

    pub async fn keep_alive(&mut self) -> Result<()> {
        self.send_request("GET_PARAMETER", None, None).await?;
        Ok(())
    }

    /// SET_PARAMETER (RFC 2326 §10.9); `parameters` is the text/parameters
    /// body (for example `"video_speed=2\r\n"`).
    pub async fn set_parameter(&mut self, parameters: &str) -> Result<()> {
        let body = parameters.as_bytes().to_vec();
        let headers = Some(format!(
            "Content-Type: text/parameters\r\nContent-Length: {}\r\n",
            body.len()
        ));
        self.send_request("SET_PARAMETER", headers, Some(body))
            .await?;
        Ok(())
    }

    /// GET_PARAMETER (RFC 2326 §10.8); returns the text/parameters body.
    pub async fn get_parameter(&mut self, parameter: &str) -> Result<String> {
        let body = parameter.as_bytes().to_vec();
        let headers = Some(format!(
            "Content-Type: text/parameters\r\nContent-Length: {}\r\n",
            body.len()
        ));
        let response = self
            .send_request("GET_PARAMETER", headers, Some(body))
            .await?;
        String::from_utf8(response.body)
            .map_err(|_| Error::Rtsp("GET_PARAMETER body is not UTF-8".into()))
    }

    async fn send_request(
        &mut self,
        method: &'static str,
        extra: Option<String>,
        body: Option<Vec<u8>>,
    ) -> Result<Response> {
        let Some(commands) = self.control_tx.as_ref() else {
            return Err(Error::Closed);
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        commands
            .send(ControlCommand::Request {
                method,
                extra,
                body,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Closed)?;
        match timeout(self.request_timeout, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Error::Closed),
            Err(_) => Err(Error::Rtsp("control request timed out".into())),
        }
    }

    pub async fn teardown(mut self) -> Result<()> {
        tracing::debug!(session = %self.id, "RTSP client TEARDOWN");
        self.send_request("TEARDOWN", None, None).await?;
        Ok(())
    }
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub async fn quality_snapshot(&self) -> ClientQualitySnapshot {
        let quality = self
            .quality
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ClientQualitySnapshot {
            tracks: quality
                .iter()
                .map(|(track, session)| (*track, session.snapshot()))
                .collect(),
            feedback: quality
                .iter()
                .map(|(track, session)| (*track, session.feedback()))
                .collect(),
        }
    }
}
impl Drop for PlaySession {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}
