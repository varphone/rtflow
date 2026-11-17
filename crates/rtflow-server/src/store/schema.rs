pub(super) const VERSION: u32 = 2;

pub(super) const MIGRATION_V1: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS paths (
    name TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL,
    publish INTEGER NOT NULL,
    read INTEGER NOT NULL,
    record INTEGER NOT NULL,
    hls INTEGER NOT NULL,
    dash INTEGER NOT NULL,
    flv INTEGER NOT NULL,
    webrtc INTEGER NOT NULL,
    max_readers INTEGER NOT NULL,
    tenant TEXT,
    labels TEXT NOT NULL DEFAULT '{}',
    metadata TEXT NOT NULL DEFAULT 'null',
    source TEXT NOT NULL CHECK (source IN ('api','dynamic')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    expires_at INTEGER,
    revision INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS paths_source ON paths(source);

CREATE TABLE IF NOT EXISTS stream_keys (
    kid TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    roles TEXT NOT NULL,
    paths TEXT NOT NULL,
    protocols TEXT NOT NULL,
    max_connections INTEGER NOT NULL,
    hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    enabled INTEGER NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS proxy_mounts (
    path TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL,
    mode TEXT NOT NULL,
    idle_grace_ms INTEGER,
    description_refresh_ms INTEGER,
    upstreams_enc BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS srt_endpoints (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('pull','push')),
    enabled INTEGER NOT NULL,
    remote TEXT NOT NULL,
    stream_id TEXT,
    mount TEXT,
    passphrase_enc BLOB,
    pbkeylen INTEGER NOT NULL,
    latency_ms INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS idempotency_keys (
    key TEXT PRIMARY KEY,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    status INTEGER NOT NULL,
    response TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idempotency_created ON idempotency_keys(created_at);
"#;

/// v2: pull-source outputs (live/recording policy projection).
pub(super) const MIGRATION_V2: &str = r#"
ALTER TABLE proxy_mounts ADD COLUMN outputs TEXT;
"#;
