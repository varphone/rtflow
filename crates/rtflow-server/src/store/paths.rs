use super::{Store, StoreError, StoreResult};
use crate::paths::{PathRecord, PathSource};
use rusqlite::{OptionalExtension, params};
use std::collections::BTreeMap;

pub(crate) fn load_all(store: &Store) -> StoreResult<Vec<PathRecord>> {
    let connection = store.connection();
    let mut statement = connection.prepare(
        "SELECT name, enabled, publish, read, record, hls, dash, flv, webrtc,
                max_readers, tenant, labels, metadata, source, created_at,
                updated_at, expires_at, revision
         FROM paths ORDER BY name",
    )?;
    let rows = statement.query_map([], row_to_record)?;
    let mut records = Vec::new();
    for row in rows {
        records.push(row?);
    }
    Ok(records)
}

pub(crate) fn insert(store: &Store, record: &PathRecord) -> StoreResult<()> {
    let connection = store.connection();
    connection.execute(
        "INSERT INTO paths (name, enabled, publish, read, record, hls, dash, flv,
                            webrtc, max_readers, tenant, labels, metadata, source,
                            created_at, updated_at, expires_at, revision)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
        params![
            record.name,
            record.enabled as i64,
            record.publish as i64,
            record.read as i64,
            record.record as i64,
            record.hls as i64,
            record.dash as i64,
            record.flv as i64,
            record.webrtc as i64,
            record.max_readers as i64,
            record.tenant,
            json(&record.labels)?,
            record.metadata.to_string(),
            source_str(record.source),
            record.created_at as i64,
            record.updated_at as i64,
            record.expires_at.map(|value| value as i64),
            record.revision as i64,
        ],
    )?;
    Ok(())
}

/// Insert or replace a record by name, including its source. Used when a
/// dynamic path is upgraded to an API record by a pull source.
pub(crate) fn upsert(store: &Store, record: &PathRecord) -> StoreResult<()> {
    let connection = store.connection();
    connection.execute(
        "INSERT INTO paths (name, enabled, publish, read, record, hls, dash, flv,
                            webrtc, max_readers, tenant, labels, metadata, source,
                            created_at, updated_at, expires_at, revision)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
         ON CONFLICT(name) DO UPDATE SET
            enabled=excluded.enabled,
            publish=excluded.publish,
            read=excluded.read,
            record=excluded.record,
            hls=excluded.hls,
            dash=excluded.dash,
            flv=excluded.flv,
            webrtc=excluded.webrtc,
            max_readers=excluded.max_readers,
            tenant=excluded.tenant,
            labels=excluded.labels,
            metadata=excluded.metadata,
            source=excluded.source,
            updated_at=excluded.updated_at,
            expires_at=excluded.expires_at,
            revision=excluded.revision",
        params![
            record.name,
            record.enabled as i64,
            record.publish as i64,
            record.read as i64,
            record.record as i64,
            record.hls as i64,
            record.dash as i64,
            record.flv as i64,
            record.webrtc as i64,
            record.max_readers as i64,
            record.tenant,
            json(&record.labels)?,
            record.metadata.to_string(),
            source_str(record.source),
            record.created_at as i64,
            record.updated_at as i64,
            record.expires_at.map(|value| value as i64),
            record.revision as i64,
        ],
    )?;
    Ok(())
}

pub(crate) fn update(store: &Store, record: &PathRecord, expected: Option<u64>) -> StoreResult<()> {
    let connection = store.connection();
    let changed = connection.execute(
        "UPDATE paths SET enabled=?2, publish=?3, read=?4, record=?5, hls=?6,
                          dash=?7, flv=?8, webrtc=?9, max_readers=?10, tenant=?11,
                          labels=?12, metadata=?13, updated_at=?14, expires_at=?15,
                          revision=?16
         WHERE name=?1 AND (?17 IS NULL OR revision=?17)",
        params![
            record.name,
            record.enabled as i64,
            record.publish as i64,
            record.read as i64,
            record.record as i64,
            record.hls as i64,
            record.dash as i64,
            record.flv as i64,
            record.webrtc as i64,
            record.max_readers as i64,
            record.tenant,
            json(&record.labels)?,
            record.metadata.to_string(),
            record.updated_at as i64,
            record.expires_at.map(|value| value as i64),
            record.revision as i64,
            expected.map(|value| value as i64),
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::Conflict(record.name.clone()));
    }
    Ok(())
}

