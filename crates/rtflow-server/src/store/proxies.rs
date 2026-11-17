use super::{Store, StoreError, StoreResult};
use crate::config::{ProxyMode, ProxyMount};
use rusqlite::{OptionalExtension, params};

pub(crate) fn load_all(store: &Store) -> StoreResult<Vec<ProxyMount>> {
    let connection = store.connection();
    let mut statement = connection.prepare(
        "SELECT path, enabled, mode, idle_grace_ms, description_refresh_ms,
                upstreams_enc, outputs, schedule, revision FROM proxy_mounts ORDER BY path",
    )?;
    let rows = statement.query_map([], |row| row_to_mount(store, row))?;
    let mut mounts = Vec::new();
    for row in rows {
        mounts.push(row?);
    }
    Ok(mounts)
}

pub(crate) fn insert(store: &Store, mount: &ProxyMount) -> StoreResult<()> {
    let upstreams = serde_json::to_vec(&mount.upstreams)
        .map_err(|error| StoreError::Data(error.to_string()))?;
    let sealed = store.seal(&aad(&mount.path), &upstreams)?;
    let outputs = mount
        .outputs
        .map(|value| serde_json::to_string(&value))
        .transpose()
        .map_err(|error| StoreError::Data(error.to_string()))?;
    let schedule = mount
        .schedule
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| StoreError::Data(error.to_string()))?;
    let now = crate::paths::unix_now() as i64;
    store.connection().execute(
        "INSERT INTO proxy_mounts (path, enabled, mode, idle_grace_ms,
                                   description_refresh_ms, upstreams_enc, outputs,
                                   schedule, created_at, updated_at, revision)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9,1)",
        params![
            mount.path,
            mount.enabled as i64,
            mode_str(mount.mode),
            mount.idle_grace.map(|value| value.as_millis() as i64),
            mount
                .description_refresh
                .map(|value| value.as_millis() as i64),
            sealed,
            outputs,
            schedule,
            now,
        ],
    )?;
    Ok(())
}

pub(crate) fn update(store: &Store, mount: &ProxyMount, expected: u64) -> StoreResult<()> {
    let upstreams = serde_json::to_vec(&mount.upstreams)
        .map_err(|error| StoreError::Data(error.to_string()))?;
    let sealed = store.seal(&aad(&mount.path), &upstreams)?;
    let outputs = mount
        .outputs
        .map(|value| serde_json::to_string(&value))
        .transpose()
        .map_err(|error| StoreError::Data(error.to_string()))?;
    let schedule = mount
        .schedule
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| StoreError::Data(error.to_string()))?;
    let changed = store.connection().execute(
        "UPDATE proxy_mounts SET enabled=?2, mode=?3, idle_grace_ms=?4,
                                description_refresh_ms=?5, upstreams_enc=?6,
                                outputs=?7, schedule=?8, updated_at=?9,
                                revision=revision+1
         WHERE path=?1 AND revision=?10",
        params![
            mount.path,
            mount.enabled as i64,
            mode_str(mount.mode),
            mount.idle_grace.map(|value| value.as_millis() as i64),
            mount
                .description_refresh
                .map(|value| value.as_millis() as i64),
            sealed,
            outputs,
            schedule,
            crate::paths::unix_now() as i64,
            expected as i64,
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::Conflict(mount.path.clone()));
    }
    Ok(())
}

pub(crate) fn revision(store: &Store, path: &str) -> StoreResult<Option<u64>> {
    let connection = store.connection();
    Ok(connection
        .query_row(
            "SELECT revision FROM proxy_mounts WHERE path=?1",
            [path],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .map(|value| value as u64))
}

pub(crate) fn delete(store: &Store, path: &str) -> StoreResult<()> {
    let changed = store
        .connection()
        .execute("DELETE FROM proxy_mounts WHERE path=?1", [path])?;
    if changed == 0 {
        return Err(StoreError::NotFound(path.into()));
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn get(store: &Store, path: &str) -> StoreResult<Option<ProxyMount>> {
    let connection = store.connection();
    Ok(connection
        .query_row(
            "SELECT path, enabled, mode, idle_grace_ms, description_refresh_ms,
                    upstreams_enc, outputs, schedule, revision FROM proxy_mounts WHERE path=?1",
            [path],
            |row| row_to_mount(store, row),
        )
        .optional()?)
}

fn row_to_mount(store: &Store, row: &rusqlite::Row<'_>) -> rusqlite::Result<ProxyMount> {
    let encrypted: Vec<u8> = row.get(5)?;
    let path: String = row.get(0)?;
    let upstreams = store
        .open_secret(&aad(&path), &encrypted)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let outputs = row
        .get::<_, Option<String>>(6)?
        .and_then(|value| serde_json::from_str(&value).ok());
    let schedule = row
        .get::<_, Option<String>>(7)?
        .and_then(|value| serde_json::from_str(&value).ok());
    Ok(ProxyMount {
        path,
        enabled: row.get::<_, i64>(1)? != 0,
        mode: if row.get::<_, String>(2)?.eq_ignore_ascii_case("always") {
            ProxyMode::Always
        } else {
            ProxyMode::OnDemand
        },
        idle_grace: row
            .get::<_, Option<i64>>(3)?
            .map(|value| std::time::Duration::from_millis(value as u64)),
        description_refresh: row
            .get::<_, Option<i64>>(4)?
            .map(|value| std::time::Duration::from_millis(value as u64)),
        outputs,
        schedule,
        upstreams,
    })
}

fn mode_str(mode: ProxyMode) -> &'static str {
    if mode.is_always() {
        "always"
    } else {
        "on-demand"
    }
}

fn aad(path: &str) -> String {
    format!("proxy:{path}:upstreams")
}
