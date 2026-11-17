//! Weekly pull windows for RTSP pull sources.
//!
//! Sources without a schedule are managed by startup/API registration. This
//! task only adds/removes sources whose `schedule` says they should (not) run.
use crate::{config::ProxySection, paths::PathStore, proxy_control::ensure_mount};
use chrono::Local;
use rtflow::server::ProxyService;
use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const TICK: Duration = Duration::from_secs(15);

pub(crate) fn spawn(
    config: Arc<RwLock<ProxySection>>,
    paths: Arc<PathStore>,
    proxies: Arc<ProxyService>,
    event_sink: Arc<dyn rtflow::EventSink>,
    cancel: CancellationToken,
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    tasks.push(tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tick.tick() => reconcile(&config, &paths, &proxies, &event_sink, &cancel),
            }
        }
    }));
}

fn reconcile(
    config: &Arc<RwLock<ProxySection>>,
    paths: &Arc<PathStore>,
    proxies: &Arc<ProxyService>,
    event_sink: &Arc<dyn rtflow::EventSink>,
    cancel: &CancellationToken,
) {
    let section = config
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let now = Local::now();
    let mut running: HashSet<String> = proxies
        .statuses()
        .into_iter()
        .map(|status| status.path.to_string())
        .collect();
    let mut active = running.len();
    for mount in &section.mounts {
        let Some(schedule) = &mount.schedule else {
            continue;
        };
        let allowed = schedule.allows(now);
        let is_running = running.contains(&mount.path);
        if !mount.enabled || !allowed {
            if is_running {
                proxies.remove(&mount.path);
                running.remove(&mount.path);
                active = active.saturating_sub(1);
                tracing::info!(path = %mount.path, "pull source left schedule; stopped");
            }
            continue;
        }
        if is_running {
            continue;
        }
        if active >= section.max_proxies {
            tracing::warn!(path = %mount.path, max = section.max_proxies, "scheduled pull skipped: proxy limit reached");
            continue;
        }
        match ensure_mount(&section, paths, proxies, event_sink, cancel, mount) {
            Ok(()) => {
                running.insert(mount.path.clone());
                active += 1;
                tracing::info!(path = %mount.path, "pull source entered schedule; started");
            }
            Err(error) => {
                tracing::warn!(path = %mount.path, %error, "scheduled pull start failed");
            }
        }
    }
}
