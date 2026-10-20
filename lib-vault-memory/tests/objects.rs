//! Object operations through a `Claim`: the `Store` contract (design §5, D40, D43), scoping
//! (§4.1, D36, D48) and the case-collision rule (D6).

use futures::executor::block_on;
use futures::{AsyncReadExt, AsyncWriteExt};
use lib_vault::{Error, Key, KeyError, MAX_DEPTH, Vault};
use lib_vault_memory::{MemoryClaimer, MemoryStore};

#[test]
fn write_then_read_round_trips() {
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("session").unwrap()).await.unwrap();

        let mut w = c.open_write("notes.md").await.unwrap();
        w.write_all(b"hello").await.unwrap();
        w.close().await.unwrap();
        drop(w);

        let mut buf = Vec::new();
        let mut r = c.open_read("notes.md").await.unwrap();
        r.read_to_end(&mut buf).await.unwrap();
        drop(r);
        assert_eq!(buf, b"hello");
    });
}

#[test]
fn key_argument_forms_are_equivalent() {
    // D48: `&str`, `&Key` and `Option<&Key>` all name the same object.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

        let mut w = c.open_write("b").await.unwrap();
        w.write_all(b"x").await.unwrap();
        w.close().await.unwrap();
        drop(w);

        let b = Key::new("b").unwrap();
        let mut buf = Vec::new();
        let mut r = c.open_read(&b).await.unwrap();
        r.read_to_end(&mut buf).await.unwrap();
        drop(r);
        assert_eq!(buf, b"x");

        let mut buf = Vec::new();
        let mut r = c.open_read(Some(&b)).await.unwrap();
        r.read_to_end(&mut buf).await.unwrap();
        drop(r);
        assert_eq!(buf, b"x");
    });
}

#[test]
fn none_is_the_claimed_key_itself() {
    // D48: `None` names the claimed object.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a/b").unwrap()).await.unwrap();

        let mut w = c.open_write(None).await.unwrap();
        w.write_all(b"self").await.unwrap();
        w.close().await.unwrap();
        drop(w);

        let mut buf = Vec::new();
        let mut r = c.open_read(None).await.unwrap();
        r.read_to_end(&mut buf).await.unwrap();
        drop(r);
        assert_eq!(buf, b"self");
        c.release().unwrap();

        let mut parent = v.claim(Key::new("a").unwrap()).await.unwrap();
        let mut buf = Vec::new();
        let mut r = parent.open_read("b").await.unwrap();
        r.read_to_end(&mut buf).await.unwrap();
        drop(r);
        assert_eq!(buf, b"self");
    });
}

#[test]
fn keys_are_relative_to_the_claim() {
    // §4.1: a claim on `a` writing `b/c` stores `a/b/c`, visible from other claims.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        let mut w = c.open_write("b/c").await.unwrap();
        w.write_all(b"deep").await.unwrap();
        w.close().await.unwrap();
        drop(w);
        c.release().unwrap();

        let mut c = v.claim(Key::new("a/b/c").unwrap()).await.unwrap();
        let mut buf = Vec::new();
        let mut r = c.open_read(None).await.unwrap();
        r.read_to_end(&mut buf).await.unwrap();
        drop(r);
        assert_eq!(buf, b"deep");
    });
}

#[test]
fn a_claim_cannot_see_objects_outside_its_key() {
    // §4.1, D36: `b` from a claim on `a` is `a/b`, never the top-level `b`.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut outside = v.claim(Key::new("b").unwrap()).await.unwrap();
        let mut w = outside.open_write(None).await.unwrap();
        w.write_all(b"outside").await.unwrap();
        w.close().await.unwrap();
        drop(w);
        outside.release().unwrap();

        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        assert!(matches!(
            c.open_read("b").await,
            Err(Error::NotFound { .. })
        ));
        assert!(c.list(None).await.unwrap().is_empty());
    });
}

#[test]
fn reading_a_missing_object_is_not_found() {
    // D43: `open_read` of a missing key is an error, reported with the full key.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        match c.open_read("missing").await.err() {
            Some(Error::NotFound { key }) => assert_eq!(key, Key::new("a/missing").unwrap()),
            other => panic!("expected NotFound, got {other:?}"),
        }
    });
}

