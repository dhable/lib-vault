//! Typed values through a `Claim`: the `Store` contract (design §5, D40, D43, D48), scoping
//! (D36), the case rule (D6) and the SQLite handles (D78), against real database files.

use crate::app::{AppDispatcher, AppValue, Doc, User};
use lib_vault::{Error, Key, Vault};
use lib_vault_memory::MemoryClaimer;
use lib_vault_sqlite::{SqliteClaimer, SqliteError, SqliteStore};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool};

#[tokio::test(flavor = "multi_thread")]
async fn write_then_read_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("docs/a").unwrap()).await.unwrap();

    let doc = Doc {
        title: "Hello".into(),
        body: "world".into(),
    };
    let mut w = c.open_write(None).await.unwrap();
    w.put(doc.clone());
    w.close().await.unwrap();

    // As one of the app's types.
    let r = c.open_read(None).await.unwrap();
    assert_eq!(r.key(), &Key::new("docs/a").unwrap());
    let read: Doc = r.try_as().unwrap();
    assert_eq!(read, doc);

    // As the dispatcher's value type.
    let r = c.open_read(None).await.unwrap();
    assert_eq!(r.value(), &AppValue::Doc(doc.clone()));
    assert_eq!(r.into_value(), AppValue::Doc(doc));
}

