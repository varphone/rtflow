//! On-demand proxy task lifecycle owned by a server mount.
use crate::mount::{Mount, MountConfig};
use crate::{
    client::{ClientOptions, RtspClient},
    error::Operation,
    observe::{FrameworkEvent, ProxyEvent, emit},
    proxy::{
        PolicySelector, ProxyState, TimelineNormalizer, TrackCompatibility, UpstreamConfig,
        UpstreamHealth, UpstreamSelector,
    },
    runtime::{TaskSpec, TaskSupervisor},
};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub(super) fn start_proxy_if_idle(
    mount: Arc<Mount>,
    server_cancel: CancellationToken,
    supervisor: TaskSupervisor,
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
            supervisor,
            no_subscriber_grace,
        );
    }
}

fn finish_proxy(mount: &Mount, upstream: &UpstreamConfig, state: ProxyState) {
    upstream.set_state(state);
    mount.proxy_running.store(false, Ordering::Release);
}

struct ProxyRunGuard {
    mount: Arc<Mount>,
    upstream: UpstreamConfig,
}

impl Drop for ProxyRunGuard {
    fn drop(&mut self) {
        if self.mount.proxy_running.swap(false, Ordering::AcqRel) {
            self.upstream.set_state(ProxyState::Stopped);
        }
    }
}

fn spawn_proxy(
    mount: Arc<Mount>,
    upstream: UpstreamConfig,
    server_cancel: CancellationToken,
    supervisor: TaskSupervisor,
    no_subscriber_grace: Duration,
) {
    let task_name = format!("proxy-{}", mount.id);
    supervisor.spawn(
        TaskSpec::new(task_name, Operation::ProxyConnect),
        async move { run_proxy(mount, upstream, server_cancel, no_subscriber_grace).await },
    );
}

