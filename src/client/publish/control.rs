use super::super::wire::{candidate_headers_cseq, encode_request_body, request_with_body};
use crate::{
    Error, Result,
    auth::{authorization_from_challenge, basic_authorization},
    rtsp::{Method, Response},
    runtime::io::{ControlConnection, ControlWriteHalf},
    transport::InterleavedFrame,
};
use std::{
    str::FromStr,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, mpsc},
};

pub(super) enum ControlInner {
    Udp(Mutex<ControlConnection>),
    Interleaved {
        writer: Mutex<ControlWriteHalf>,
        responses: Mutex<mpsc::Receiver<Response>>,
    },
}

/// Shared control plane used by `push`/keep-alive/teardown.
pub(super) struct PublisherControl {
    pub(super) inner: ControlInner,
    pub(super) cseq: AtomicU32,
    pub(super) request_timeout: Duration,
    pub(super) uri: String,
    pub(super) session_id: String,
    pub(super) credentials: Option<crate::auth::Credentials>,
}

impl PublisherControl {
    fn next_cseq(&self) -> u32 {
        self.cseq.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) async fn request(
        &self,
        method: &'static str,
        extra: Option<&str>,
        body: Option<&[u8]>,
    ) -> Result<Response> {
        let mut headers = format!("Session: {}\r\n", self.session_id);
        if let Some(extra) = extra {
            headers.push_str(extra);
        }
        match &self.inner {
            ControlInner::Udp(stream) => {
                let sequence = self.next_cseq();
                let mut stream = stream.lock().await;
                let response = request_with_body(
                    &mut stream,
                    method,
                    &self.uri,
                    sequence,
                    Some(&headers),
                    body,
                    self.request_timeout,
                    self.credentials.as_ref(),
                )
                .await?;
                if response.status != 200 {
                    return Err(Error::RtspStatus {
                        method,
                        status: response.status,
                    });
                }
                Ok(response)
            }
            ControlInner::Interleaved { writer, responses } => {
                let sequence = self.next_cseq();
                let mut authorization = self.credentials.as_ref().map(basic_authorization);
                let response = loop {
                    let wire = encode_request_body(
                        method,
                        &self.uri,
                        sequence,
                        Some(&headers),
                        authorization.as_deref(),
                        body,
                    );
                    tokio::time::timeout(self.request_timeout, async {
                        writer.lock().await.write_all(&wire).await
                    })
                    .await
                    .map_err(|_| Error::Limit("interleaved publish write timeout"))??;
                    let mut responses = responses.lock().await;
                    let candidate = loop {
                        let candidate =
                            tokio::time::timeout(self.request_timeout, responses.recv())
                                .await
                                .map_err(|_| Error::Rtsp("RTSP response timeout".into()))?
                                .ok_or(Error::Closed)?;
                        if candidate_headers_cseq(&candidate) == Some(sequence) {
                            break candidate;
                        }
                    };
                    if candidate.status != 401 {
                        break candidate;
                    }
                    let (Some(credentials), Some(challenge)) = (
                        self.credentials.as_ref(),
                        candidate.headers.get("www-authenticate"),
                    ) else {
                        break candidate;
                    };
                    let challenged = authorization_from_challenge(
                        challenge,
                        Method::from_str(method)?,
                        &self.uri,
                        credentials,
                    )?;
                    if authorization.as_deref() == Some(challenged.as_str()) {
                        break candidate;
                    }
                    authorization = Some(challenged);
                };
                if response.status != 200 {
                    return Err(Error::RtspStatus {
                        method,
                        status: response.status,
                    });
                }
                Ok(response)
            }
        }
    }

    pub(super) async fn write_interleaved(&self, channel: u8, payload: bytes::Bytes) -> Result<()> {
        let ControlInner::Interleaved { writer, .. } = &self.inner else {
            return Err(Error::Rtsp("not an interleaved publisher".into()));
        };
        let wire = InterleavedFrame { channel, payload }.try_encode()?;
        writer.lock().await.write_all(&wire).await?;
        Ok(())
    }
}
