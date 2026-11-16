//! SRT pull sources: act as caller, demux MPEG-TS and publish into mounts.
use crate::config::{Config, SrtPull};
use rtflow::{
    MountConfig, MountHandle, MountRegistry,
    srt::{SrtCaller, SrtCallerConfig, ts_demux::TsStreamDemuxer},
};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

/// Spawn one task per configured pull source. Reconnects with exponential
/// backoff until the server shuts down.
pub fn spawn(
    config: Arc<Config>,
    mounts: Arc<MountRegistry>,
    cancel: CancellationToken,
) -> Vec<tokio::task::JoinHandle<()>> {
    config
        .srt
        .pull
        .iter()
        .cloned()
        .map(|pull| {
            let mounts = mounts.clone();
            let cancel = cancel.clone();
            let base = config.clone();
            tokio::spawn(async move {
                run_pull(pull, base, mounts, cancel).await;
            })
        })
        .collect()
}

async fn run_pull(
    pull: SrtPull,
    config: Arc<Config>,
    mounts: Arc<MountRegistry>,
    cancel: CancellationToken,
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
                if let Err(error) = pump(caller, &mount_path, mounts.clone(), cancel.clone()).await
                {
                    tracing::warn!(%error, mount = %mount_path, "SRT pull: stream ended");
                }
                if let Some(handle) = mounts.get_handle(&mount_path) {
                    handle.mount().cancel.cancel();
                }
                mounts.unregister(&mount_path);
            }
            Err(error) => {
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
