//! Object operations through a `Claim`: the `Store` contract (design §5, D40, D43), scoping
//! (§4.1, D36, D48) and the case-collision rule (D6).

mod common;

use common::*;
use futures::{AsyncReadExt, AsyncWriteExt};
use lib_vault::{Error, KeyError, MAX_DEPTH};

#[test]
fn write_then_read_round_trips() {
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "session").await;
        write(&mut c, "notes.md", b"hello").await;
        assert_eq!(read(&mut c, "notes.md").await.unwrap(), b"hello");
    });
}

#[test]
fn key_argument_forms_are_equivalent() {
    // D48: `&str`, `&Key` and `Option<&Key>` all name the same object.
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "a").await;
        write(&mut c, "b", b"x").await;
        assert_eq!(read(&mut c, &key("b")).await.unwrap(), b"x");
        assert_eq!(read(&mut c, Some(&key("b"))).await.unwrap(), b"x");
    });
}

#[test]
fn none_is_the_claimed_key_itself() {
    // D48: `None` names the claimed object.
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "a/b").await;
        write(&mut c, None, b"self").await;
        assert_eq!(read(&mut c, None).await.unwrap(), b"self");
        c.release().unwrap();

        let mut parent = claim(&v, "a").await;
        assert_eq!(read(&mut parent, "b").await.unwrap(), b"self");
    });
}

#[test]
fn keys_are_relative_to_the_claim() {
    // §4.1: a claim on `a` writing `b/c` stores `a/b/c`, visible from other claims.
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "a").await;
        write(&mut c, "b/c", b"deep").await;
        c.release().unwrap();

        let mut c = claim(&v, "a/b/c").await;
        assert_eq!(read(&mut c, None).await.unwrap(), b"deep");
    });
}

#[test]
fn a_claim_cannot_see_objects_outside_its_key() {
    // §4.1, D36: `b` from a claim on `a` is `a/b`, never the top-level `b`.
    block_on(async {
        let v = vault();
        let mut outside = claim(&v, "b").await;
        write(&mut outside, None, b"outside").await;
        outside.release().unwrap();

        let mut c = claim(&v, "a").await;
        assert!(matches!(
            read(&mut c, "b").await,
            Err(Error::NotFound { .. })
        ));
        assert!(list(&mut c, None).await.is_empty());
    });
}

#[test]
fn reading_a_missing_object_is_not_found() {
    // D43: `open_read` of a missing key is an error, reported with the full key.
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "a").await;
        match read(&mut c, "missing").await {
            Err(Error::NotFound { key: k }) => assert_eq!(k, key("a/missing")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    });
}

#[test]
fn writing_overwrites_existing_data() {
    // D43: `open_write` creates or overwrites.
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "a").await;
        write(&mut c, "f", b"first, longer").await;
        write(&mut c, "f", b"second").await;
        assert_eq!(read(&mut c, "f").await.unwrap(), b"second");
    });
}

#[test]
fn writes_commit_on_close() {
    // D43: each store documents its commit point; the memory store commits on `close()`.
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "a").await;
        let mut w = c.open_write("f").await.unwrap();
        w.write_all(b"pending").await.unwrap();
        w.flush().await.unwrap();
        drop(w); // dropped without close: discarded
        assert!(matches!(
            read(&mut c, "f").await,
            Err(Error::NotFound { .. })
        ));

        write(&mut c, "f", b"committed").await;
        assert_eq!(read(&mut c, "f").await.unwrap(), b"committed");
    });
}

#[test]
fn readers_see_a_snapshot() {
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "a").await;
        write(&mut c, "f", b"v1").await;
        let mut r = c.open_read("f").await.unwrap();
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"v1");
    });
}

#[test]
fn deleting_removes_the_object() {
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "a").await;
        write(&mut c, "f", b"x").await;
        c.delete("f").await.unwrap();
        assert!(matches!(
            read(&mut c, "f").await,
            Err(Error::NotFound { .. })
        ));
    });
}

