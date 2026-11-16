//! Dynamic SRT pull/push endpoints backed by the business database.
use crate::{
    config::Config,
    store::{
        self, Store,
        srt::{SrtDirection, SrtRecord},
    },
};
use rtflow::MountRegistry;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};
use tokio_util::sync::CancellationToken;

struct RecordTask {
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    pull_status: Option<Arc<Mutex<Vec<crate::srt_pull::SrtPullStatus>>>>,
    push_status: Option<Arc<Mutex<Vec<crate::srt_push::SrtPushStatus>>>>,
}

pub struct SrtSupervisor {
    store: Arc<Store>,
    config: Arc<Config>,
    mounts: Arc<MountRegistry>,
    cancel: CancellationToken,
    records: RwLock<Vec<SrtRecord>>,
    tasks: Mutex<HashMap<String, RecordTask>>,
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
            tasks: Mutex::new(HashMap::new()),
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

    /// Restart every endpoint. Used at startup and after a full reload.
    pub fn reload(self: &Arc<Self>) {
        {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (_, task) in tasks.drain() {
                task.cancel.cancel();
                for handle in task.tasks {
                    handle.abort();
                }
            }
        }
        for record in self.records() {
            self.start(&record);
        }
    }

    fn start(self: &Arc<Self>, record: &SrtRecord) {
        if !record.enabled {
            return;
        }
        let mut runtime = (*self.config).clone();
        runtime.srt.pull.clear();
        runtime.srt.push.clear();
        match record.direction {
            SrtDirection::Pull => runtime.srt.pull.push(record.pull()),
            SrtDirection::Push => runtime.srt.push.push(record.push()),
        }
        let runtime = Arc::new(runtime);
        let cancel = self.cancel.child_token();
        let (tasks, pull_status, push_status) = match record.direction {
            SrtDirection::Pull => {
                let status = crate::srt_pull::status_table(&runtime);
                let tasks = crate::srt_pull::spawn(
                    runtime,
                    self.mounts.clone(),
                    cancel.clone(),
                    status.clone(),
                );
                (tasks, Some(status), None)
            }
            SrtDirection::Push => {
                let status = crate::srt_push::status_table(&runtime);
                let tasks = crate::srt_push::spawn(
                    runtime,
                    self.mounts.clone(),
                    cancel.clone(),
                    status.clone(),
                );
                (tasks, None, Some(status))
            }
        };
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                record.id.clone(),
                RecordTask {
                    cancel,
                    tasks,
                    pull_status,
                    push_status,
                },
            );
    }

    fn stop(&self, id: &str) {
        if let Some(task) = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
        {
            task.cancel.cancel();
            for handle in task.tasks {
                handle.abort();
            }
        }
    }

    fn restart(self: &Arc<Self>, id: &str) {
        self.stop(id);
        if let Some(record) = self.records().into_iter().find(|record| record.id == id) {
            self.start(&record);
        }
    }

    pub fn upsert(self: &Arc<Self>, record: SrtRecord, replace: bool) -> Result<(), String> {
        let existing =
            store::srt::revision(&self.store, &record.id).map_err(|error| error.to_string())?;
        match (existing, replace) {
            (Some(revision), true) => {
                store::srt::update(&self.store, &record, revision)
                    .map_err(|error| error.to_string())?;
            }
            (Some(_), false) => return Err("SRT endpoint already exists".into()),
            (None, _) => {
                store::srt::insert(&self.store, &record).map_err(|error| error.to_string())?
            }
        }
        self.refresh_records();
        self.restart(&record.id);
        Ok(())
    }

    pub fn delete(self: &Arc<Self>, id: &str) -> Result<(), String> {
        self.stop(id);
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
        let tasks = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut values = Vec::new();
        for record in records
            .iter()
            .filter(|record| record.direction == direction)
        {
            let mut value = json!({
                "id": record.id,
                "name": record.name,
                "revision": record.revision,
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
            if let Some(task) = tasks.get(&record.id) {
                match direction {
                    SrtDirection::Pull => {
                        if let Some(status) = task
                            .pull_status
                            .as_ref()
                            .and_then(|status| status.lock().ok()?.first().cloned())
                        {
                            value["connected"] = json!(status.connected);
                            value["encrypted"] = json!(status.encrypted);
                            value["bytes"] = json!(status.bytes);
                            value["rtt_us"] = json!(status.rtt_us);
                            value["skipped"] = json!(status.skipped);
                            value["last_error"] = json!(status.last_error);
                        }
                    }
                    SrtDirection::Push => {
                        if let Some(status) = task
                            .push_status
                            .as_ref()
                            .and_then(|status| status.lock().ok()?.first().cloned())
                        {
                            value["connected"] = json!(status.connected);
                            value["encrypted"] = json!(status.encrypted);
                            value["bytes"] = json!(status.bytes_sent);
                            value["rtt_us"] = json!(status.rtt_us);
                            value["last_error"] = json!(status.last_error);
                        }
                    }
                }
            }
            values.push(value);
        }
        json!({ "items": values })
    }

    #[allow(dead_code)]
    pub fn shutdown(&self) {
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, task) in tasks.drain() {
            task.cancel.cancel();
            for handle in task.tasks {
                handle.abort();
            }
        }
    }
}