#[test]
fn writing_overwrites_existing_data() {
    // D43: `open_write` creates or overwrites.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

        let mut w = c.open_write("f").await.unwrap();
        w.write_all(b"first, longer").await.unwrap();
        w.close().await.unwrap();
        drop(w);

        let mut w = c.open_write("f").await.unwrap();
        w.write_all(b"second").await.unwrap();
        w.close().await.unwrap();
        drop(w);

        let mut buf = Vec::new();
        let mut r = c.open_read("f").await.unwrap();
        r.read_to_end(&mut buf).await.unwrap();
        drop(r);
        assert_eq!(buf, b"second");
    });
}

#[test]
fn writes_commit_on_close() {
    // D43: each store documents its commit point; the memory store commits on `close()`.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        let mut w = c.open_write("f").await.unwrap();
        w.write_all(b"pending").await.unwrap();
        w.flush().await.unwrap();
        drop(w); // dropped without close: discarded
        assert!(matches!(
            c.open_read("f").await,
            Err(Error::NotFound { .. })
        ));

        let mut w = c.open_write("f").await.unwrap();
        w.write_all(b"committed").await.unwrap();
        w.close().await.unwrap();
        drop(w);

        let mut buf = Vec::new();
        let mut r = c.open_read("f").await.unwrap();
        r.read_to_end(&mut buf).await.unwrap();
        drop(r);
        assert_eq!(buf, b"committed");
    });
}

#[test]
fn readers_see_a_snapshot() {
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        let mut w = c.open_write("f").await.unwrap();
        w.write_all(b"v1").await.unwrap();
        w.close().await.unwrap();
        drop(w);

        let mut r = c.open_read("f").await.unwrap();
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"v1");
    });
}

#[test]
fn deleting_removes_the_object() {
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        let mut w = c.open_write("f").await.unwrap();
        w.write_all(b"x").await.unwrap();
        w.close().await.unwrap();
        drop(w);

        c.delete("f").await.unwrap();
        assert!(matches!(
            c.open_read("f").await,
            Err(Error::NotFound { .. })
        ));
    });
}

#[test]
fn deleting_a_missing_object_succeeds() {
    // D43: like HTTP DELETE.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        c.delete("missing").await.unwrap();
        c.delete(None).await.unwrap();
    });
}

#[test]
fn delete_none_deletes_the_claimed_object() {
    // D48, P7: `claim.delete(None)` deletes the claimed object itself.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

        let mut w = c.open_write(None).await.unwrap();
        w.write_all(b"self").await.unwrap();
        w.close().await.unwrap();
        drop(w);

        let mut w = c.open_write("child").await.unwrap();
        w.write_all(b"kept").await.unwrap();
        w.close().await.unwrap();
        drop(w);

        c.delete(None).await.unwrap();
        assert!(matches!(
            c.open_read(None).await,
            Err(Error::NotFound { .. })
        ));

        let mut buf = Vec::new();
        let mut r = c.open_read("child").await.unwrap();
        r.read_to_end(&mut buf).await.unwrap();
        drop(r);
        assert_eq!(buf, b"kept");
    });
}

#[test]
fn list_returns_keys_relative_to_the_prefix() {
    // D40: everything beneath the key, relative to it; `None` lists the whole claim.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("root").unwrap()).await.unwrap();
        for k in ["x", "dir/y", "dir/sub/z", "dirt/w"] {
            let mut w = c.open_write(k).await.unwrap();
            w.write_all(b"_").await.unwrap();
            w.close().await.unwrap();
            drop(w);
        }

        let keys = c.list(None).await.unwrap();
        assert_eq!(
            keys.iter().map(Key::as_str).collect::<Vec<_>>(),
            ["dir/sub/z", "dir/y", "dirt/w", "x"]
        );
        let keys = c.list("dir").await.unwrap();
        assert_eq!(
            keys.iter().map(Key::as_str).collect::<Vec<_>>(),
            ["sub/z", "y"]
        );
        let keys = c.list("dir/sub").await.unwrap();
        assert_eq!(keys.iter().map(Key::as_str).collect::<Vec<_>>(), ["z"]);
        assert!(c.list("nothing").await.unwrap().is_empty());
    });
}

