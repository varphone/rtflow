//! Application-level metrics layered on top of the framework registry.
use crate::{hooks::HookClient, recording::RecordingSupervisor};
use rtflow::{
    MountRegistry,
    core::metrics::MetricsRegistry,
    http::{Handler, ResponseWriter},
    metrics::render_prometheus,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Counters owned by the application (not the framework registry).
#[derive(Default)]
pub struct ServerCounters {
    pub(crate) auth_failures: AtomicU64,
}

impl ServerCounters {
    pub fn auth_failure(&self) {
        self.auth_failures.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn metrics_handler(
    registry: Arc<MetricsRegistry>,
    mounts: Arc<MountRegistry>,
    recording: Arc<RecordingSupervisor>,
    counters: Arc<ServerCounters>,
    hooks: Option<Arc<HookClient>>,
) -> Handler {
    Arc::new(move |_req, mut writer: ResponseWriter<_>| {
        let registry = registry.clone();
        let mounts = mounts.clone();
        let recording = recording.clone();
        let counters = counters.clone();
        let hooks = hooks.clone();
        Box::pin(async move {
            let mut body = render_prometheus(&registry);
            let total = mounts.paths().len();
            let online = mounts
                .paths()
                .iter()
                .filter(|path| {
                    mounts
                        .get_handle(path.as_str())
                        .is_some_and(|handle| handle.snapshot().publisher_connected)
                })
                .count();
            body.push_str(&format!(
                "# HELP rtflow_paths_total Registered paths\n# TYPE rtflow_paths_total gauge\nrtflow_paths_total {total}\n"
            ));
            body.push_str(&format!(
                "# HELP rtflow_paths_online Paths with an active publisher\n# TYPE rtflow_paths_online gauge\nrtflow_paths_online {online}\n"
            ));
            let recordings = recording.status();
            body.push_str(&format!(
                "# HELP rtflow_recordings_active Active recording sessions\n# TYPE rtflow_recordings_active gauge\nrtflow_recordings_active {}\n",
                recordings.iter().filter(|item| item.active).count()
            ));
            body.push_str(&format!(
                "# HELP rtflow_recording_bytes_total Bytes written by active recordings\n# TYPE rtflow_recording_bytes_total counter\nrtflow_recording_bytes_total {}\n",
                recordings.iter().map(|item| item.bytes).sum::<u64>()
            ));
            body.push_str(&format!(
                "# HELP rtflow_auth_failures_total Rejected authentication decisions\n# TYPE rtflow_auth_failures_total counter\nrtflow_auth_failures_total {}\n",
                counters.auth_failures.load(Ordering::Relaxed)
            ));
            body.push_str(&format!(
                "# HELP rtflow_hook_failures_total Authorization hook failures\n# TYPE rtflow_hook_failures_total counter\nrtflow_hook_failures_total {}\n",
                hooks.as_ref().map(|hooks| hooks.failures()).unwrap_or(0)
            ));
            writer
                .write_body(
                    200,
                    "text/plain; version=0.0.4; charset=utf-8",
                    body.as_bytes(),
                )
                .await?;
            Ok(writer)
        })
    })
}
