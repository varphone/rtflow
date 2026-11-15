//! Registered stream paths.
//!
//! Two layers compose here:
//! - static glob rules from `rtflow.toml` (`live/*`, `private/**`);
//! - concrete API/dynamic records persisted in `<state_dir>/paths.json`.
//!
//! A concrete record overrides the matching rule for that path. Static rules
//! are never rewritten by the API: configuration remains the operator's
//! source of truth, while runtime registration is a separate, auditable layer.
use crate::config::{Config, PathRule};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::RwLock,
    time::{SystemTime, UNIX_EPOCH},
};

const STORE_FILE: &str = "paths.json";
const STORE_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PathSource {
    Config,
    Api,
    Dynamic,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PathRecord {
    pub name: String,
    pub enabled: bool,
    pub publish: bool,
    pub read: bool,
    pub record: bool,
    pub hls: bool,
    pub dash: bool,
    pub flv: bool,
    pub webrtc: bool,
    pub max_readers: usize,
    pub tenant: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub metadata: serde_json::Value,
    pub source: PathSource,
    pub created_at: u64,
    pub updated_at: u64,
    /// Unix seconds; dynamic paths without a publisher expire after this.
    pub expires_at: Option<u64>,
}

/// Partial update for an API-registered path. Omitted fields keep their
/// current value (PATCH semantics).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct PathPatch {
    pub enabled: Option<bool>,
    pub publish: Option<bool>,
    pub read: Option<bool>,
    pub record: Option<bool>,
    pub hls: Option<bool>,
    pub dash: Option<bool>,
    pub flv: Option<bool>,
    pub webrtc: Option<bool>,
    pub max_readers: Option<usize>,
    pub tenant: Option<String>,
    pub labels: Option<BTreeMap<String, String>>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct PathSpec {
    pub enabled: bool,
    pub publish: bool,
    pub read: bool,
    pub record: bool,
    pub hls: bool,
    pub dash: bool,
    pub flv: bool,
    pub webrtc: bool,
    pub max_readers: usize,
    pub tenant: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub metadata: serde_json::Value,
    #[serde(with = "humantime_serde")]
    pub ttl: std::time::Duration,
}

impl Default for PathSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            publish: true,
            read: true,
            record: false,
            hls: true,
            dash: true,
            flv: true,
            webrtc: true,
            max_readers: 0,
            tenant: None,
            labels: BTreeMap::new(),
            metadata: serde_json::Value::Null,
            ttl: std::time::Duration::ZERO,
        }
    }
}

impl PathSpec {
    pub fn from_rule(rule: &PathRule, source: PathSource, name: &str) -> PathRecord {
        let now = unix_now();
        PathRecord {
            name: name.to_owned(),
            enabled: rule.enabled,
            publish: rule.publish,
            read: rule.read,
            record: rule.record,
            hls: rule.hls,
            dash: rule.dash,
            flv: rule.flv,
            webrtc: rule.webrtc,
            max_readers: rule.max_readers,
            tenant: rule.tenant.clone(),
            labels: rule.labels.clone(),
            metadata: serde_json::Value::Null,
            source,
            created_at: now,
            updated_at: now,
            expires_at: None,
        }
    }

    fn into_record(self, name: &str, source: PathSource) -> PathRecord {
        let now = unix_now();
        let expires_at = if self.ttl.is_zero() {
            None
        } else {
            Some(now.saturating_add(self.ttl.as_secs()))
        };
        PathRecord {
            name: name.to_owned(),
            enabled: self.enabled,
            publish: self.publish,
            read: self.read,
            record: self.record,
            hls: self.hls,
            dash: self.dash,
            flv: self.flv,
            webrtc: self.webrtc,
            max_readers: self.max_readers,
            tenant: self.tenant,
            labels: self.labels,
            metadata: self.metadata,
            source,
            created_at: now,
            updated_at: now,
            expires_at,
        }
    }
}

