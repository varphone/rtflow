//! Proxy startup/reload/metrics glue for the application.
use crate::config::{Config, ProxyMount, ProxySection};
use rtflow::server::ProxyService;
use std::{io::Write, path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(serde::Deserialize, serde::Serialize, Default)]
struct ProxyOverlay {
    mounts: Vec<ProxyMount>,
}

fn overlay_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("proxies.toml")
}

/// Replace the configured proxy list with the runtime overlay when present.
pub(crate) fn load_overlay(mut config: Config) -> Config {
    if !config.proxy.persist_runtime {
        return config;
    }
    let path = overlay_path(&config.server.state_dir);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return config;
    };
    match toml::from_str::<ProxyOverlay>(&text) {
        Ok(overlay) => {
            config.proxy.mounts = overlay.mounts;
            tracing::info!(path = %path.display(), "proxy runtime overlay loaded");
        }
        Err(error) => tracing::warn!(path = %path.display(), %error, "proxy overlay ignored"),
    }
    config
}

/// Persist the current runtime proxy list (best effort, 0600).
pub(crate) fn save_overlay(state_dir: &Path, section: &ProxySection) {
    if !section.persist_runtime {
        return;
    }
    let path = overlay_path(state_dir);
    let overlay = ProxyOverlay {
        mounts: section.mounts.clone(),
    };
    let Ok(text) = toml::to_string_pretty(&overlay) else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(state_dir) {
        tracing::warn!(%error, "proxy overlay directory");
        return;
    }
    let temporary = path.with_extension("toml.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&temporary).and_then(|mut file| {
        file.write_all(text.as_bytes())?;
        file.sync_all()
    }) {
        Ok(()) => {
            if let Err(error) = std::fs::rename(&temporary, &path) {
                tracing::warn!(%error, "proxy overlay rename");
            }
        }
        Err(error) => tracing::warn!(%error, "proxy overlay write"),
    }
}

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
            mount
                .description_refresh
                .or(Some(config.description_refresh)),
        ) {
            tracing::warn!(path = %mount.path, %error, "proxy config rejected");
        }
    }
}

pub(crate) fn apply_reload(
    old: &ProxySection,
    new: &ProxySection,
    _proxies: &Arc<ProxyService>,
    _event_sink: Arc<dyn rtflow::EventSink>,
    _cancel: &CancellationToken,
) -> bool {
    if old != new {
        tracing::warn!(
            "[[proxy.mounts]] is managed by the business database; TOML changes are ignored"
        );
    }
    false
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
