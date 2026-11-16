use super::{Store, StoreError, StoreResult};
use crate::keys::KeyRecord;
use rusqlite::{OptionalExtension, params};

pub(crate) fn load_all(store: &Store) -> StoreResult<Vec<KeyRecord>> {
    let connection = store.connection();
    let mut statement = connection.prepare(
        "SELECT kid, name, roles, paths, protocols, max_connections, hash,
                created_at, expires_at, enabled, revision
         FROM stream_keys ORDER BY created_at",
    )?;
    let rows = statement.query_map([], row_to_record)?;
    let mut records = Vec::new();
    for row in rows {
        records.push(row?);
    }
    Ok(records)
}

pub(crate) fn insert(store: &Store, record: &KeyRecord) -> StoreResult<()> {
    store.connection().execute(
        "INSERT INTO stream_keys (kid, name, roles, paths, protocols, max_connections,
                                  hash, created_at, expires_at, enabled, revision)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            record.kid,
            record.name,
            json(&record.roles)?,
            json(&record.paths)?,
            json(&record.protocols)?,
            record.max_connections as i64,
            record.hash,
            record.created_at as i64,
            record.expires_at as i64,
            record.enabled as i64,
            record.revision as i64,
        ],
    )?;
    Ok(())
}

pub(crate) fn set_enabled(
    store: &Store,
    kid: &str,
    enabled: bool,
    revision: u64,
) -> StoreResult<()> {
    let changed = store.connection().execute(
        "UPDATE stream_keys SET enabled=?2, revision=?3 WHERE kid=?1 AND revision=?4",
        params![kid, enabled as i64, revision as i64, (revision.saturating_sub(1)) as i64],
    )?;
    if changed == 0 {
        return Err(StoreError::Conflict(kid.into()));
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn get(store: &Store, kid: &str) -> StoreResult<Option<KeyRecord>> {
    let connection = store.connection();
    Ok(connection
        .query_row(
            "SELECT kid, name, roles, paths, protocols, max_connections, hash,
                    created_at, expires_at, enabled, revision
             FROM stream_keys WHERE kid=?1",
            [kid],
            row_to_record,
        )
        .optional()?)
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<KeyRecord> {
    Ok(KeyRecord {
        kid: row.get(0)?,
        name: row.get(1)?,
        roles: parse_json(row.get::<_, String>(2)?),
        paths: parse_json(row.get::<_, String>(3)?),
        protocols: parse_json(row.get::<_, String>(4)?),
        max_connections: row.get::<_, i64>(5)? as usize,
        hash: row.get(6)?,
        created_at: row.get::<_, i64>(7)? as u64,
        expires_at: row.get::<_, i64>(8)? as u64,
        enabled: row.get::<_, i64>(9)? != 0,
        revision: row.get::<_, i64>(10)? as u64,
    })
}

fn parse_json<T: serde::de::DeserializeOwned + Default>(text: String) -> T {
    serde_json::from_str(&text).unwrap_or_default()
}

fn json<T: serde::Serialize>(value: &T) -> StoreResult<String> {
    serde_json::to_string(value).map_err(|error| StoreError::Data(error.to_string()))
}