#[test]
fn deleting_a_missing_object_succeeds() {
    // D43: like HTTP DELETE.
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "a").await;
        c.delete("missing").await.unwrap();
        c.delete(None).await.unwrap();
    });
}

#[test]
fn delete_none_deletes_the_claimed_object() {
    // D48, P7: `claim.delete(None)` deletes the claimed object itself.
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "a").await;
        write(&mut c, None, b"self").await;
        write(&mut c, "child", b"kept").await;
        c.delete(None).await.unwrap();
        assert!(matches!(
            read(&mut c, None).await,
            Err(Error::NotFound { .. })
        ));
        assert_eq!(read(&mut c, "child").await.unwrap(), b"kept");
    });
}

#[test]
fn list_returns_keys_relative_to_the_prefix() {
    // D40: everything beneath the key, relative to it; `None` lists the whole claim.
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "root").await;
        for k in ["x", "dir/y", "dir/sub/z", "dirt/w"] {
            write(&mut c, k, b"_").await;
        }
        assert_eq!(
            list(&mut c, None).await,
            ["dir/sub/z", "dir/y", "dirt/w", "x"]
        );
        assert_eq!(list(&mut c, "dir").await, ["sub/z", "y"]);
        assert_eq!(list(&mut c, "dir/sub").await, ["z"]);
        assert!(list(&mut c, "nothing").await.is_empty());
    });
}

#[test]
fn list_compares_whole_components() {
    // D40: `dir` must not match `dirt`.
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "root").await;
        write(&mut c, "dirt/w", b"_").await;
        assert!(list(&mut c, "dir").await.is_empty());
    });
}

#[test]
fn list_excludes_the_listed_key_itself() {
    // D40: "beneath" the key; the key itself has no relative name.
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "root").await;
        write(&mut c, "dir", b"_").await;
        write(&mut c, "dir/f", b"_").await;
        assert_eq!(list(&mut c, "dir").await, ["f"]);
    });
}

#[test]
fn invalid_string_keys_are_rejected() {
    // D6, D34, D48: a `&str` is validated when the method is called.
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "a").await;
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
        let v = vault();
        let mut c = claim(&v, "a/b").await;
        let deep = vec!["x"; MAX_DEPTH - 1].join("/");
        let err = c.open_write(deep.as_str()).await.err().unwrap();
        assert!(matches!(err, Error::Key(KeyError::TooDeep)), "{err:?}");
    });
}

#[test]
fn case_collisions_are_refused() {
    // D6: `open_write` must fail if the key differs only by ASCII case from an existing one.
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "a").await;
        write(&mut c, "Notes.md", b"x").await;
        assert!(c.open_write("notes.md").await.is_err());
        assert!(c.open_write("NOTES.MD").await.is_err());
        // The same spelling is an overwrite, not a collision.
        write(&mut c, "Notes.md", b"y").await;
        assert_eq!(read(&mut c, "Notes.md").await.unwrap(), b"y");
    });
}

#[test]
fn case_collisions_compare_the_full_key() {
    // Only a key that differs *only* by case collides; different paths don't.
    block_on(async {
        let v = vault();
        let mut c = claim(&v, "a").await;
        write(&mut c, "Dir/f", b"x").await;
        assert!(c.open_write("dir/f").await.is_err());
        write(&mut c, "dir/g", b"y").await;
    });
}

#[test]
fn stores_can_be_shared_between_vaults() {
    // A cloned `MemoryStore` refers to the same objects.
    block_on(async {
        let store = lib_vault_memory::MemoryStore::new();
        let claimer = lib_vault_memory::MemoryClaimer::new();
        let v1 = lib_vault::Vault::new(store.clone(), claimer.clone());
        let v2 = lib_vault::Vault::new(store, claimer);

        let mut c = claim(&v1, "a").await;
        write(&mut c, None, b"shared").await;
        c.release().unwrap();

        let mut c = claim(&v2, "a").await;
        assert_eq!(read(&mut c, None).await.unwrap(), b"shared");
    });
}
