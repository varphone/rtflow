use super::*;

#[test]
fn migrations_and_store_info() {
    let store = Store::open_in_memory().unwrap();
    let info = store.info().unwrap();
    assert_eq!(info.schema_version, 1);
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