#[derive(Deserialize, Serialize)]
struct StoreFile {
    version: u32,
    paths: Vec<PathRecord>,
}

pub struct PathStore {
    file: PathBuf,
    rules: RwLock<BTreeMap<String, PathRule>>,
    records: RwLock<BTreeMap<String, PathRecord>>,
    persist_dynamic: bool,
}

impl PathStore {
    pub fn new(config: &Config, persist_dynamic: bool) -> Result<Self, PathError> {
        let state_dir = &config.server.state_dir;
        let file = state_dir.join(STORE_FILE);
        let records = load_file(&file)?;
        let mut rules = config.paths.clone();
        if rules.is_empty() {
            rules.insert("**".into(), PathRule::default());
        }
        Ok(Self {
            file,
            rules: RwLock::new(rules),
            records: RwLock::new(records),
            persist_dynamic,
        })
    }

    /// Effective record for a path: explicit record first, then the most
    /// specific configured rule. Paths with no matching enabled rule still
    /// produce a disabled synthetic record so callers can reject uniformly.
    pub fn resolve(&self, name: &str) -> PathRecord {
        if let Some(record) = self
            .records
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .cloned()
        {
            // Dynamically registered paths always follow the current config
            // rule: otherwise a reloaded `[paths]` policy would be ignored for
            // every path that ever went live.
            if record.source == PathSource::Dynamic {
                let (pattern, rule) = self.rule_for(name);
                if !pattern.is_empty() {
                    return PathSpec::from_rule(&rule, PathSource::Dynamic, name);
                }
            }
            return record;
        }
        let (pattern, rule) = self.rule_for(name);
        PathSpec::from_rule(
            &rule,
            if pattern.is_empty() {
                PathSource::Dynamic
            } else {
                PathSource::Config
            },
            name,
        )
    }

    /// Replace the configured rules (SIGHUP reload). API records are kept.
    pub fn set_rules(&self, rules: BTreeMap<String, PathRule>) {
        let mut rules = rules;
        if rules.is_empty() {
            rules.insert("**".into(), PathRule::default());
        }
        *self
            .rules
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = rules;
    }

    pub fn rule_for(&self, name: &str) -> (String, PathRule) {
        let path = name.trim_start_matches('/');
        let rules = self
            .rules
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rules
            .iter()
            .filter(|(pattern, _)| crate::config::glob_match(pattern, path))
            .max_by_key(|(pattern, _)| specificity(pattern))
            .map(|(pattern, rule)| (pattern.clone(), rule.clone()))
            .unwrap_or_else(|| (String::new(), disabled_rule()))
    }

