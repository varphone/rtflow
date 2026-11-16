//! Proxy startup/reload/metrics glue for the application.
use crate::config::ProxySection;
use rtflow::server::ProxyService;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) fn register_configured(
    config: &ProxySection,
    proxies: &Arc<ProxyService>,
    event_sink: Arc<dyn rtflow::EventSink>,
    cancel: &CancellationToken,
) {
    if !config.enabled {
        return;
    }
    let mut active = proxies.statuses().len();
    for mount in &config.mounts {
        if !mount.enabled {
            continue;
        }
        let path: Arc<str> = Arc::from(mount.path.as_str());
        let Ok(upstream) = mount.upstream_config_with(config.redirect_policy) else {
            continue;
        };
        let startup_cache = upstream.startup_cache;
        let options = rtflow::mount::MountOptions::builder()
            .event_sink(event_sink.clone())
            .startup_cache_config(startup_cache)
            .cancel(cancel.child_token())
            .build();
        if active >= config.max_proxies {
            tracing::warn!(path = %mount.path, max = config.max_proxies, "proxy limit reached");
            continue;
        }
        active += 1;
        if let Err(error) = proxies.ensure(
            path,
            upstream,
            options,
            mount.idle_grace,
            mount.mode.is_always(),
        ) {
            tracing::warn!(path = %mount.path, %error, "proxy config rejected");
        }
    }
}

pub(crate) fn apply_reload(
    old: &ProxySection,
    new: &ProxySection,
    proxies: &Arc<ProxyService>,
    event_sink: Arc<dyn rtflow::EventSink>,
    cancel: &CancellationToken,
) -> bool {
    let mut changed = false;
    let old_mounts: std::collections::BTreeMap<_, _> = old
        .mounts
        .iter()
        .map(|mount| (mount.path.as_str(), mount))
        .collect();
    let new_mounts: std::collections::BTreeMap<_, _> = new
        .mounts
        .iter()
        .map(|mount| (mount.path.as_str(), mount))
        .collect();
    for path in old_mounts.keys() {
        if !new.enabled || !new_mounts.contains_key(path) {
            proxies.remove(path);
            changed = true;
        }
    }
    if !new.enabled {
        return changed;
    }
    for (path, mount) in &new_mounts {
        if !mount.enabled {
            if old_mounts.contains_key(path) {
                proxies.remove(path);
                changed = true;
            }
            continue;
        }
        let unchanged = old.enabled
            && old_mounts
                .get(path)
                .is_some_and(|previous| *previous == *mount);
        if unchanged {
            continue;
        }
        proxies.remove(path);
        let Ok(upstream) = mount.upstream_config_with(new.redirect_policy) else {
            continue;
        };
        let startup_cache = upstream.startup_cache;
        let options = rtflow::mount::MountOptions::builder()
            .event_sink(event_sink.clone())
            .startup_cache_config(startup_cache)
            .cancel(cancel.child_token())
            .build();
        let path: Arc<str> = Arc::from(*path);
        let _ = proxies.ensure(
            path,
            upstream,
            options,
            mount.idle_grace,
            mount.mode.is_always(),
        );
        changed = true;
    }
    changed
}

pub(crate) fn spawn_metrics(
    proxies: Arc<ProxyService>,
    metrics: Arc<rtflow::core::metrics::MetricsRegistry>,
    cancel: CancellationToken,
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    tasks.push(tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tick.tick() => {
                    let statuses = proxies.statuses();
                    metrics
                        .proxy_configured
                        .store(statuses.len() as i64, Ordering::Relaxed);
                    metrics.proxy_streaming.store(
                        statuses.iter().filter(|s| s.state == "streaming").count() as i64,
                        Ordering::Relaxed,
                    );
                }
            }
        }
    }));
}
