//! Claim exclusion and release: the `Claimer` contract (design §6, D44, D45) and the
//! `ClaimLock` / `Claim::release` rules (§4.4, D11, D39).

mod common;

use common::*;
use lib_vault::{Error, Vault};
use lib_vault_memory::{MemoryClaimer, MemoryStore};

fn refused(v: &Vault, k: &str) -> bool {
    match block_on(v.claim(key(k))) {
        Err(Error::AlreadyClaimed { key: refused }) => {
            assert_eq!(refused, key(k));
            true
        }
        Err(other) => panic!("unexpected error for {k}: {other:?}"),
        Ok(_) => false,
    }
}

#[test]
fn the_same_key_cannot_be_claimed_twice() {
    let v = vault();
    let _held = block_on(claim(&v, "a/b"));
    assert!(refused(&v, "a/b"));
}

#[test]
fn descendants_conflict() {
    // D44: while `a/b` is held, `a/b/c` is refused.
    let v = vault();
    let _held = block_on(claim(&v, "a/b"));
    assert!(refused(&v, "a/b/c"));
    assert!(refused(&v, "a/b/c/d"));
}

#[test]
fn ancestors_conflict() {
    // D44: while `a/b` is held, `a` is refused.
    let v = vault();
    let _held = block_on(claim(&v, "a/b"));
    assert!(refused(&v, "a"));
}

#[test]
fn disjoint_keys_do_not_conflict() {
    // D44: siblings and keys that only share a string prefix are independent.
    let v = vault();
    let _held = block_on(claim(&v, "a/b"));
    assert!(!refused(&v, "a/c"));
    assert!(!refused(&v, "a/bc"));
    assert!(!refused(&v, "b"));
}

#[test]
fn many_disjoint_claims_can_be_held_together() {
    let v = vault();
    let held: Vec<_> = ["x", "y/1", "y/2", "z"]
        .iter()
        .map(|k| block_on(claim(&v, k)))
        .collect();
    assert!(refused(&v, "y"));
    drop(held);
    assert!(!refused(&v, "y"));
}

#[test]
fn release_frees_the_key_immediately() {
    // D39: `Ok` from release means the key can be claimed again right away.
    let v = vault();
    let c = block_on(claim(&v, "a"));
    c.release().unwrap();
    assert!(!refused(&v, "a"));
}

#[test]
fn dropping_a_claim_frees_the_key() {
    // D11: dropping releases the lock if `release` wasn't called.
    let v = vault();
    let c = block_on(claim(&v, "a"));
    drop(c);
    assert!(!refused(&v, "a/b"));
}

#[test]
fn a_refused_claim_holds_nothing() {
    // D47: a failed `try_lock` leaves no lock behind.
    let v = vault();
    let held = block_on(claim(&v, "a"));
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
    let _held = block_on(claim(&v1, "a"));
    assert!(refused(&v2, "a/b"));
}

#[test]
fn separate_claimers_do_not_exclude_each_other() {
    // The memory claimer only sees its own lock table (documented limitation).
    let v1 = vault();
    let v2 = vault();
    let _held = block_on(claim(&v1, "a"));
    assert!(!refused(&v2, "a"));
}

#[test]
fn claims_can_move_between_threads() {
    // D37: `Claim` is `Send`; releasing on another thread frees the key.
    let v = vault();
    let c = block_on(claim(&v, "a"));
    std::thread::spawn(move || c.release().unwrap())
        .join()
        .unwrap();
    assert!(!refused(&v, "a"));
}

#[test]
fn concurrent_claims_on_one_key_admit_exactly_one() {
    let v = vault();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let v = v.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                block_on(v.claim(key("shared")))
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
