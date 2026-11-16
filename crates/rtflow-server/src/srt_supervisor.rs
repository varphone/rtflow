//! Dynamic SRT pull/push endpoints backed by the business database.
//!
//! Mutations restart the SRT task set as a unit. This is deliberately simple:
//! SRT endpoints are few, and reconnecting all of them is preferable to a
//! partially applied change while the database is authoritative.
use crate::{
    config::Config,
    store::{self, Store, srt::{SrtDirection, SrtRecord}},
};
use rtflow::MountRegistry;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

struct Current {
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    pull_status: Arc<Mutex<Vec<crate::srt_pull::SrtPullStatus>>>,
    push_status: Arc<Mutex<Vec<crate::srt_push::SrtPushStatus>>>,
    pull_ids: Vec<String>,
    push_ids: Vec<String>,
}

pub struct SrtSupervisor {
    store: Arc<Store>,
    config: Arc<Config>,
    mounts: Arc<MountRegistry>,
    cancel: CancellationToken,
    records: RwLock<Vec<SrtRecord>>,
    current: Mutex<Option<Current>>,
}

impl SrtSupervisor {
    pub fn new(
        store: Arc<Store>,
        config: Arc<Config>,
        mounts: Arc<MountRegistry>,
        cancel: CancellationToken,
    ) -> Result<Arc<Self>, String> {
        let mut records = store::srt::load_all(&store).map_err(|error| error.to_string())?;
        if records.is_empty() {
            for (index, pull) in config.srt.pull.iter().enumerate() {
                let record = SrtRecord {
                    id: format!("srt_pull_{index}"),
                    name: pull
                        .stream_id
                        .clone()
                        .unwrap_or_else(|| format!("pull {index}")),
                    enabled: true,
                    direction: SrtDirection::Pull,
                    remote: pull.remote.clone(),
                    stream_id: pull.stream_id.clone(),
                    mount: pull.mount.clone(),
                    passphrase: pull.passphrase.clone(),
                    pbkeylen: pull.pbkeylen,
                    latency_ms: pull.latency_ms,
                    revision: 1,
                };
                store::srt::insert(&store, &record).map_err(|error| error.to_string())?;
                records.push(record);
            }
            for (index, push) in config.srt.push.iter().enumerate() {
                let record = SrtRecord {
                    id: format!("srt_push_{index}"),
                    name: push.mount.clone(),
                    enabled: true,
                    direction: SrtDirection::Push,
                    remote: push.remote.clone(),
                    stream_id: push.stream_id.clone(),
                    mount: Some(push.mount.clone()),
                    passphrase: push.passphrase.clone(),
                    pbkeylen: push.pbkeylen,
                    latency_ms: push.latency_ms,
                    revision: 1,
                };
                store::srt::insert(&store, &record).map_err(|error| error.to_string())?;
                records.push(record);
            }
        }
        let supervisor = Arc::new(Self {
            store,
            config,
            mounts,
            cancel,
            records: RwLock::new(records),
            current: Mutex::new(None),
        });
        supervisor.reload();
        Ok(supervisor)
    }

