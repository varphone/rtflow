//! Append-only JSON Lines audit log.
//!
//! Records control-plane changes and authentication decisions so operators can
//! answer "who changed what, when" without scraping text logs. Rotates by size;
//! secrets and tokens are never written (callers pass ids, not credentials).
use serde::Serialize;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize)]
pub struct AuditEvent {
    pub ts: u64,
    /// Machine-readable action, e.g. `path.create`, `auth.deny`, `key.issue`.
    pub action: &'static str,
    pub result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl AuditEvent {
    pub fn new(action: &'static str, result: &'static str) -> Self {
        Self {
            ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            action,
            result,
            path: None,
            protocol: None,
            role: None,
            user: None,
            key_id: None,
            peer: None,
            detail: None,
        }
    }

    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn protocol(mut self, protocol: impl Into<String>) -> Self {
        self.protocol = Some(protocol.into());
        self
    }

    pub fn role(mut self, role: impl Into<String>) -> Self {
        self.role = Some(role.into());
        self
    }

    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    pub fn key_id(mut self, key_id: impl Into<String>) -> Self {
        self.key_id = Some(key_id.into());
        self
    }

    pub fn peer(mut self, peer: impl Into<String>) -> Self {
        self.peer = Some(peer.into());
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

pub struct AuditLog {
    file: PathBuf,
    max_bytes: u64,
    max_files: usize,
    lock: Mutex<()>,
}

impl AuditLog {
    pub fn new(file: PathBuf, max_bytes: u64, max_files: usize) -> Self {
        Self {
            file,
            max_bytes: max_bytes.max(1024),
            max_files: max_files.max(1),
            lock: Mutex::new(()),
        }
    }

    /// Best-effort write: auditing must never fail the operation it records.
    pub fn record(&self, event: &AuditEvent) {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(parent) = self.file.parent()
            && fs::create_dir_all(parent).is_err()
        {
            return;
        }
        self.rotate_if_needed();
        let Ok(line) = serde_json::to_string(event) else {
            return;
        };
        let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)
        else {
            return;
        };
        let _ = writeln!(file, "{line}");
    }

    fn rotate_if_needed(&self) {
        let Ok(metadata) = fs::metadata(&self.file) else {
            return;
        };
        if metadata.len() < self.max_bytes {
            return;
        }
        for index in (1..self.max_files).rev() {
            let from = rotated_path(&self.file, index);
            let to = rotated_path(&self.file, index + 1);
            if from.exists() {
                let _ = fs::rename(&from, to);
            }
        }
        let _ = fs::rename(&self.file, rotated_path(&self.file, 1));
    }

    pub fn path(&self) -> &Path {
        &self.file
    }

    /// Newest-first audit events, reading rotated files oldest to newest.
    /// Malformed lines are skipped so one partial write cannot hide history.
    pub fn recent(&self, limit: usize) -> Vec<serde_json::Value> {
        let mut files: Vec<PathBuf> = (1..self.max_files)
            .rev()
            .map(|index| rotated_path(&self.file, index))
            .filter(|path| path.is_file())
            .collect();
        files.push(self.file.clone());
        let mut events = Vec::new();
        for path in files {
            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };
            for line in text.lines() {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                    events.push(value);
                }
            }
        }
        events.reverse();
        events.truncate(limit.min(1000));
        events
    }
}

fn rotated_path(file: &Path, index: usize) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_json_lines_and_rotates() {
        let dir = std::env::temp_dir().join(format!("rtflow-audit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let log = AuditLog::new(dir.join("audit.log"), 512, 3);
        for index in 0..50 {
            log.record(
                &AuditEvent::new("path.create", "ok")
                    .path(format!("live/cam{index}"))
                    .user("admin"),
            );
        }
        let text = fs::read_to_string(log.path()).unwrap();
        assert!(text.contains("\"action\":\"path.create\""));
        assert!(dir.join("audit.log.1").exists());
        let recent = log.recent(5);
        assert_eq!(recent.len(), 5);
        assert!(recent[0]["ts"].as_u64().unwrap() >= recent[4]["ts"].as_u64().unwrap());
        let _ = fs::remove_dir_all(dir);
    }
}
