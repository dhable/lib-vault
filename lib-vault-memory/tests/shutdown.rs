//! Vault shutdown against the memory backend (design §4.5, D13, D41, D46, D47).

mod common;

use common::*;
use lib_vault::{Error, Vault};
use lib_vault_memory::{MemoryClaimer, MemoryStore};
use std::thread;
use std::time::Duration;

fn spawn_shutdown(v: &Vault) -> thread::JoinHandle<()> {
    let v = v.clone();
    thread::spawn(move || block_on(v.shutdown()))
}

fn still_running(h: &thread::JoinHandle<()>) -> bool {
    thread::sleep(Duration::from_millis(50));
    !h.is_finished()
}

#[test]
fn shutdown_with_no_claims_returns() {
    block_on(vault().shutdown());
}

#[test]
fn claims_are_refused_after_shutdown() {
    // D13: no new claims once shutdown has started.
    let v = vault();
    block_on(v.clone().shutdown());
    assert!(matches!(
        block_on(v.claim(key("a"))),
        Err(Error::ShuttingDown)
    ));
}

#[test]
fn shutdown_waits_for_a_dropped_claim() {
    let v = vault();
    let c = block_on(claim(&v, "a"));
    let h = spawn_shutdown(&v);
    assert!(still_running(&h));
    drop(c);
    h.join().unwrap();
}

#[test]
fn shutdown_waits_for_a_released_claim() {
    // D11: `release(self)` consumes the claim, so the count drops when it returns.
    let v = vault();
    let c = block_on(claim(&v, "a"));
    let h = spawn_shutdown(&v);
    assert!(still_running(&h));
    c.release().unwrap();
    h.join().unwrap();
}

#[test]
fn shutdown_waits_for_every_claim() {
    let v = vault();
    let a = block_on(claim(&v, "a"));
    let b = block_on(claim(&v, "b"));
    let h = spawn_shutdown(&v);
    drop(a);
    assert!(still_running(&h));
    drop(b);
    h.join().unwrap();
}

#[test]
fn shutdown_wakes_when_a_claim_is_dropped_on_another_thread() {
    let v = vault();
    let c = block_on(claim(&v, "a"));
    let h = spawn_shutdown(&v);
    assert!(still_running(&h));
    thread::spawn(move || drop(c)).join().unwrap();
    h.join().unwrap();
}

#[test]
fn a_refused_claim_does_not_block_shutdown() {
    // D47: a failed `try_lock` gives its outstanding count back.
    let v = vault();
    let held = block_on(claim(&v, "a"));
    assert!(block_on(v.claim(key("a"))).is_err());
    drop(held);
    block_on(v.shutdown());
}

#[test]
fn several_shutdown_callers_all_finish() {
    // D46: several clones may await shutdown at once.
    let v = vault();
    let c = block_on(claim(&v, "a"));
    let h1 = spawn_shutdown(&v);
    let h2 = spawn_shutdown(&v);
    assert!(still_running(&h1));
    assert!(still_running(&h2));
    drop(c);
    h1.join().unwrap();
    h2.join().unwrap();
}

#[test]
fn shutdown_is_per_vault_not_per_claimer() {
    // §4.5: shutdown only affects the vault it was called on (and its clones).
    let claimer = MemoryClaimer::new();
    let store = MemoryStore::new();
    let v1 = Vault::new(store.clone(), claimer.clone());
    let v2 = Vault::new(store, claimer);
    block_on(v1.shutdown());
    assert!(block_on(v2.claim(key("a"))).is_ok());
}
