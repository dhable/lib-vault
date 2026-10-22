//! Typed values through a `Claim`: the `Store` contract (design §5, D40, D43, D48), scoping
//! (D36), the case rule (D6) and the ClickHouse handles, against a live server.

use crate::app::{AppDispatcher, AppValue, CaseCollision, Doc, User};
use crate::fresh_database;
use lib_vault::{Error, Key, Vault};
use lib_vault_clickhouse::{
    ClickhouseClaimer, ClickhouseClaimerOptions, ClickhouseError, ClickhouseStore,
};
use lib_vault_memory::MemoryClaimer;

#[tokio::test(flavor = "multi_thread")]
async fn write_then_read_round_trips() {
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
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
    // The dispatcher maps each struct onto its own table's typed columns, under the full key.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client.clone(),
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
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

    let (title, body) = client
        .query("SELECT title, body FROM docs FINAL WHERE key = 'team/readme'")
        .fetch_one::<(String, String)>()
        .await
        .unwrap();
    assert_eq!(title, "About");
    assert_eq!(body, "We build vaults.");

    let (name, age) = client
        .query("SELECT name, age FROM users FINAL WHERE key = 'team/people/ada'")
        .fetch_one::<(String, i64)>()
        .await
        .unwrap();
    assert_eq!(name, "Ada");
    assert_eq!(age, 36);
}

#[tokio::test(flavor = "multi_thread")]
async fn creating_the_store_runs_the_dispatchers_migration() {
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    // Running it again on an existing database is fine.
    ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();

    let tables = client
        .query("SELECT name FROM system.tables WHERE database = currentDatabase() ORDER BY name")
        .fetch_all::<String>()
        .await
        .unwrap();
    assert_eq!(tables, ["docs", "objects", "users"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn key_forms_and_none_name_the_same_value() {
    // D48: `None` is the claimed key; `&str`, `&Key` and `Option<&Key>` are relative to it.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
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
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    match c.open_read("missing").await.err() {
        Some(Error::NotFound { key }) => assert_eq!(key, Key::new("a/missing").unwrap()),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_as_the_wrong_type_is_refused() {
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
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
    match source.downcast_ref::<ClickhouseError>() {
        Some(ClickhouseError::WrongType { key }) => assert_eq!(*key, Key::new("a/u").unwrap()),
        other => panic!("expected WrongType, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_commit_on_close() {
    // D43: the commit point is `close()`. Dropping the writer, or closing it without a
    // `put`, leaves the key as it was; the last `put` wins.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
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
    // D43: `open_write` creates or overwrites, here with a value of another kind; repeated
    // overwrites of one kind always read back the latest.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client.clone(),
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    for round in 0..5 {
        let mut w = c.open_write("f").await.unwrap();
        w.put(Doc {
            title: format!("doc {round}"),
            body: String::new(),
        });
        w.close().await.unwrap();
        let doc: Doc = c.open_read("f").await.unwrap().try_as().unwrap();
        assert_eq!(doc.title, format!("doc {round}"));
    }

    let mut w = c.open_write("f").await.unwrap();
    w.put(User {
        name: "now a user".into(),
        age: 7,
    });
    w.close().await.unwrap();

    let user: User = c.open_read("f").await.unwrap().try_as().unwrap();
    assert_eq!(user.name, "now a user");
    let docs = client
        .query("SELECT count() FROM docs FINAL")
        .fetch_one::<u64>()
        .await
        .unwrap();
    assert_eq!(docs, 0, "the old doc is tombstoned");
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_semantics() {
    // D43: delete removes the value, and deleting a missing key succeeds (like HTTP DELETE).
    // D48: `delete(None)` deletes the claimed key's value, not the values beneath it.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client.clone(),
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
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

    let indexed = client
        .query("SELECT count() FROM objects")
        .fetch_one::<u64>()
        .await
        .unwrap();
    assert_eq!(indexed, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_returns_relative_keys_by_whole_components() {
    // D40: everything beneath the key, relative to it; `None` lists the whole claim.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
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
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
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
    // reaches the caller as `ClickhouseError::Dispatch`, and the existing value is untouched.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
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
    let Some(ClickhouseError::Dispatch(cause)) = source.downcast_ref::<ClickhouseError>() else {
        panic!("expected a dispatcher error, got {source:?}");
    };
    let collision = cause.downcast_ref::<CaseCollision>().unwrap();
    assert_eq!(collision.existing, "a/Notes");

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
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client.clone(),
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v = Vault::new(store, claimer);

    // An index row the app's dispatcher doesn't understand.
    client
        .query("INSERT INTO objects VALUES ('a/odd', 'a/odd', 'bogus')")
        .execute()
        .await
        .unwrap();

    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    let err = c.open_read("odd").await.err().unwrap();
    let Error::StoreError(source) = &err else {
        panic!("expected a store error, got {err:?}");
    };
    let Some(ClickhouseError::Dispatch(cause)) = source.downcast_ref::<ClickhouseError>() else {
        panic!("expected a dispatcher error, got {source:?}");
    };
    assert!(cause.to_string().contains("unknown kind"), "{cause}");
}

#[tokio::test(flavor = "multi_thread")]
async fn values_persist_across_clients() {
    // A new client, store and vault (another process, in effect) reads what the first wrote.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");

    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client.clone(),
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v = Vault::new(store, claimer.clone());
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    let mut w = c.open_write(None).await.unwrap();
    w.put(Doc {
        title: "durable".into(),
        body: String::new(),
    });
    w.close().await.unwrap();
    c.release().unwrap();
    v.shutdown().await;
    claimer.shutdown().await.unwrap();

    let client = crate::server().with_database(&db);
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    let doc: Doc = c.open_read(None).await.unwrap().try_as().unwrap();
    assert_eq!(doc.title, "durable");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writers_on_one_server() {
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
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
    // The store relies on nothing the ClickHouse claimer does.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
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

    let claim_tables = client
        .query(
            "SELECT count() FROM system.tables
             WHERE database = currentDatabase() AND startsWith(name, 'vault_claim')",
        )
        .fetch_one::<u64>()
        .await
        .unwrap();
    assert_eq!(claim_tables, 0);
}