#[tokio::test(flavor = "multi_thread")]
async fn values_are_rows_in_the_apps_tables() {
    // The dispatcher maps each struct onto its own table's columns, under the full key.
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool.clone()).await.unwrap();
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("team").unwrap()).await.unwrap();

    let mut w = c.open_write("readme").await.unwrap();
    w.put(Doc {
        title: "About".into(),
        body: "We build vaults.".into(),
    });
    w.close().await.unwrap();

    let mut w = c.open_write("people/ada").await.unwrap();
    w.put(User {
        name: "Ada".into(),
        age: 36,
    });
    w.close().await.unwrap();

    let doc = sqlx::query("SELECT title, body FROM docs WHERE key = 'team/readme'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(doc.get::<String, _>("title"), "About");
    assert_eq!(doc.get::<String, _>("body"), "We build vaults.");

    let user = sqlx::query("SELECT name, age FROM users WHERE key = 'team/people/ada'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(user.get::<String, _>("name"), "Ada");
    assert_eq!(user.get::<i64, _>("age"), 36);
}

#[tokio::test(flavor = "multi_thread")]
async fn creating_the_store_runs_the_dispatchers_migration() {
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    // Running it again on an existing database is fine.
    SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();

    let tables: Vec<String> =
        sqlx::query("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|row| row.get("name"))
            .collect();
    assert_eq!(tables, ["docs", "objects", "users"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn key_forms_and_none_name_the_same_value() {
    // D48: `None` is the claimed key; `&str`, `&Key` and `Option<&Key>` are relative to it.
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v = Vault::new(store, claimer);

    let mut c = v.claim(Key::new("x/y").unwrap()).await.unwrap();
    let mut w = c.open_write(None).await.unwrap();
    w.put(User {
        name: "self".into(),
        age: 1,
    });
    w.close().await.unwrap();
    c.release().unwrap();

    let mut parent = v.claim(Key::new("x").unwrap()).await.unwrap();
    let y = Key::new("y").unwrap();
    let by_str: User = parent.open_read("y").await.unwrap().try_as().unwrap();
    let by_key: User = parent.open_read(&y).await.unwrap().try_as().unwrap();
    let by_option: User = parent.open_read(Some(&y)).await.unwrap().try_as().unwrap();
    assert_eq!(by_str.name, "self");
    assert_eq!(by_key, by_str);
    assert_eq!(by_option, by_str);
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_a_missing_value_reports_the_full_key() {
    // D43: `open_read` of a missing key is an error, at open time.
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    match c.open_read("missing").await.err() {
        Some(Error::NotFound { key }) => assert_eq!(key, Key::new("a/missing").unwrap()),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_as_the_wrong_type_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    let mut w = c.open_write("u").await.unwrap();
    w.put(User {
        name: "Ada".into(),
        age: 36,
    });
    w.close().await.unwrap();

    let err = c.open_read("u").await.unwrap().try_as::<Doc>().unwrap_err();
    let Error::StoreError(source) = &err else {
        panic!("expected a store error, got {err:?}");
    };
    match source.downcast_ref::<SqliteError>() {
        Some(SqliteError::WrongType { key }) => assert_eq!(*key, Key::new("a/u").unwrap()),
        other => panic!("expected WrongType, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_commit_on_close() {
    // D43: the commit point is `close()`. Dropping the writer, or closing it without a
    // `put`, leaves the key as it was; the last `put` wins.
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    let mut w = c.open_write("f").await.unwrap();
    w.put(Doc {
        title: "never".into(),
        body: "committed".into(),
    });
    drop(w);
    assert!(matches!(
        c.open_read("f").await,
        Err(Error::NotFound { .. })
    ));

    let w = c.open_write("f").await.unwrap();
    w.close().await.unwrap();
    assert!(matches!(
        c.open_read("f").await,
        Err(Error::NotFound { .. })
    ));

    let mut w = c.open_write("f").await.unwrap();
    w.put(Doc {
        title: "first".into(),
        body: String::new(),
    });
    w.put(Doc {
        title: "second".into(),
        body: String::new(),
    });
    w.close().await.unwrap();
    let doc: Doc = c.open_read("f").await.unwrap().try_as().unwrap();
    assert_eq!(doc.title, "second");
}

#[tokio::test(flavor = "multi_thread")]
async fn overwriting_replaces_the_value_and_its_type() {
    // D43: `open_write` creates or overwrites, here with a value of another kind.
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool.clone()).await.unwrap();
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    let mut w = c.open_write("f").await.unwrap();
    w.put(Doc {
        title: "a doc".into(),
        body: String::new(),
    });
    w.close().await.unwrap();

    let mut w = c.open_write("f").await.unwrap();
    w.put(User {
        name: "now a user".into(),
        age: 7,
    });
    w.close().await.unwrap();

    let user: User = c.open_read("f").await.unwrap().try_as().unwrap();
    assert_eq!(user.name, "now a user");
    let docs: i64 = sqlx::query("SELECT count(*) AS n FROM docs")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("n");
    assert_eq!(docs, 0, "the old doc row is gone");
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_semantics() {
    // D43: delete removes the value, and deleting a missing key succeeds (like HTTP DELETE).
    // D48: `delete(None)` deletes the claimed key's value, not the values beneath it.
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool.clone()).await.unwrap();
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    let mut w = c.open_write(None).await.unwrap();
    w.put(Doc {
        title: "self".into(),
        body: String::new(),
    });
    w.close().await.unwrap();

    let mut w = c.open_write("child").await.unwrap();
    w.put(User {
        name: "kept".into(),
        age: 1,
    });
    w.close().await.unwrap();

    c.delete(None).await.unwrap();
    assert!(matches!(
        c.open_read(None).await,
        Err(Error::NotFound { .. })
    ));
    let kept: User = c.open_read("child").await.unwrap().try_as().unwrap();
    assert_eq!(kept.name, "kept");

    c.delete("child").await.unwrap();
    assert!(matches!(
        c.open_read("child").await,
        Err(Error::NotFound { .. })
    ));
    c.delete("missing").await.unwrap();

    let rows: i64 = sqlx::query("SELECT count(*) AS n FROM objects")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("n");
    assert_eq!(rows, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_returns_relative_keys_by_whole_components() {
    // D40: everything beneath the key, relative to it; `None` lists the whole claim.
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("root").unwrap()).await.unwrap();
    for k in ["x", "dir", "dir/y", "dir/sub/z", "dirt/w"] {
        let mut w = c.open_write(k).await.unwrap();
        w.put(Doc {
            title: k.into(),
            body: String::new(),
        });
        w.close().await.unwrap();
    }

    let keys = c.list(None).await.unwrap();
    assert_eq!(
        keys.iter().map(Key::as_str).collect::<Vec<_>>(),
        ["dir", "dir/sub/z", "dir/y", "dirt/w", "x"]
    );
    let keys = c.list("dir").await.unwrap();
    assert_eq!(
        keys.iter().map(Key::as_str).collect::<Vec<_>>(),
        ["sub/z", "y"]
    );
    assert!(c.list("di").await.unwrap().is_empty());
    assert!(c.list("x").await.unwrap().is_empty());
    assert!(c.list("nothing").await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn claims_only_see_their_own_subtree() {
    // D36: `b` from a claim on `a` is `a/b`, never the top-level `b`.
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v = Vault::new(store, claimer);

    let mut outside = v.claim(Key::new("b").unwrap()).await.unwrap();
    let mut w = outside.open_write(None).await.unwrap();
    w.put(Doc {
        title: "outside".into(),
        body: String::new(),
    });
    w.close().await.unwrap();
    outside.release().unwrap();

    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    assert!(matches!(
        c.open_read("b").await,
        Err(Error::NotFound { .. })
    ));
    assert!(c.list(None).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn case_collisions_are_refused() {
    // D6: the app's dispatcher refuses a key that differs only by ASCII case; the error
    // reaches the caller as `SqliteError::Dispatch`, and the existing value is untouched.
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    let mut w = c.open_write("Notes").await.unwrap();
    w.put(Doc {
        title: "original".into(),
        body: String::new(),
    });
    w.close().await.unwrap();

    let mut w = c.open_write("notes").await.unwrap();
    w.put(Doc {
        title: "collides".into(),
        body: String::new(),
    });
    let err = w.close().await.unwrap_err();
    let Error::StoreError(source) = &err else {
        panic!("expected a store error, got {err:?}");
    };
    let Some(SqliteError::Dispatch(cause)) = source.downcast_ref::<SqliteError>() else {
        panic!("expected a dispatcher error, got {source:?}");
    };
    let sql = cause.downcast_ref::<sqlx::Error>().unwrap();
    assert!(sql.as_database_error().unwrap().is_unique_violation());

    // The failed write's transaction rolled back: the original is intact.
    let doc: Doc = c.open_read("Notes").await.unwrap().try_as().unwrap();
    assert_eq!(doc.title, "original");
    assert!(matches!(
        c.open_read("notes").await,
        Err(Error::NotFound { .. })
    ));

    // The same spelling is an overwrite, not a collision.
    let mut w = c.open_write("Notes").await.unwrap();
    w.put(Doc {
        title: "updated".into(),
        body: String::new(),
    });
    w.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatcher_errors_reach_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool.clone()).await.unwrap();
    let v = Vault::new(store, claimer);

    // A row the app's dispatcher doesn't understand.
    sqlx::query("INSERT INTO objects (key, kind) VALUES ('a/odd', 'bogus')")
        .execute(&pool)
        .await
        .unwrap();

    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    let err = c.open_read("odd").await.err().unwrap();
    let Error::StoreError(source) = &err else {
        panic!("expected a store error, got {err:?}");
    };
    let Some(SqliteError::Dispatch(cause)) = source.downcast_ref::<SqliteError>() else {
        panic!("expected a dispatcher error, got {source:?}");
    };
    assert!(cause.to_string().contains("unknown kind"), "{cause}");
}

#[tokio::test(flavor = "multi_thread")]
async fn values_persist_in_the_database_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");

    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool.clone()).await.unwrap();
    let v = Vault::new(store, claimer.clone());
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    let mut w = c.open_write(None).await.unwrap();
    w.put(Doc {
        title: "durable".into(),
        body: String::new(),
    });
    w.close().await.unwrap();
    c.release().unwrap();
    // A clean shutdown: the vault, then the claimer's rows, then the pool.
    v.shutdown().await;
    claimer.shutdown().await.unwrap();
    pool.close().await;

    let options = SqliteConnectOptions::new()
        .filename(&path)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    let doc: Doc = c.open_read(None).await.unwrap().try_as().unwrap();
    assert_eq!(doc.title, "durable");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writers_on_one_database() {
    // Many claims writing at once, each through its own pooled connection: `BEGIN IMMEDIATE`
    // queues them on SQLite's write lock instead of failing.
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v = Vault::new(store, claimer);

    let tasks: Vec<_> = (0..16)
        .map(|i| {
            let v = v.clone();
            tokio::spawn(async move {
                let mut c = v
                    .claim(Key::new(&format!("items/{i}")).unwrap())
                    .await
                    .unwrap();
                for round in 0..5 {
                    let mut w = c.open_write(None).await.unwrap();
                    w.put(User {
                        name: format!("item {i}"),
                        age: round,
                    });
                    w.close().await.unwrap();
                }
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }

    let mut c = v.claim(Key::new("items").unwrap()).await.unwrap();
    assert_eq!(c.list(None).await.unwrap().len(), 16);
    let last: User = c.open_read("3").await.unwrap().try_as().unwrap();
    assert_eq!(last.age, 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_store_works_with_any_claimer() {
    // D75-style independence: the store relies on nothing the SQLite claimer does.
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let v = Vault::new(store, MemoryClaimer::new());

    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    let mut w = c.open_write("f").await.unwrap();
    w.put(Doc {
        title: "memory-claimed".into(),
        body: String::new(),
    });
    w.close().await.unwrap();
    let doc: Doc = c.open_read("f").await.unwrap().try_as().unwrap();
    assert_eq!(doc.title, "memory-claimed");
    assert!(matches!(
        v.claim(Key::new("a/f").unwrap()).await,
        Err(Error::AlreadyClaimed { .. })
    ));

    let claim_tables: i64 =
        sqlx::query("SELECT count(*) AS n FROM sqlite_master WHERE name LIKE 'vault_claim%'")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("n");
    assert_eq!(claim_tables, 0);
}
