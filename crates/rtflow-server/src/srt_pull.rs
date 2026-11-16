//! SRT pull sources: act as caller, demux MPEG-TS and publish into mounts.
use crate::config::{Config, SrtPull};
use rtflow::{
    MountConfig, MountHandle, MountRegistry,
    srt::{SrtCaller, SrtCallerConfig, ts_demux::TsStreamDemuxer},
};
use serde::Serialize;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// Runtime status of one configured pull source.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SrtPullStatus {
    pub remote: String,
    pub stream_id: Option<String>,
    pub mount: String,
    pub connected: bool,
    pub encrypted: bool,
    pub bytes: u64,
    pub rtt_us: u32,
    pub skipped: u64,
    pub last_error: Option<String>,
}

/// Create the status table for the configured pulls.
pub fn status_table(config: &Config) -> Arc<Mutex<Vec<SrtPullStatus>>> {
    let entries = config
        .srt
        .pull
        .iter()
        .map(|pull| SrtPullStatus {
            remote: pull.remote.clone(),
            stream_id: pull.stream_id.clone(),
            mount: pull
                .mount
                .clone()
                .unwrap_or_else(|| default_mount_path(pull)),
            ..SrtPullStatus::default()
        })
        .collect();
    Arc::new(Mutex::new(entries))
}

/// Spawn one task per configured pull source. Reconnects with exponential
/// backoff until the server shuts down.
pub fn spawn(
    config: Arc<Config>,
    mounts: Arc<MountRegistry>,
    cancel: CancellationToken,
    status: Arc<Mutex<Vec<SrtPullStatus>>>,
) -> Vec<tokio::task::JoinHandle<()>> {
    config
        .srt
        .pull
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, pull)| {
            let mounts = mounts.clone();
            let cancel = cancel.clone();
            let base = config.clone();
            let status = status.clone();
            tokio::spawn(async move {
                run_pull(pull, index, base, mounts, cancel, status).await;
            })
        })
        .collect()
}

fn update_status(
    status: &Arc<Mutex<Vec<SrtPullStatus>>>,
    index: usize,
    update: impl FnOnce(&mut SrtPullStatus),
) {
    if let Some(entry) = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(index)
    {
        update(entry);
    }
}

async fn run_pull(
    pull: SrtPull,
    index: usize,
    config: Arc<Config>,
    mounts: Arc<MountRegistry>,
    cancel: CancellationToken,
    status: Arc<Mutex<Vec<SrtPullStatus>>>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let passphrase = pull
            .passphrase
            .clone()
            .or_else(|| config.srt.passphrase.clone());
        let pbkeylen = if pull.pbkeylen == 0 {
            config.srt.pbkeylen
        } else {
            pull.pbkeylen
        };
        let caller_config = SrtCallerConfig {
            remote: match pull.remote.parse() {
                Ok(remote) => remote,
                Err(_) => {
                    tracing::error!(remote = %pull.remote, "SRT pull: invalid remote address");
                    return;
                }
            },
            stream_id: pull.stream_id.clone(),
            latency_ms: pull.latency_ms.unwrap_or(config.srt.latency_ms),
            passphrase,
            pbkeylen,
            connect_timeout: Duration::from_secs(5),
            peer_idle_timeout: config.srt.peer_idle_timeout,
            mtu: config.srt.mtu,
            tlpktdrop: config.srt.tlpktdrop,
            ..SrtCallerConfig::default()
        };
        match SrtCaller::connect(caller_config).await {
            Ok(caller) => {
                backoff = Duration::from_secs(1);
                tracing::info!(
                    remote = %pull.remote,
                    stream_id = ?pull.stream_id,
                    "SRT pull: connected"
                );
                let mount_path = pull
                    .mount
                    .clone()
                    .unwrap_or_else(|| default_mount_path(&pull));
                update_status(&status, index, |entry| {
                    entry.connected = true;
                    entry.encrypted = caller.encrypted();
                    entry.mount = mount_path.clone();
                    entry.last_error = None;
                });
                if let Err(error) = pump(
                    caller,
                    &mount_path,
                    mounts.clone(),
                    cancel.clone(),
                    &status,
                    index,
                )
                .await
                {
                    tracing::warn!(%error, mount = %mount_path, "SRT pull: stream ended");
                    update_status(&status, index, |entry| {
                        entry.last_error = Some(error.to_string());
                    });
                }
                update_status(&status, index, |entry| entry.connected = false);
                if let Some(handle) = mounts.get_handle(&mount_path) {
                    handle.mount().cancel.cancel();
                }
                mounts.unregister(&mount_path);
            }
            Err(error) => {
                let message = error.to_string();
                update_status(&status, index, |entry| {
                    entry.connected = false;
                    entry.last_error = Some(message.clone());
                });
                tracing::warn!(remote = %pull.remote, %error, "SRT pull: connect failed");
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn pump(
    mut caller: SrtCaller,
    mount_path: &str,
    mounts: Arc<MountRegistry>,
    cancel: CancellationToken,
    status: &Arc<Mutex<Vec<SrtPullStatus>>>,
    index: usize,
) -> rtflow::Result<()> {
    let mut demux = TsStreamDemuxer::new();
    let mut mount: Option<MountHandle> = None;
    let mut publisher: Option<rtflow::LocalPublisher> = None;
    loop {
        let payload = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = caller.close().await;
                return Ok(());
            }
            result = caller.recv() => result?,
        };
        demux.push(&payload);
        {
            let stats = caller.stats();
            update_status(status, index, |entry| {
                entry.bytes = stats.bytes_received;
                entry.rtt_us = stats.rtt_us;
                entry.skipped = stats.skipped;
            });
        }
        let units = demux.take_units();
        if units.is_empty() {
            continue;
        }
        if mount.is_none()
            && let Some(description) = demux.description()
        {
            let config = MountConfig::Publisher {
                description,
                allow_replace_after_disconnect: true,
            };
            match mounts.register(mount_path.to_owned().into(), config) {
                Ok(handle) => {
                    tracing::info!(mount = %mount_path, "SRT pull: mount registered");
                    publisher = handle.publisher().ok();
                    mount = Some(handle);
                }
                Err(error) => {
                    tracing::warn!(%error, mount = %mount_path, "SRT pull: mount registration failed");
                    return Ok(());
                }
            }
        }
        if let Some(publisher) = &publisher {
            for unit in units {
                let _ = publisher.push(unit);
            }
        }
    }
}

/// Mount path for a pull source: explicit override, else the Stream ID
/// resource, else `/srt/<remote port>`.
fn default_mount_path(pull: &SrtPull) -> String {
    if let Some(stream_id) = &pull.stream_id {
        let resource = stream_id
            .strip_prefix("#!::")
            .and_then(|kv| kv.split(',').find_map(|pair| pair.strip_prefix("r=")))
            .unwrap_or(stream_id);
        let path = resource.split('?').next().unwrap_or(resource);
        if !path.is_empty() {
            return if path.starts_with('/') {
                path.to_owned()
            } else {
                format!("/{path}")
            };
        }
    }
    let port = pull
        .remote
        .rsplit_once(':')
        .map(|(_, port)| port)
        .unwrap_or("0");
    format!("/srt/{port}")
}
