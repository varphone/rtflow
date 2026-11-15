//! HTTP authentication and lifecycle hooks.
//!
//! Authorization hooks are awaited on the control path and fail closed when
//! configured. Lifecycle hooks go through a bounded queue so a slow business
//! API can never stall publishing or playback.
use crate::config::HooksSection;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    PublishAuth,
    PlayAuth,
    PublishDone,
    PlayDone,
    RecordDone,
}

#[derive(Clone, Debug, Serialize)]
pub struct HookRequest {
    pub event: HookKind,
    pub path: String,
    pub role: String,
    pub protocol: String,
    pub peer: Option<String>,
    pub key_id: Option<String>,
    pub stream_id: Option<String>,
    pub reason: Option<String>,
    pub timestamp: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct HookDecision {
    #[serde(default)]
    pub allow: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

pub struct HookClient {
    config: HooksSection,
    client: reqwest::Client,
    queue: mpsc::Sender<HookRequest>,
}

impl HookClient {
    pub fn new(config: HooksSection) -> Result<Arc<Self>, HookError> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout.max(Duration::from_millis(100)))
            .build()
            .map_err(|error| HookError::Client(error.to_string()))?;
        let (queue, receiver) = mpsc::channel(1024);
        let worker_config = config.clone();
        let worker_client = client.clone();
        tokio::spawn(async move {
            run_notifications(worker_config, worker_client, receiver).await;
        });
        Ok(Arc::new(Self {
            config,
            client,
            queue,
        }))
    }

    pub fn enabled(&self) -> bool {
        self.config.on_publish_auth.is_some()
            || self.config.on_play_auth.is_some()
            || self.config.on_publish_done.is_some()
            || self.config.on_play_done.is_some()
            || self.config.on_record_done.is_some()
    }

    /// Blocking authorization call. A missing URL allows; a failed hook is
    /// denied when `fail_closed` is set.
    pub async fn authorize(
        &self,
        kind: HookKind,
        mut request: HookRequest,
    ) -> Result<HookDecision, HookError> {
        let url = match kind {
            HookKind::PublishAuth => self.config.on_publish_auth.as_deref(),
            HookKind::PlayAuth => self.config.on_play_auth.as_deref(),
            _ => None,
        };
        let Some(url) = url else {
            return Ok(HookDecision {
                allow: true,
                reason: None,
                path: None,
            });
        };
        request.event = kind;
        let response = self.post(url, &request).await;
        match response {
            Ok(decision) => Ok(decision),
            Err(error) if self.config.fail_closed => Err(error),
            Err(error) => {
                tracing::warn!(%error, path = %request.path, "hook failed; allowing (fail_closed=false)");
                Ok(HookDecision {
                    allow: true,
                    reason: Some("hook unavailable".into()),
                    path: None,
                })
            }
        }
    }

    /// Fire-and-forget lifecycle notification.
    pub fn notify(&self, request: HookRequest) {
        let relevant = match request.event {
            HookKind::PublishDone => self.config.on_publish_done.is_some(),
            HookKind::PlayDone => self.config.on_play_done.is_some(),
            HookKind::RecordDone => self.config.on_record_done.is_some(),
            _ => false,
        };
        if relevant && self.queue.try_send(request).is_err() {
            tracing::warn!("hook queue full; lifecycle notification dropped");
        }
    }

    async fn post(&self, url: &str, request: &HookRequest) -> Result<HookDecision, HookError> {
        let body =
            serde_json::to_vec(request).map_err(|error| HookError::Serialize(error.to_string()))?;
        let timestamp = unix_now().to_string();
        let mut builder = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .header("X-Rtflow-Timestamp", &timestamp)
            .body(body.clone());
        if let Some(secret) = self.config.secret.as_deref() {
            let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
                .map_err(|error| HookError::Client(error.to_string()))?;
            mac.update(&body);
            mac.update(timestamp.as_bytes());
            let signature = hex(&mac.finalize().into_bytes());
            builder = builder.header("X-Rtflow-Signature", format!("sha256={signature}"));
        }
        let response = builder
            .send()
            .await
            .map_err(|error| HookError::Http(error.to_string()))?;
        if !response.status().is_success() {
            return Err(HookError::Status(response.status().as_u16()));
        }
        response
            .json::<HookDecision>()
            .await
            .map_err(|error| HookError::Http(error.to_string()))
    }
}

async fn run_notifications(
    config: HooksSection,
    client: reqwest::Client,
    mut queue: mpsc::Receiver<HookRequest>,
) {
    while let Some(request) = queue.recv().await {
        let url = match request.event {
            HookKind::PublishDone => config.on_publish_done.as_deref(),
            HookKind::PlayDone => config.on_play_done.as_deref(),
            HookKind::RecordDone => config.on_record_done.as_deref(),
            _ => None,
        };
        let Some(url) = url else {
            continue;
        };
        let body = match serde_json::to_vec(&request) {
            Ok(body) => body,
            Err(_) => continue,
        };
        let mut delivered = false;
        for attempt in 0..3 {
            let timestamp = unix_now().to_string();
            let mut builder = client
                .post(url)
                .timeout(config.timeout)
                .header("Content-Type", "application/json")
                .header("X-Rtflow-Timestamp", &timestamp)
                .body(body.clone());
            if let Some(secret) = config.secret.as_deref() {
                let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC key");
                mac.update(&body);
                mac.update(timestamp.as_bytes());
                let signature = hex(&mac.finalize().into_bytes());
                builder = builder.header("X-Rtflow-Signature", format!("sha256={signature}"));
            }
            match builder.send().await {
                Ok(response) if response.status().is_success() => {
                    delivered = true;
                    break;
                }
                Ok(response) => {
                    tracing::warn!(
                        event = ?request.event,
                        status = response.status().as_u16(),
                        attempt,
                        "lifecycle hook rejected"
                    );
                }
                Err(error) => {
                    tracing::warn!(event = ?request.event, %error, attempt, "lifecycle hook failed");
                }
            }
            tokio::time::sleep(Duration::from_millis(200 * (attempt + 1) as u64)).await;
        }
        if !delivered {
            tracing::warn!(event = ?request.event, path = %request.path, "lifecycle hook dropped after retries");
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("hook client: {0}")]
    Client(String),
    #[error("hook request: {0}")]
    Http(String),
    #[error("hook returned status {0}")]
    Status(u16),
    #[error("hook body: {0}")]
    Serialize(String),
}