    pub fn records(&self) -> Vec<SrtRecord> {
        self.records
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn reload(self: &Arc<Self>) {
        if let Some(current) = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            current.cancel.cancel();
            for task in current.tasks {
                task.abort();
            }
        }
        let records = self.records();
        let pulls: Vec<_> = records
            .iter()
            .filter(|record| record.enabled && record.direction == SrtDirection::Pull)
            .map(|record| (record.id.clone(), record.pull()))
            .collect();
        let pushes: Vec<_> = records
            .iter()
            .filter(|record| record.enabled && record.direction == SrtDirection::Push)
            .map(|record| (record.id.clone(), record.push()))
            .collect();
        let mut runtime = (*self.config).clone();
        runtime.srt.pull = pulls.iter().map(|(_, pull)| pull.clone()).collect();
        runtime.srt.push = pushes.iter().map(|(_, push)| push.clone()).collect();
        let runtime = Arc::new(runtime);
        let cancel = self.cancel.child_token();
        let pull_status = crate::srt_pull::status_table(&runtime);
        let push_status = crate::srt_push::status_table(&runtime);
        let mut tasks = crate::srt_pull::spawn(
            runtime.clone(),
            self.mounts.clone(),
            cancel.clone(),
            pull_status.clone(),
        );
        tasks.extend(crate::srt_push::spawn(
            runtime,
            self.mounts.clone(),
            cancel.clone(),
            push_status.clone(),
        ));
        *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Current {
            cancel,
            tasks,
            pull_status,
            push_status,
            pull_ids: pulls.iter().map(|(id, _)| id.clone()).collect(),
            push_ids: pushes.iter().map(|(id, _)| id.clone()).collect(),
        });
    }

    pub fn upsert(&self, record: SrtRecord, replace: bool) -> Result<(), String> {
        let existing = store::srt::revision(&self.store, &record.id)
            .map_err(|error| error.to_string())?;
        match (existing, replace) {
            (Some(revision), true) => {
                store::srt::update(&self.store, &record, revision)
                    .map_err(|error| error.to_string())?;
            }
            (Some(_), false) => return Err("SRT endpoint already exists".into()),
            (None, _) => store::srt::insert(&self.store, &record)
                .map_err(|error| error.to_string())?,
        }
        self.refresh_records();
        // `reload` needs Arc; this method is only called through Arc in the API.
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        store::srt::delete(&self.store, id).map_err(|error| error.to_string())?;
        self.refresh_records();
        Ok(())
    }

    pub fn refresh_records(&self) {
        if let Ok(records) = store::srt::load_all(&self.store) {
            *self
                .records
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = records;
        }
    }

    pub fn pull_json(&self) -> Value {
        self.status_json(SrtDirection::Pull)
    }

    pub fn push_json(&self) -> Value {
        self.status_json(SrtDirection::Push)
    }

    fn status_json(&self, direction: SrtDirection) -> Value {
        let records = self.records();
        let current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut values = Vec::new();
        for record in records.iter().filter(|record| record.direction == direction) {
            let mut value = json!({
                "id": record.id,
                "name": record.name,
                "role": if direction == SrtDirection::Pull { "caller" } else { "sender" },
                "direction": if direction == SrtDirection::Pull { "pull" } else { "push" },
                "enabled": record.enabled,
                "remote": record.remote,
                "stream_id": record.stream_id,
                "mount": record.mount,
                "pbkeylen": record.pbkeylen,
                "latency_ms": record.latency_ms,
                "connected": false,
            });
            match (direction, &*current) {
                (SrtDirection::Pull, Some(current)) => {
                    if let Some(status) = current
                        .pull_ids
                        .iter()
                        .position(|id| id == &record.id)
                        .and_then(|index| current.pull_status.lock().ok()?.get(index).cloned())
                    {
                        value["connected"] = json!(status.connected);
                        value["encrypted"] = json!(status.encrypted);
                        value["bytes"] = json!(status.bytes);
                        value["rtt_us"] = json!(status.rtt_us);
                        value["skipped"] = json!(status.skipped);
                        value["last_error"] = json!(status.last_error);
                    }
                }
                (SrtDirection::Push, Some(current)) => {
                    if let Some(status) = current
                        .push_ids
                        .iter()
                        .position(|id| id == &record.id)
                        .and_then(|index| current.push_status.lock().ok()?.get(index).cloned())
                    {
                        value["connected"] = json!(status.connected);
                        value["encrypted"] = json!(status.encrypted);
                        value["bytes"] = json!(status.bytes_sent);
                        value["rtt_us"] = json!(status.rtt_us);
                        value["last_error"] = json!(status.last_error);
                    }
                }
                _ => {}
            }
            values.push(value);
        }
        json!({ "items": values })
    }

    #[allow(dead_code)]
    pub fn shutdown(&self) {
        if let Some(current) = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            current.cancel.cancel();
            for task in current.tasks {
                task.abort();
            }
        }
    }
}

#[allow(dead_code)]
fn _duration(value: Duration) -> Duration {
    value
}
