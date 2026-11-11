//! Object operations through a `Claim`: the `Store` contract (design §5, D40, D43), scoping
//! (§4.1, D36, D48) and the case-collision rule (D6).

use futures::executor::block_on;
use lib_vault::{Error, Key, KeyError, MAX_DEPTH, Vault};
use lib_vault_memory::{MemoryClaimer, MemoryStore};

#[test]
fn write_then_read_round_trips() {
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("session").unwrap()).await.unwrap();

        c.store("notes.md", b"hello".to_vec()).await.unwrap();

        let buf: Vec<u8> = c.load("notes.md").await.unwrap();
        assert_eq!(buf, b"hello");
    });
}

#[test]
fn key_argument_forms_are_equivalent() {
    // D48: `&str`, `&Key` and `Option<&Key>` all name the same object.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

        c.store("b", b"x".to_vec()).await.unwrap();

        let b = Key::new("b").unwrap();
        let buf: Vec<u8> = c.load(&b).await.unwrap();
        assert_eq!(buf, b"x");

        let buf: Vec<u8> = c.load(Some(&b)).await.unwrap();
        assert_eq!(buf, b"x");
    });
}

#[test]
fn none_is_the_claimed_key_itself() {
    // D48: `None` names the claimed object.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a/b").unwrap()).await.unwrap();

        c.store(None, b"self".to_vec()).await.unwrap();

        let buf: Vec<u8> = c.load(None).await.unwrap();
        assert_eq!(buf, b"self");
        c.release().unwrap();

        let mut parent = v.claim(Key::new("a").unwrap()).await.unwrap();
        let buf: Vec<u8> = parent.load("b").await.unwrap();
        assert_eq!(buf, b"self");
    });
}

#[test]
fn keys_are_relative_to_the_claim() {
    // §4.1: a claim on `a` writing `b/c` stores `a/b/c`, visible from other claims.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        c.store("b/c", b"deep".to_vec()).await.unwrap();
        c.release().unwrap();

        let mut c = v.claim(Key::new("a/b/c").unwrap()).await.unwrap();
        let buf: Vec<u8> = c.load(None).await.unwrap();
        assert_eq!(buf, b"deep");
    });
}

#[test]
fn a_claim_cannot_see_objects_outside_its_key() {
    // §4.1, D36: `b` from a claim on `a` is `a/b`, never the top-level `b`.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut outside = v.claim(Key::new("b").unwrap()).await.unwrap();
        outside.store(None, b"outside".to_vec()).await.unwrap();
        outside.release().unwrap();

        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        assert!(matches!(
            c.load::<Vec<u8>>("b").await,
            Err(Error::NotFound { .. })
        ));
        assert!(c.list(None).await.unwrap().is_empty());
    });
}

#[test]
fn reading_a_missing_object_is_not_found() {
    // D43: `load` of a missing key is an error, reported with the full key.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        match c.load::<Vec<u8>>("missing").await.err() {
            Some(Error::NotFound { key }) => assert_eq!(key, Key::new("a/missing").unwrap()),
            other => panic!("expected NotFound, got {other:?}"),
        }
    });
}

#[test]
fn writing_overwrites_existing_data() {
    // D43: `store` creates or overwrites.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

        c.store("f", b"first, longer".to_vec()).await.unwrap();

        c.store("f", b"second".to_vec()).await.unwrap();

        let buf: Vec<u8> = c.load("f").await.unwrap();
        assert_eq!(buf, b"second");
    });
}

#[test]
fn storing_another_type_is_refused() {
    // D86, D87: the memory store holds `Vec<u8>`; any other type is `WrongType`, reported
    // with the full key, and nothing is stored.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

        match c.store("f", String::from("text")).await.err() {
            Some(Error::WrongType { key }) => assert_eq!(key, Key::new("a/f").unwrap()),
            other => panic!("expected WrongType, got {other:?}"),
        }
        // A byte string literal is a `&[u8; N]`, not a `Vec<u8>`.
        assert!(matches!(
            c.store("f", b"bytes").await,
            Err(Error::WrongType { .. })
        ));
        assert!(matches!(
            c.load::<Vec<u8>>("f").await,
            Err(Error::NotFound { .. })
        ));
    });
}

