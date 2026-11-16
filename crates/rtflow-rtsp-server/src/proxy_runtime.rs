//! On-demand proxy task lifecycle owned by a server mount.
use rtflow_core::{
    error::Operation,
    observe::{FrameworkEvent, ProxyEvent, emit},
};
use rtflow_mount::continuity::{TimelineNormalizer, TrackCompatibility};
use rtflow_mount::upstream::{
    PolicySelector, ProxyState, UpstreamConfig, UpstreamHealth, UpstreamSelector,
};
use rtflow_mount::{Mount, MountConfig};
use rtflow_rtsp_client::{ClientOptions, RtspClient};
use rtflow_runtime::{TaskSpec, TaskSupervisor};

/// Window during which a freshly started proxy ignores the no-subscriber
/// grace, so a SETUP-phase warmup survives until the session issues PLAY.
const PROXY_WARMUP_HOLDOFF: std::time::Duration = std::time::Duration::from_secs(2);
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// Keep an on-demand proxy aware of subscribers from ANY protocol (RTSP,
/// HLS, HTTP-FLV). Protocol handlers only touch the shared broadcast bus, so
/// the server-side watcher is what actually starts the upstream pull.
pub(super) fn watch_proxy(
    mount: Arc<Mount>,
    server_cancel: CancellationToken,
    supervisor: TaskSupervisor,
    no_subscriber_grace: Duration,
    always: bool,
) {
    let task_name = format!("proxy-watch-{}", mount.id);
    supervisor.clone().spawn(
        TaskSpec::new(task_name, Operation::ProxyConnect),
        async move {
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = server_cancel.cancelled() => return Ok(()),
                    _ = mount.cancel.cancelled() => return Ok(()),
                    _ = tick.tick() => {
                        if always || mount.frames.receiver_count() > 0 {
                            start_proxy_if_idle(
                                mount.clone(),
                                server_cancel.clone(),
                                supervisor.clone(),
                                no_subscriber_grace,
                                always,
                            );
                        }
                    }
                }
            }
        },
    );
}

pub(super) fn start_proxy_if_idle(
    mount: Arc<Mount>,
    server_cancel: CancellationToken,
    supervisor: TaskSupervisor,
    no_subscriber_grace: Duration,
    always: bool,
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
            always,
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
    always: bool,
) {
    let task_name = format!("proxy-{}", mount.id);
    supervisor.spawn(
        TaskSpec::new(task_name, Operation::ProxyConnect),
        async move { run_proxy(mount, upstream, server_cancel, no_subscriber_grace, always).await },
    );
}

async fn run_proxy(
    mount: Arc<Mount>,
    upstream: UpstreamConfig,
    server_cancel: CancellationToken,
    no_subscriber_grace: Duration,
    always: bool,
) -> rtflow_core::error::Result<()> {
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
    // TODO(client-roadmap M3): this hand-rolled backoff/failover duplicates
    // `RtspClient::connect_reconnecting`; once the client emits connect/
    // backoff events, evaluate converging on the shared reconnect API.
    let mut attempts = 0u32;
    let mut no_subscriber_since: Option<std::time::Instant> = None;
    let mut had_subscriber = false;
    loop {
        // On-demand lifecycle. The warmup holdoff applies only before the
        // first subscriber ever appears (SETUP-phase warmup); afterwards
        // the proxy stops once subscribers stay away for the grace period.
        if mount.frames.receiver_count() == 0 {
            let idle_since = *no_subscriber_since.get_or_insert_with(|| {
                if had_subscriber {
                    std::time::Instant::now()
                } else {
                    std::time::Instant::now() + PROXY_WARMUP_HOLDOFF
                }
            });
            if !always && idle_since.elapsed() >= no_subscriber_grace {
                finish_proxy(&mount, &upstream, ProxyState::Idle);
                return Ok(());
            }
        } else {
            had_subscriber = true;
            no_subscriber_since = None;
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
        let mut builder = ClientOptions::builder()
            .transport(endpoint.transport)
            .request_timeout(endpoint.request_timeout)
            .jitter(endpoint.jitter.clone());
        if let Some(credentials) = endpoint.credentials.clone() {
            builder = builder.credentials(credentials);
        }
        #[cfg(feature = "tls")]
        if let Some(tls) = endpoint.tls.clone() {
            builder = builder.tls(tls);
        }
        let options = match builder.build() {
            Ok(options) => options,
            Err(_) => {
                finish_proxy(&mount, &upstream, ProxyState::Stopped);
                return Ok(());
            }
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
        let mut failure_code = rtflow_core::error::ErrorCode::UpstreamUnavailable;
        match connected {
            Ok(mut session) => {
                let compatibility =
                    TrackCompatibility::compare(&expected_description, &session.description);
                if !compatibility.compatible {
                    failure_code = rtflow_core::error::ErrorCode::InvalidMedia;
                    emit(
                        &mount.event_sink,
                        FrameworkEvent::Proxy(ProxyEvent::Failed {
                            mount: mount.id.clone(),
                            code: rtflow_core::error::ErrorCode::InvalidMedia,
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
                    let mut had_subscriber = false;
                    let interval_duration = no_subscriber_grace
                        .min(Duration::from_millis(250))
                        .max(Duration::from_millis(10));
                    let mut idle_check = tokio::time::interval(interval_duration);
                    let mut idle_watchdog = tokio::time::interval(Duration::from_secs(1));
                    idle_watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    let mut last_frame = Instant::now();
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
                                    let since = idle_since.get_or_insert_with(|| {
                                        if had_subscriber {
                                            std::time::Instant::now()
                                        } else {
                                            std::time::Instant::now() + PROXY_WARMUP_HOLDOFF
                                        }
                                    });
                                    if !always && since.elapsed() >= no_subscriber_grace {
                                        finish_proxy(&mount, &upstream, ProxyState::Idle);
                                        return Ok(());
                                    }
                                } else {
                                    had_subscriber = true;
                                    idle_since = None;
                                }
                            }
                            // A half-open upstream (peer died without FIN)
                            // leaves `session.next()` pending forever; a
                            // media idle watchdog turns that into a stream
                            // failure so the retry/failover path runs.
                            _ = idle_watchdog.tick() => {
                                if last_frame.elapsed() >= endpoint.media_idle_timeout {
                                    failure_code = rtflow_core::error::ErrorCode::UpstreamUnavailable;
                                    emit(
                                        &mount.event_sink,
                                        FrameworkEvent::Proxy(ProxyEvent::Failed {
                                            mount: mount.id.clone(),
                                            code: rtflow_core::error::ErrorCode::UpstreamUnavailable,
                                        }),
                                    );
                                    break;
                                }
                            }
                            frame = session.next() => match frame {
                                Some(Ok(mut frame)) => {
                                    last_frame = Instant::now();
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
