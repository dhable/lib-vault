//! Claim exclusion and release: the `Claimer` contract (design §6, D44, D45) and the
//! `ClaimLock` / `Claim::release` rules (§4.4, D11, D39).

use futures::executor::block_on;
use lib_vault::{Error, Key, Vault, io_driver};
use lib_vault_memory::{MemoryClaimer, MemoryStore};

fn refused(v: &Vault<io_driver::AsyncIoDriver>, k: &str) -> bool {
    match block_on(v.claim(Key::new(k).unwrap())) {
        Err(Error::AlreadyClaimed { key: refused }) => {
            assert_eq!(refused, Key::new(k).unwrap());
            true
        }
        Err(other) => panic!("unexpected error for {k}: {other:?}"),
        Ok(_) => false,
    }
}

#[test]
fn the_same_key_cannot_be_claimed_twice() {
    let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
    let _held = block_on(v.claim(Key::new("a/b").unwrap())).unwrap();
    assert!(refused(&v, "a/b"));
}

#[test]
fn descendants_conflict() {
    // D44: while `a/b` is held, `a/b/c` is refused.
    let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
    let _held = block_on(v.claim(Key::new("a/b").unwrap())).unwrap();
    assert!(refused(&v, "a/b/c"));
    assert!(refused(&v, "a/b/c/d"));
}

#[test]
fn ancestors_conflict() {
    // D44: while `a/b` is held, `a` is refused.
    let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
    let _held = block_on(v.claim(Key::new("a/b").unwrap())).unwrap();
    assert!(refused(&v, "a"));
}

#[test]
fn disjoint_keys_do_not_conflict() {
    // D44: siblings and keys that only share a string prefix are independent.
    let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
    let _held = block_on(v.claim(Key::new("a/b").unwrap())).unwrap();
    assert!(!refused(&v, "a/c"));
    assert!(!refused(&v, "a/bc"));
    assert!(!refused(&v, "b"));
}

#[test]
fn many_disjoint_claims_can_be_held_together() {
    let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
    let held: Vec<_> = ["x", "y/1", "y/2", "z"]
        .iter()
        .map(|k| block_on(v.claim(Key::new(k).unwrap())).unwrap())
        .collect();
    assert!(refused(&v, "y"));
    drop(held);
    assert!(!refused(&v, "y"));
}

#[test]
fn release_frees_the_key_immediately() {
    // D39: `Ok` from release means the key can be claimed again right away.
    let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
    let c = block_on(v.claim(Key::new("a").unwrap())).unwrap();
    c.release().unwrap();
    assert!(!refused(&v, "a"));
}

#[test]
fn dropping_a_claim_frees_the_key() {
    // D11: dropping releases the lock if `release` wasn't called.
    let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
    let c = block_on(v.claim(Key::new("a").unwrap())).unwrap();
    drop(c);
    assert!(!refused(&v, "a/b"));
}

#[test]
fn a_refused_claim_holds_nothing() {
    // D47: a failed `try_lock` leaves no lock behind.
    let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
    let held = block_on(v.claim(Key::new("a").unwrap())).unwrap();
    assert!(refused(&v, "a/b"));
    drop(held);
    assert!(!refused(&v, "a/b"));
    assert!(!refused(&v, "a"));
}

#[test]
fn exclusion_spans_vaults_sharing_a_claimer() {
    // D44: exclusion holds between every holder sharing the claimer, including other
    // `Vault`s. A cloned `MemoryClaimer` shares its lock table.
    let claimer = MemoryClaimer::new();
    let v1 = Vault::new(MemoryStore::new(), claimer.clone());
    let v2 = Vault::new(MemoryStore::new(), claimer);
    let _held = block_on(v1.claim(Key::new("a").unwrap())).unwrap();
    assert!(refused(&v2, "a/b"));
}

#[test]
fn separate_claimers_do_not_exclude_each_other() {
    // The memory claimer only sees its own lock table (documented limitation).
    let v1 = Vault::new(MemoryStore::new(), MemoryClaimer::new());
    let v2 = Vault::new(MemoryStore::new(), MemoryClaimer::new());
    let _held = block_on(v1.claim(Key::new("a").unwrap())).unwrap();
    assert!(!refused(&v2, "a"));
}

#[test]
fn claims_can_move_between_threads() {
    // D37: `Claim` is `Send`; releasing on another thread frees the key.
    let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
    let c = block_on(v.claim(Key::new("a").unwrap())).unwrap();
    std::thread::spawn(move || c.release().unwrap())
        .join()
        .unwrap();
    assert!(!refused(&v, "a"));
}

#[test]
fn concurrent_claims_on_one_key_admit_exactly_one() {
    let v = Vault::new(MemoryStore::new(), MemoryClaimer::new());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let v = v.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                block_on(v.claim(Key::new("shared").unwrap()))
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| matches!(e, Error::AlreadyClaimed { .. }))
    );
}