    /// True when an explicit record or a configured rule covers `name`.
    pub fn known(&self, name: &str) -> bool {
        if self
            .records
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(name)
        {
            return true;
        }
        !self.rule_for(name).0.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<PathRecord> {
        let records = self
            .records
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        records.get(name).cloned()
    }

    pub fn list(&self) -> Vec<PathRecord> {
        self.records
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    pub fn create(&self, name: &str, spec: PathSpec) -> Result<PathRecord, PathError> {
        validate_name(name)?;
        let mut records = self
            .records
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if records.contains_key(name) {
            return Err(PathError::Conflict(name.into()));
        }
        let record = spec.into_record(name, PathSource::Api);
        records.insert(name.to_owned(), record.clone());
        self.persist_locked(&records)?;
        Ok(record)
    }

    pub fn patch(&self, name: &str, patch: PathPatch) -> Result<PathRecord, PathError> {
        validate_name(name)?;
        let mut records = self
            .records
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(existing) = records.get(name) else {
            return Err(PathError::NotFound(name.into()));
        };
        if existing.source == PathSource::Config {
            return Err(PathError::ReadOnly(name.into()));
        }
        let mut record = existing.clone();
        if let Some(value) = patch.enabled {
            record.enabled = value;
        }
        if let Some(value) = patch.publish {
            record.publish = value;
        }
        if let Some(value) = patch.read {
            record.read = value;
        }
        if let Some(value) = patch.record {
            record.record = value;
        }
        if let Some(value) = patch.hls {
            record.hls = value;
        }
        if let Some(value) = patch.dash {
            record.dash = value;
        }
        if let Some(value) = patch.flv {
            record.flv = value;
        }
        if let Some(value) = patch.webrtc {
            record.webrtc = value;
        }
        if let Some(value) = patch.max_readers {
            record.max_readers = value;
        }
        if let Some(value) = patch.tenant {
            record.tenant = Some(value);
        }
        if let Some(value) = patch.labels {
            record.labels = value;
        }
        if let Some(value) = patch.metadata {
            record.metadata = value;
        }
        record.updated_at = unix_now();
        records.insert(name.to_owned(), record.clone());
        self.persist_locked(&records)?;
        Ok(record)
    }

    pub fn update(&self, name: &str, spec: PathSpec) -> Result<PathRecord, PathError> {
        validate_name(name)?;
        let mut records = self
            .records
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(existing) = records.get(name) else {
            return Err(PathError::NotFound(name.into()));
        };
        if existing.source == PathSource::Config {
            return Err(PathError::ReadOnly(name.into()));
        }
        let mut record = spec.into_record(name, existing.source);
        record.created_at = existing.created_at;
        records.insert(name.to_owned(), record.clone());
        self.persist_locked(&records)?;
        Ok(record)
    }

    pub fn remove(&self, name: &str) -> Result<PathRecord, PathError> {
        let mut records = self
            .records
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(existing) = records.get(name) else {
            return Err(PathError::NotFound(name.into()));
        };
        if existing.source == PathSource::Config {
            return Err(PathError::ReadOnly(name.into()));
        }
        let record = records.remove(name).expect("checked above");
        self.persist_locked(&records)?;
        Ok(record)
    }

    /// Register a path seen publishing, unless a concrete record or a config
    /// rule already covers it.
    pub fn ensure_dynamic(&self, name: &str, rule: &PathRule) -> PathRecord {
        {
            let records = self
                .records
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(record) = records.get(name) {
                return record.clone();
            }
        }
        let record = PathSpec::from_rule(rule, PathSource::Dynamic, name);
        let mut records = self
            .records
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        records.insert(name.to_owned(), record.clone());
        let _ = self.persist_locked(&records);
        record
    }

    pub fn remove_expired(&self) -> Vec<String> {
        let now = unix_now();
        let mut records = self
            .records
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let expired: Vec<String> = records
            .iter()
            .filter(|(_, record)| {
                record.source == PathSource::Dynamic
                    && record.expires_at.is_some_and(|at| at <= now)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in &expired {
            records.remove(name);
        }
        if !expired.is_empty() {
            let _ = self.persist_locked(&records);
        }
        expired
    }

    fn persist_locked(&self, records: &BTreeMap<String, PathRecord>) -> Result<(), PathError> {
        if !self.persist_dynamic && records.values().all(|r| r.source == PathSource::Dynamic) {
            return Ok(());
        }
        let file = StoreFile {
            version: STORE_VERSION,
            paths: records.values().cloned().collect(),
        };
        atomic_write_json(&self.file, &file)
    }
}

fn disabled_rule() -> PathRule {
    PathRule {
        enabled: false,
        publish: false,
        read: false,
        record: false,
        hls: false,
        dash: false,
        flv: false,
        webrtc: false,
        max_readers: 0,
        tenant: None,
        labels: BTreeMap::new(),
    }
}

fn specificity(pattern: &str) -> (usize, usize) {
    let wildcards = pattern.matches('*').count();
    (pattern.len().saturating_sub(wildcards), wildcards)
}

pub fn validate_name(name: &str) -> Result<(), PathError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 512 || name.starts_with('/') || name.ends_with('/') {
        return Err(PathError::Invalid(
            "path name is empty or badly formed".into(),
        ));
    }
    for segment in name.split('/') {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || !segment
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~'))
        {
            return Err(PathError::Invalid(format!(
                "path segment {segment:?} is not allowed"
            )));
        }
    }
    if ["admin", "api", "hls", "llhls", "vod", "whip", "whep", "flv"].contains(&name) {
        return Err(PathError::Invalid(format!("path {name:?} is reserved")));
    }
    Ok(())
}

fn load_file(path: &Path) -> Result<BTreeMap<String, PathRecord>, PathError> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(BTreeMap::new());
    };
    let file: StoreFile = serde_json::from_str(&text)
        .map_err(|error| PathError::Corrupt(format!("{}: {error}", path.display())))?;
    if file.version != STORE_VERSION {
        return Err(PathError::Corrupt(format!(
            "{}: unsupported version {}",
            path.display(),
            file.version
        )));
    }
    Ok(file
        .paths
        .into_iter()
        .map(|record| (record.name.clone(), record))
        .collect())
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), PathError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| PathError::Io(format!("{}: {error}", parent.display())))?;
    }
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| PathError::Io(format!("serialize paths: {error}")))?;
    let temporary = path.with_extension("json.tmp");
    {
        let mut file = fs::File::create(&temporary)
            .map_err(|error| PathError::Io(format!("{}: {error}", temporary.display())))?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| PathError::Io(format!("{}: {error}", temporary.display())))?;
    }
    fs::rename(&temporary, path)
        .map_err(|error| PathError::Io(format!("{}: {error}", path.display())))?;
    Ok(())
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("invalid path: {0}")]
    Invalid(String),
    #[error("path {0} already exists")]
    Conflict(String),
    #[error("path {0} not found")]
    NotFound(String),
    #[error("path {0} comes from the config file and is read-only")]
    ReadOnly(String),
    #[error("path store: {0}")]
    Io(String),
    #[error("path store is corrupt: {0}")]
    Corrupt(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(1);

    fn store() -> (PathStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "rtflow-paths-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut config = Config::default();
        config.server.state_dir = dir.clone();
        config.paths.insert(
            "live/*".into(),
            PathRule {
                record: true,
                ..PathRule::default()
            },
        );
        let store = PathStore::new(&config, true).unwrap();
        (store, dir)
    }

    #[test]
    fn config_rule_resolves_and_api_overrides() {
        let (store, dir) = store();
        let record = store.resolve("/live/cam");
        assert!(record.record);
        assert_eq!(record.source, PathSource::Config);

        let spec = PathSpec {
            read: false,
            record: false,
            ..PathSpec::default()
        };
        store.create("live/cam", spec).unwrap();
        assert!(!store.resolve("live/cam").read);
        assert_eq!(store.resolve("live/cam").source, PathSource::Api);

        let reloaded = {
            let mut config = Config::default();
            config.server.state_dir = dir.clone();
            config.paths.insert("live/*".into(), PathRule::default());
            PathStore::new(&config, true).unwrap()
        };
        assert!(!reloaded.resolve("live/cam").read);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_unsafe_names() {
        for name in ["", "live/../x", "/live", "live//cam", "admin"] {
            assert!(validate_name(name).is_err(), "{name}");
        }
        assert!(validate_name("live/cam-01").is_ok());
    }

    #[test]
    fn expired_dynamic_paths_are_removed() {
        let (store, dir) = store();
        let record = store.ensure_dynamic("live/temp", &PathRule::default());
        assert_eq!(record.source, PathSource::Dynamic);
        {
            let mut records = store.records.write().unwrap();
            records.get_mut("live/temp").unwrap().expires_at = Some(1);
        }
        assert_eq!(store.remove_expired(), vec!["live/temp".to_owned()]);
        assert!(store.get("live/temp").is_none());
        let _ = fs::remove_dir_all(dir);
    }
}