async fn run_proxy(
    mount: Arc<Mount>,
    upstream: UpstreamConfig,
    server_cancel: CancellationToken,
    no_subscriber_grace: Duration,
) -> crate::Result<()> {
    let _guard = ProxyRunGuard {
        mount: mount.clone(),
        upstream: upstream.clone(),
    };
    let Some(expected_description) = upstream.cached_description.clone() else {
        finish_proxy(&mount, &upstream, ProxyState::Stopped);
        return Ok(());
    };
    let Ok(mut timeline) = TimelineNormalizer::new(&expected_description, upstream.continuity)
    else {
        finish_proxy(&mount, &upstream, ProxyState::Stopped);
        return Ok(());
    };
    let endpoints = upstream.endpoints();
    let mut health = vec![UpstreamHealth::default(); endpoints.len()];
    let selector = PolicySelector {
        strategy: upstream.failover.strategy,
    };
    let mut attempts = 0u32;
    loop {
        if mount.frames.receiver_count() == 0 {
            let idle_since = std::time::Instant::now();
            while mount.frames.receiver_count() == 0 {
                if idle_since.elapsed() >= no_subscriber_grace {
                    finish_proxy(&mount, &upstream, ProxyState::Idle);
                    return Ok(());
                }
                tokio::select! {
                    _ = server_cancel.cancelled() => {
                        finish_proxy(&mount, &upstream, ProxyState::Stopped);
                        return Ok(());
                    }
                    _ = mount.cancel.cancelled() => {
                        finish_proxy(&mount, &upstream, ProxyState::Stopped);
                        return Ok(());
                    }
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                }
            }
        }

        upstream.set_state(ProxyState::Connecting);
        let now = std::time::Instant::now();
        let Some(endpoint_index) = selector.select(&endpoints, &health, attempts, now) else {
            attempts = attempts.saturating_add(1);
            let delay = health
                .iter()
                .filter_map(|state| state.circuit_open_until)
                .min()
                .map(|until| until.saturating_duration_since(now))
                .unwrap_or_else(|| upstream.next_retry(attempts));
            upstream.set_state(ProxyState::BackingOff {
                retry_at: now + delay,
                attempts,
            });
            emit(
                &mount.event_sink,
                FrameworkEvent::Proxy(ProxyEvent::Backoff {
                    mount: mount.id.clone(),
                    attempt: attempts,
                    delay,
                }),
            );
            tokio::select! {
                _ = server_cancel.cancelled() => {
                    finish_proxy(&mount, &upstream, ProxyState::Stopped);
                    return Ok(());
                }
                _ = mount.cancel.cancelled() => {
                    finish_proxy(&mount, &upstream, ProxyState::Stopped);
                    return Ok(());
                }
                _ = tokio::time::sleep(delay) => {}
            }
            continue;
        };
        let endpoint = &endpoints[endpoint_index];
        emit(
            &mount.event_sink,
            FrameworkEvent::Proxy(ProxyEvent::Connecting {
                mount: mount.id.clone(),
                attempt: attempts.saturating_add(1),
            }),
        );
        let options = ClientOptions {
            transport: endpoint.transport,
            credentials: endpoint.credentials.clone(),
            request_timeout: endpoint.request_timeout,
            jitter: endpoint.jitter.clone(),
            #[cfg(feature = "tls")]
            tls: endpoint.tls.clone(),
            ..Default::default()
        };
        let connected = tokio::select! {
            _ = server_cancel.cancelled() => {
                finish_proxy(&mount, &upstream, ProxyState::Stopped);
                return Ok(());
            }
            _ = mount.cancel.cancelled() => {
                finish_proxy(&mount, &upstream, ProxyState::Stopped);
                return Ok(());
            }
            result = RtspClient::connect(endpoint.url.clone(), options) => result,
        };
        let mut failure_code = crate::ErrorCode::UpstreamUnavailable;
        match connected {
            Ok(mut session) => {
                let compatibility =
                    TrackCompatibility::compare(&expected_description, &session.description);
                if !compatibility.compatible {
                    failure_code = crate::ErrorCode::InvalidMedia;
                    emit(
                        &mount.event_sink,
                        FrameworkEvent::Proxy(ProxyEvent::Failed {
                            mount: mount.id.clone(),
                            code: crate::ErrorCode::InvalidMedia,
                        }),
                    );
                } else {
                    health[endpoint_index].record_success();
                    attempts = 0;
                    timeline.begin_epoch();
                    upstream.set_state(ProxyState::Streaming);
                    emit(
                        &mount.event_sink,
                        FrameworkEvent::Proxy(ProxyEvent::Streaming {
                            mount: mount.id.clone(),
                        }),
                    );
                    let mut idle_since = None;
                    let interval_duration = no_subscriber_grace
                        .min(Duration::from_millis(250))
                        .max(Duration::from_millis(10));
                    let mut idle_check = tokio::time::interval(interval_duration);
                    loop {
                        tokio::select! {
                            _ = server_cancel.cancelled() => {
                                finish_proxy(&mount, &upstream, ProxyState::Stopped);
                                return Ok(());
                            }
                            _ = mount.cancel.cancelled() => {
                                finish_proxy(&mount, &upstream, ProxyState::Stopped);
                                return Ok(());
                            }
                            _ = idle_check.tick() => {
                                if mount.frames.receiver_count() == 0 {
                                    let since = idle_since.get_or_insert_with(std::time::Instant::now);
                                    if since.elapsed() >= no_subscriber_grace {
                                        finish_proxy(&mount, &upstream, ProxyState::Idle);
                                        return Ok(());
                                    }
                                } else {
                                    idle_since = None;
                                }
                            }
                            frame = session.next() => match frame {
                                Some(Ok(mut frame)) => {
                                    if timeline.normalize(&mut frame).is_err() {
                                        mount.stats.add_dropped(1);
                                        break;
                                    }
                                    if mount.publish_cached(frame).is_ok() {
                                        mount.stats.add_accepted(1);
                                    } else {
                                        mount.stats.add_dropped(1);
                                    }
                                }
                                Some(Err(error)) => {
                                    failure_code = error.code();
                                    emit(
                                        &mount.event_sink,
                                        FrameworkEvent::Proxy(ProxyEvent::Failed {
                                            mount: mount.id.clone(),
                                            code: error.code(),
                                        }),
                                    );
                                    break;
                                }
                                None => break,
                            }
                        }
                    }
                }
            }
            Err(error) => {
                failure_code = error.code();
                emit(
                    &mount.event_sink,
                    FrameworkEvent::Proxy(ProxyEvent::Failed {
                        mount: mount.id.clone(),
                        code: failure_code,
                    }),
                );
            }
        }
        health[endpoint_index].record_failure(
            failure_code,
            &upstream.failover,
            std::time::Instant::now(),
        );
        if endpoints.len() == 1 {
            health[endpoint_index].circuit_open_until = None;
        }

        attempts = attempts.saturating_add(1);
        let delay = upstream.next_retry(attempts);
        upstream.set_state(ProxyState::BackingOff {
            retry_at: std::time::Instant::now() + delay,
            attempts,
        });
        emit(
            &mount.event_sink,
            FrameworkEvent::Proxy(ProxyEvent::Backoff {
                mount: mount.id.clone(),
                attempt: attempts,
                delay,
            }),
        );
        tokio::select! {
            _ = server_cancel.cancelled() => {
                finish_proxy(&mount, &upstream, ProxyState::Stopped);
                return Ok(());
            }
            _ = mount.cancel.cancelled() => {
                finish_proxy(&mount, &upstream, ProxyState::Stopped);
                return Ok(());
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}