pub(crate) fn delete(store: &Store, name: &str, expected: Option<u64>) -> StoreResult<()> {
    let changed = store.connection().execute(
        "DELETE FROM paths WHERE name=?1 AND (?2 IS NULL OR revision=?2)",
        params![name, expected.map(|value| value as i64)],
    )?;
    if changed == 0 {
        return Err(StoreError::NotFound(name.into()));
    }
    Ok(())
}

pub(crate) fn purge_expired(store: &Store, now: u64) -> StoreResult<Vec<String>> {
    let connection = store.connection();
    let mut statement = connection.prepare(
        "SELECT name FROM paths WHERE source='dynamic' AND expires_at IS NOT NULL AND expires_at <= ?1",
    )?;
    let expired: Vec<String> = statement
        .query_map([now as i64], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    if !expired.is_empty() {
        connection.execute(
            "DELETE FROM paths WHERE source='dynamic' AND expires_at IS NOT NULL AND expires_at <= ?1",
            [now as i64],
        )?;
    }
    Ok(expired)
}

#[allow(dead_code)]
pub(crate) fn get(store: &Store, name: &str) -> StoreResult<Option<PathRecord>> {
    let connection = store.connection();
    Ok(connection
        .query_row(
            "SELECT name, enabled, publish, read, record, hls, dash, flv, webrtc,
                    max_readers, tenant, labels, metadata, source, created_at,
                    updated_at, expires_at, revision
             FROM paths WHERE name=?1",
            [name],
            row_to_record,
        )
        .optional()?)
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<PathRecord> {
    let labels: String = row.get(11)?;
    let metadata: String = row.get(12)?;
    let source: String = row.get(13)?;
    Ok(PathRecord {
        name: row.get(0)?,
        enabled: row.get::<_, i64>(1)? != 0,
        publish: row.get::<_, i64>(2)? != 0,
        read: row.get::<_, i64>(3)? != 0,
        record: row.get::<_, i64>(4)? != 0,
        hls: row.get::<_, i64>(5)? != 0,
        dash: row.get::<_, i64>(6)? != 0,
        flv: row.get::<_, i64>(7)? != 0,
        webrtc: row.get::<_, i64>(8)? != 0,
        max_readers: row.get::<_, i64>(9)? as usize,
        tenant: row.get(10)?,
        labels: serde_json::from_str(&labels).unwrap_or_else(|_| BTreeMap::new()),
        metadata: serde_json::from_str(&metadata).unwrap_or(serde_json::Value::Null),
        source: source_from_str(&source),
        created_at: row.get::<_, i64>(14)? as u64,
        updated_at: row.get::<_, i64>(15)? as u64,
        expires_at: row.get::<_, Option<i64>>(16)?.map(|value| value as u64),
        revision: row.get::<_, i64>(17)? as u64,
    })
}

fn json<T: serde::Serialize>(value: &T) -> StoreResult<String> {
    serde_json::to_string(value).map_err(|error| StoreError::Data(error.to_string()))
}

fn source_str(source: PathSource) -> &'static str {
    match source {
        PathSource::Api => "api",
        PathSource::Dynamic => "dynamic",
        PathSource::Config => "api",
    }
}

fn source_from_str(source: &str) -> PathSource {
    match source {
        "dynamic" => PathSource::Dynamic,
        _ => PathSource::Api,
    }
}
