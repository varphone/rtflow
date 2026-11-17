//! Managed path policy for RTSP pull sources.
//!
//! A pull source owns the effective `PathRecord` at its mount path: live
//! protocols, recording and the reader limit are projected from
//! [`ProxyOutputs`]. Records carry a marker so deleting the pull source only
//! removes policies it created, never an operator's own path configuration.
use crate::{
    config::ProxyOutputs,
    paths::{PathError, PathRecord, PathSource, PathSpec, PathStore},
};
use serde_json::json;
use std::time::Duration;

pub(crate) const MANAGED_BY: &str = "rtsp-pull";

fn marker() -> serde_json::Value {
    json!({ "managed_by": MANAGED_BY })
}

pub(crate) fn is_managed(record: &PathRecord) -> bool {
    record
        .metadata
        .get("managed_by")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| value == MANAGED_BY)
}

fn key(path: &str) -> &str {
    path.trim_start_matches('/')
}

pub(crate) fn managed(paths: &PathStore, path: &str) -> bool {
    paths
        .get(key(path))
        .is_some_and(|record| is_managed(&record))
}

/// Current managed outputs, if this path is owned by a pull source.
pub(crate) fn outputs(paths: &PathStore, path: &str) -> Option<ProxyOutputs> {
    let record = paths.get(key(path))?;
    if !is_managed(&record) {
        return None;
    }
    Some(ProxyOutputs {
        read: record.read,
        hls: record.hls,
        dash: record.dash,
        flv: record.flv,
        webrtc: record.webrtc,
        record: record.record,
        max_readers: record.max_readers,
    })
}

/// Apply pull outputs to the path policy.
///
/// An existing managed record is updated, a runtime dynamic record is
/// upgraded to an API record, and a missing record is created. A concrete
/// non-managed API/Config policy is refused so manual configuration is never
/// silently overwritten.
pub(crate) fn apply(
    paths: &PathStore,
    path: &str,
    outputs: &ProxyOutputs,
) -> Result<PathRecord, String> {
    let name = key(path);
    if let Some(existing) = paths.get(name) {
        if existing.source == PathSource::Config {
            return Err(format!(
                "path {path} is read-only configuration; choose another pull path"
            ));
        }
        if existing.source == PathSource::Api && !is_managed(&existing) {
            return Err(format!(
                "path {path} already has a non-managed policy; delete it in 路径 or choose another pull path"
            ));
        }
    }
    let spec = PathSpec {
        enabled: true,
        publish: false,
        read: outputs.read || outputs.live_enabled(),
        record: outputs.record,
        hls: outputs.hls,
        dash: outputs.dash,
        flv: outputs.flv,
        webrtc: outputs.webrtc,
        max_readers: outputs.max_readers,
        tenant: None,
        labels: Default::default(),
        metadata: marker(),
        ttl: Duration::ZERO,
    };
    paths
        .upsert_api(name, spec)
        .map_err(|error: PathError| error.to_string())
}

/// Remove the managed policy, leaving operator-owned paths untouched.
pub(crate) fn remove(paths: &PathStore, path: &str) {
    let name = key(path);
    if let Some(record) = paths.get(name)
        && is_managed(&record)
        && record.source == PathSource::Api
    {
        let _ = paths.remove(name);
    }
}
