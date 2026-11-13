use super::control::ControlCommand;
use super::*;

pub struct ClientSession {
    pub description: MediaDescription,
    id: SessionId,
    receiver: mpsc::Receiver<Result<AccessUnit>>,
    control_tx: Option<mpsc::Sender<ControlCommand>>,
    request_timeout: Duration,
    playing: bool,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
    quality: Arc<Mutex<HashMap<u8, RtcpSession>>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClientQualitySnapshot {
    pub tracks: BTreeMap<u8, ReceptionSnapshot>,
}

/// Internal construction bundle so `ClientSession::new` stays readable.
pub(super) struct SessionParts {
    pub(super) description: MediaDescription,
    pub(super) id: SessionId,
    pub(super) receiver: mpsc::Receiver<Result<AccessUnit>>,
    pub(super) control_tx: Option<mpsc::Sender<ControlCommand>>,
    pub(super) request_timeout: Duration,
    pub(super) cancel: CancellationToken,
    pub(super) tasks: Vec<JoinHandle<()>>,
    pub(super) quality: Arc<Mutex<HashMap<u8, RtcpSession>>>,
}

impl ClientSession {
    pub(super) fn new(parts: SessionParts) -> Self {
        Self {
            description: parts.description,
            id: parts.id,
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
    pub async fn play(&mut self) -> Result<()> {
        self.send_control("PLAY").await?;
        self.playing = true;
        Ok(())
    }
    pub async fn pause(&mut self) -> Result<()> {
        self.send_control("PAUSE").await?;
        self.playing = false;
        Ok(())
    }
    /// Receive the next access unit. Media consumption never blocks on the
    /// control plane: keep-alives run in the dedicated control task
    /// (audit RT-7/RT-8) and failures surface as error frames.
    pub async fn next(&mut self) -> Option<Result<AccessUnit>> {
        self.receiver.recv().await
    }

    pub async fn keep_alive(&mut self) -> Result<()> {
        self.send_control("GET_PARAMETER").await
    }
    async fn send_control(&mut self, method: &'static str) -> Result<()> {
        let Some(commands) = self.control_tx.as_ref() else {
            return Err(Error::Closed);
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        commands
            .send(ControlCommand::Method {
                method,
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
        self.send_control("TEARDOWN").await
    }
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub async fn quality_snapshot(&self) -> ClientQualitySnapshot {
        ClientQualitySnapshot {
            tracks: self
                .quality
                .lock()
                .await
                .iter()
                .map(|(track, session)| (*track, session.snapshot()))
                .collect(),
        }
    }
}
impl Drop for ClientSession {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}