#[test]
fn list_compares_whole_components() {
    // D40: `dir` must not match `dirt`.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("root").unwrap()).await.unwrap();
        let mut w = c.open_write("dirt/w").await.unwrap();
        w.write_all(b"_").await.unwrap();
        w.close().await.unwrap();
        drop(w);

        assert!(c.list("dir").await.unwrap().is_empty());
    });
}

#[test]
fn list_excludes_the_listed_key_itself() {
    // D40: "beneath" the key; the key itself has no relative name.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("root").unwrap()).await.unwrap();
        for k in ["dir", "dir/f"] {
            let mut w = c.open_write(k).await.unwrap();
            w.write_all(b"_").await.unwrap();
            w.close().await.unwrap();
            drop(w);
        }

        let keys = c.list("dir").await.unwrap();
        assert_eq!(keys.iter().map(Key::as_str).collect::<Vec<_>>(), ["f"]);
    });
}

#[test]
fn invalid_string_keys_are_rejected() {
    // D6, D34, D48: a `&str` is validated when the method is called.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        for bad in ["", "../x", "b//c", "CON", "-x", "bad*char"] {
            let err = c.open_write(bad).await.err().expect(bad);
            assert!(matches!(err, Error::Key(_)), "{bad:?}: {err:?}");
        }
    });
}

#[test]
fn keys_past_the_depth_cap_are_rejected() {
    // D6, D35: joining the claim key and the relative key may exceed `MAX_DEPTH`.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a/b").unwrap()).await.unwrap();
        let deep = vec!["x"; MAX_DEPTH - 1].join("/");
        let err = c.open_write(deep.as_str()).await.err().unwrap();
        assert!(matches!(err, Error::Key(KeyError::TooDeep)), "{err:?}");
    });
}

#[test]
fn case_collisions_are_refused() {
    // D6: `open_write` must fail if the key differs only by ASCII case from an existing one.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        let mut w = c.open_write("Notes.md").await.unwrap();
        w.write_all(b"x").await.unwrap();
        w.close().await.unwrap();
        drop(w);

        assert!(c.open_write("notes.md").await.is_err());
        assert!(c.open_write("NOTES.MD").await.is_err());

        // The same spelling is an overwrite, not a collision.
        let mut w = c.open_write("Notes.md").await.unwrap();
        w.write_all(b"y").await.unwrap();
        w.close().await.unwrap();
        drop(w);

        let mut buf = Vec::new();
        let mut r = c.open_read("Notes.md").await.unwrap();
        r.read_to_end(&mut buf).await.unwrap();
        drop(r);
        assert_eq!(buf, b"y");
    });
}

#[test]
fn case_collisions_compare_the_full_key() {
    // Only a key that differs *only* by case collides; different paths don't.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        let mut w = c.open_write("Dir/f").await.unwrap();
        w.write_all(b"x").await.unwrap();
        w.close().await.unwrap();
        drop(w);

        assert!(c.open_write("dir/f").await.is_err());

        let mut w = c.open_write("dir/g").await.unwrap();
        w.write_all(b"y").await.unwrap();
        w.close().await.unwrap();
        drop(w);
    });
}

#[test]
fn stores_can_be_shared_between_vaults() {
    // A cloned `MemoryStore` refers to the same objects.
    block_on(async {
        let store = MemoryStore::new();
        let claimer = MemoryClaimer::new();
        let v1 = Vault::new(store.clone(), claimer.clone());
        let v2 = Vault::new(store, claimer);

        let mut c = v1.claim(Key::new("a").unwrap()).await.unwrap();
        let mut w = c.open_write(None).await.unwrap();
        w.write_all(b"shared").await.unwrap();
        w.close().await.unwrap();
        drop(w);
        c.release().unwrap();

        let mut c = v2.claim(Key::new("a").unwrap()).await.unwrap();
        let mut buf = Vec::new();
        let mut r = c.open_read(None).await.unwrap();
        r.read_to_end(&mut buf).await.unwrap();
        drop(r);
        assert_eq!(buf, b"shared");
    });
}
