use super::*;

#[test]
fn migrations_and_store_info() {
    let store = Store::open_in_memory().unwrap();
    let info = store.info().unwrap();
    assert_eq!(info.schema_version, 3);
    let columns: i64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('proxy_mounts') WHERE name IN ('outputs','schedule')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(columns, 2);
    let connection = store.connection();
    let table: String = connection
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='paths'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table, "paths");
}

#[test]
fn secrets_are_bound_to_aad() {
    let store = Store::open_in_memory().unwrap();
    let sealed = store.seal("proxy:/cam:upstreams", b"secret").unwrap();
    assert_eq!(
        store.open_secret("proxy:/cam:upstreams", &sealed).unwrap(),
        b"secret"
    );
    assert!(
        store
            .open_secret("proxy:/other:upstreams", &sealed)
            .is_err()
    );
}

#[test]
fn proxy_outputs_roundtrip() {
    use crate::config::{ProxyMount, ProxyOutputs, ProxyUpstream};
    let store = Store::open_in_memory().unwrap();
    let mount = ProxyMount {
        path: "/pull/cam".into(),
        mode: crate::config::ProxyMode::Always,
        outputs: Some(ProxyOutputs {
            dash: true,
            record: true,
            max_readers: 4,
            ..ProxyOutputs::default()
        }),
        schedule: Some(crate::config::ProxySchedule {
            days: vec![
                crate::config::ScheduleDay::Mon,
                crate::config::ScheduleDay::Fri,
            ],
            start: "08:00".into(),
            end: "18:00".into(),
        }),
        upstreams: vec![ProxyUpstream {
            url: "rtsp://127.0.0.1/live".into(),
            ..ProxyUpstream::default()
        }],
        ..ProxyMount::default()
    };
    super::proxies::insert(&store, &mount).unwrap();
    let loaded = super::proxies::load_all(&store).unwrap();
    assert_eq!(loaded, vec![mount]);
}

#[test]
fn legacy_proxy_row_without_outputs_loads_none() {
    let store = Store::open_in_memory().unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO proxy_mounts (path, enabled, mode, idle_grace_ms,
                                       description_refresh_ms, upstreams_enc, outputs,
                                       created_at, updated_at, revision)
             VALUES ('/legacy', 1, 'always', NULL, NULL, x'00', NULL, 1, 1, 1)",
            [],
        )
        .unwrap();
    let loaded = super::proxies::load_all(&store).unwrap();
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0].outputs.is_none());
}

#[test]
fn upgrades_v1_database_and_keeps_legacy_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    let database = state.join("rtflow.db");
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(super::schema::MIGRATION_V1)
            .unwrap();
        connection
            .execute(
                "INSERT INTO proxy_mounts (path, enabled, mode, idle_grace_ms,
                                           description_refresh_ms, upstreams_enc,
                                           created_at, updated_at, revision)
                 VALUES ('/legacy', 1, 'on-demand', NULL, NULL, x'00', 1, 1, 1)",
                [],
            )
            .unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
    }
    let store = Store::open(&crate::config::StorageSection::default(), state).unwrap();
    assert_eq!(store.info().unwrap().schema_version, 3);
    let mounts = super::proxies::load_all(&store).unwrap();
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].path, "/legacy");
    assert!(mounts[0].outputs.is_none());
}
