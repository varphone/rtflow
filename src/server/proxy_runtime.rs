//! On-demand proxy task lifecycle owned by a server mount.
use super::{Mount, MountConfig};
use crate::{
    client::{ClientOptions, RtspClient},
    proxy::{ProxyState, UpstreamConfig},
};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub(super) fn start_proxy_if_idle(
    mount: Arc<Mount>,
    server_cancel: CancellationToken,
    no_subscriber_grace: Duration,
) {
    if let MountConfig::Proxy { upstream } = &mount.config
        && mount
            .proxy_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        upstream.set_state(ProxyState::Connecting);
        spawn_proxy(
            mount.clone(),
            (**upstream).clone(),
            server_cancel,
            no_subscriber_grace,
        );
    }
}

fn finish_proxy(mount: &Mount, upstream: &UpstreamConfig, state: ProxyState) {
    upstream.set_state(state);
    mount.proxy_running.store(false, Ordering::Release);
}

fn spawn_proxy(
    mount: Arc<Mount>,
    upstream: UpstreamConfig,
    server_cancel: CancellationToken,
    no_subscriber_grace: Duration,
) {
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            if mount.frames.receiver_count() == 0 {
                let idle_since = std::time::Instant::now();
                while mount.frames.receiver_count() == 0 {
                    if idle_since.elapsed() >= no_subscriber_grace {
                        finish_proxy(&mount, &upstream, ProxyState::Idle);
                        return;
                    }
                    tokio::select! {
                        _ = server_cancel.cancelled() => {
                            finish_proxy(&mount, &upstream, ProxyState::Stopped);
                            return;
                        }
                        _ = mount.cancel.cancelled() => {
                            finish_proxy(&mount, &upstream, ProxyState::Stopped);
                            return;
                        }
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                    }
                }
            }

            upstream.set_state(ProxyState::Connecting);
            let options = ClientOptions {
                transport: upstream.transport,
                credentials: upstream.credentials.clone(),
                request_timeout: upstream.request_timeout,
                jitter: upstream.jitter.clone(),
                ..Default::default()
            };
            let connected = tokio::select! {
                _ = server_cancel.cancelled() => {
                    finish_proxy(&mount, &upstream, ProxyState::Stopped);
                    return;
                }
                _ = mount.cancel.cancelled() => {
                    finish_proxy(&mount, &upstream, ProxyState::Stopped);
                    return;
                }
                result = RtspClient::connect(upstream.url.clone(), options) => result,
            };
            if let Ok(mut session) = connected {
                attempts = 0;
                upstream.set_state(ProxyState::Streaming);
                let mut idle_since = None;
                let interval_duration = no_subscriber_grace
                    .min(Duration::from_millis(250))
                    .max(Duration::from_millis(10));
                let mut idle_check = tokio::time::interval(interval_duration);
                loop {
                    tokio::select! {
                        _ = server_cancel.cancelled() => {
                            finish_proxy(&mount, &upstream, ProxyState::Stopped);
                            return;
                        }
                        _ = mount.cancel.cancelled() => {
                            finish_proxy(&mount, &upstream, ProxyState::Stopped);
                            return;
                        }
                        _ = idle_check.tick() => {
                            if mount.frames.receiver_count() == 0 {
                                let since = idle_since.get_or_insert_with(std::time::Instant::now);
                                if since.elapsed() >= no_subscriber_grace {
                                    finish_proxy(&mount, &upstream, ProxyState::Idle);
                                    return;
                                }
                            } else {
                                idle_since = None;
                            }
                        }
                        frame = session.next() => match frame {
                            Some(Ok(frame)) => {
                                if mount.frames.send(frame).is_ok() {
                                    mount.stats.lock().unwrap().accepted_frames += 1;
                                } else {
                                    mount.stats.lock().unwrap().dropped_frames += 1;
                                }
                            }
                            Some(Err(_)) | None => break,
                        }
                    }
                }
            }

            attempts = attempts.saturating_add(1);
            let delay = upstream.next_retry(attempts);
            upstream.set_state(ProxyState::BackingOff {
                retry_at: std::time::Instant::now() + delay,
                attempts,
            });
            tokio::select! {
                _ = server_cancel.cancelled() => {
                    finish_proxy(&mount, &upstream, ProxyState::Stopped);
                    return;
                }
                _ = mount.cancel.cancelled() => {
                    finish_proxy(&mount, &upstream, ProxyState::Stopped);
                    return;
                }
                _ = tokio::time::sleep(delay) => {}
            }
        }
    });
}