#[test]
fn loading_as_another_type_is_refused() {
    // D86: `load::<T>` of an object that isn't a `T` is `WrongType`, with the full key.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        c.store("f", b"text".to_vec()).await.unwrap();

        match c.load::<String>("f").await.err() {
            Some(Error::WrongType { key }) => assert_eq!(key, Key::new("a/f").unwrap()),
            other => panic!("expected WrongType, got {other:?}"),
        }
        // The object is still there.
        let buf: Vec<u8> = c.load("f").await.unwrap();
        assert_eq!(buf, b"text");
    });
}

#[test]
fn load_returns_a_copy() {
    // D87: each `load` hands out its own copy of the bytes.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        c.store("f", b"v1".to_vec()).await.unwrap();

        let mut first: Vec<u8> = c.load("f").await.unwrap();
        first.extend_from_slice(b" changed");
        c.store("g", first).await.unwrap();

        let again: Vec<u8> = c.load("f").await.unwrap();
        assert_eq!(again, b"v1");
    });
}

#[test]
fn deleting_removes_the_object() {
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        c.store("f", b"x".to_vec()).await.unwrap();

        c.delete("f").await.unwrap();
        assert!(matches!(
            c.load::<Vec<u8>>("f").await,
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

        c.store(None, b"self".to_vec()).await.unwrap();

        c.store("child", b"kept".to_vec()).await.unwrap();

        c.delete(None).await.unwrap();
        assert!(matches!(
            c.load::<Vec<u8>>(None).await,
            Err(Error::NotFound { .. })
        ));

        let buf: Vec<u8> = c.load("child").await.unwrap();
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
            c.store(k, b"_".to_vec()).await.unwrap();
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
        c.store("dirt/w", b"_".to_vec()).await.unwrap();

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
            c.store(k, b"_".to_vec()).await.unwrap();
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
            let err = c.store(bad, b"x".to_vec()).await.expect_err(bad);
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
        let err = c.store(deep.as_str(), b"x".to_vec()).await.err().unwrap();
        assert!(matches!(err, Error::Key(KeyError::TooDeep)), "{err:?}");
    });
}

#[test]
fn case_collisions_are_refused() {
    // D6: `store` must fail if the key differs only by ASCII case from an existing one.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        c.store("Notes.md", b"x".to_vec()).await.unwrap();

        assert!(c.store("notes.md", b"z".to_vec()).await.is_err());
        assert!(c.store("NOTES.MD", b"z".to_vec()).await.is_err());
        // A refused store leaves nothing behind.
        assert_eq!(c.list(None).await.unwrap(), [Key::new("Notes.md").unwrap()]);

        // The same spelling is an overwrite, not a collision.
        c.store("Notes.md", b"y".to_vec()).await.unwrap();

        let buf: Vec<u8> = c.load("Notes.md").await.unwrap();
        assert_eq!(buf, b"y");
    });
}

#[test]
fn case_collisions_compare_the_full_key() {
    // Only a key that differs *only* by case collides; different paths don't.
    block_on(async {
        let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
        let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
        c.store("Dir/f", b"x".to_vec()).await.unwrap();

        assert!(c.store("dir/f", b"z".to_vec()).await.is_err());

        c.store("dir/g", b"y".to_vec()).await.unwrap();
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
        c.store(None, b"shared".to_vec()).await.unwrap();
        c.release().unwrap();

        let mut c = v2.claim(Key::new("a").unwrap()).await.unwrap();
        let buf: Vec<u8> = c.load(None).await.unwrap();
        assert_eq!(buf, b"shared");
    });
}
