use super::{Store, StoreError, StoreResult};
use crate::config::{SrtPull, SrtPush};
use rusqlite::{OptionalExtension, params};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SrtDirection {
    Pull,
    Push,
}

impl SrtDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pull => "pull",
            Self::Push => "push",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SrtRecord {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub direction: SrtDirection,
    pub remote: String,
    pub stream_id: Option<String>,
    pub mount: Option<String>,
    pub passphrase: Option<String>,
    pub pbkeylen: u32,
    pub latency_ms: Option<u32>,
    #[allow(dead_code)]
    pub revision: u64,
}

impl SrtRecord {
    pub(crate) fn pull(&self) -> SrtPull {
        SrtPull {
            remote: self.remote.clone(),
            stream_id: self.stream_id.clone(),
            mount: self.mount.clone(),
            passphrase: self.passphrase.clone(),
            pbkeylen: self.pbkeylen,
            latency_ms: self.latency_ms,
        }
    }

    pub(crate) fn push(&self) -> SrtPush {
        SrtPush {
            mount: self.mount.clone().unwrap_or_default(),
            remote: self.remote.clone(),
            stream_id: self.stream_id.clone(),
            passphrase: self.passphrase.clone(),
            pbkeylen: self.pbkeylen,
            latency_ms: self.latency_ms,
        }
    }
}

pub(crate) fn load_all(store: &Store) -> StoreResult<Vec<SrtRecord>> {
    let connection = store.connection();
    let mut statement = connection.prepare(
        "SELECT id, name, direction, enabled, remote, stream_id, mount,
                passphrase_enc, pbkeylen, latency_ms, revision
         FROM srt_endpoints ORDER BY created_at, id",
    )?;
    let rows = statement.query_map([], |row| row_to_record(store, row))?;
    let mut records = Vec::new();
    for row in rows {
        records.push(row?);
    }
    Ok(records)
}

pub(crate) fn insert(store: &Store, record: &SrtRecord) -> StoreResult<()> {
    let sealed = seal(store, record)?;
    let now = crate::paths::unix_now() as i64;
    store.connection().execute(
        "INSERT INTO srt_endpoints (id, name, direction, enabled, remote, stream_id,
                                    mount, passphrase_enc, pbkeylen, latency_ms,
                                    created_at, updated_at, revision)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11,1)",
        params![
            record.id,
            record.name,
            record.direction.as_str(),
            record.enabled as i64,
            record.remote,
            record.stream_id,
            record.mount,
            sealed,
            record.pbkeylen as i64,
            record.latency_ms.map(|value| value as i64),
            now,
        ],
    )?;
    Ok(())
}

pub(crate) fn update(store: &Store, record: &SrtRecord, expected: u64) -> StoreResult<()> {
    let sealed = seal(store, record)?;
    let changed = store.connection().execute(
        "UPDATE srt_endpoints SET name=?2, enabled=?3, remote=?4, stream_id=?5,
                                  mount=?6, passphrase_enc=?7, pbkeylen=?8,
                                  latency_ms=?9, updated_at=?10, revision=revision+1
         WHERE id=?1 AND revision=?11",
        params![
            record.id,
            record.name,
            record.enabled as i64,
            record.remote,
            record.stream_id,
            record.mount,
            sealed,
            record.pbkeylen as i64,
            record.latency_ms.map(|value| value as i64),
            crate::paths::unix_now() as i64,
            expected as i64,
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::Conflict(record.id.clone()));
    }
    Ok(())
}

pub(crate) fn delete(store: &Store, id: &str) -> StoreResult<()> {
    let changed = store
        .connection()
        .execute("DELETE FROM srt_endpoints WHERE id=?1", [id])?;
    if changed == 0 {
        return Err(StoreError::NotFound(id.into()));
    }
    Ok(())
}

pub(crate) fn revision(store: &Store, id: &str) -> StoreResult<Option<u64>> {
    let connection = store.connection();
    Ok(connection
        .query_row(
            "SELECT revision FROM srt_endpoints WHERE id=?1",
            [id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .map(|value| value as u64))
}

fn row_to_record(store: &Store, row: &rusqlite::Row<'_>) -> rusqlite::Result<SrtRecord> {
    let encrypted: Option<Vec<u8>> = row.get(7)?;
    let id: String = row.get(0)?;
    let direction = if row.get::<_, String>(2)?.eq_ignore_ascii_case("push") {
        SrtDirection::Push
    } else {
        SrtDirection::Pull
    };
    let passphrase = encrypted.and_then(|bytes| {
        store
            .open_secret(&aad(&id), &bytes)
            .ok()
            .and_then(|plain| String::from_utf8(plain).ok())
    });
    Ok(SrtRecord {
        id,
        name: row.get(1)?,
        direction,
        enabled: row.get::<_, i64>(3)? != 0,
        remote: row.get(4)?,
        stream_id: row.get(5)?,
        mount: row.get(6)?,
        passphrase,
        pbkeylen: row.get::<_, i64>(8)? as u32,
        latency_ms: row.get::<_, Option<i64>>(9)?.map(|value| value as u32),
        revision: row.get::<_, i64>(10)? as u64,
    })
}

fn seal(store: &Store, record: &SrtRecord) -> StoreResult<Option<Vec<u8>>> {
    record
        .passphrase
        .as_ref()
        .map(|passphrase| store.seal(&aad(&record.id), passphrase.as_bytes()))
        .transpose()
}

fn aad(id: &str) -> String {
    format!("srt:{id}:passphrase")
}
