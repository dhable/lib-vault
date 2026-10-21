//! Integration tests for `lib-vault-sqlite` against real SQLite databases in temporary
//! directories.
//!
//! One test binary: `app` is the example application every test stores (its record types and
//! its `Dispatcher`). It's app code, the part a user of this crate writes, not a test helper,
//! so the tests share it (D76).

mod app;
mod claims;
mod objects;
