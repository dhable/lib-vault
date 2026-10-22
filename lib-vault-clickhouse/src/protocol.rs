//! The pure parts of the claim protocol: which rows a claim touches, how tokens name their
//! owner, and how Keeper conflicts show up in ClickHouse errors. No server needed.

use lib_vault::Key;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Separates the owner from the per-claim sequence number in a token (`<owner>#<seq>`). Keys
/// can't contain it (D6), and owner ids never do.
pub(crate) const TOKEN_SEPARATOR: char = '#';

/// The rows a claim on `key` locks, D67-style: a shared hold on each proper ancestor
/// (top-down) and an exclusive hold on the key itself. `a/b/c` → (`[a, a/b]`, `a/b/c`).
pub(crate) fn lock_plan(key: &Key) -> (Vec<String>, String) {
    let mut ancestors = Vec::new();
    let mut path = String::new();
    for component in key.components() {
        if !path.is_empty() {
            ancestors.push(path.clone());
            path.push('/');
        }
        path.push_str(component);
    }
    (ancestors, path)
}

/// The owner a token belongs to.
pub(crate) fn token_owner(token: &str) -> &str {
    token
        .split_once(TOKEN_SEPARATOR)
        .map_or(token, |(owner, _)| owner)
}

/// A unique id for a claimer, across processes and hosts: pid, start time and a per-process
/// counter.
pub(crate) fn new_owner_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{n}", std::process::id())
}

/// How a Keeper write that ClickHouse refused can be told apart from other failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeeperConflict {
    /// A strict-mode `INSERT` found an existing key.
    NodeExists,
    /// A strict-mode `ALTER UPDATE` or `DELETE` lost to a concurrent change of a row it read
    /// (`Bad version`), or to its deletion (`No node`).
    Lost,
}

/// Recognizes the Keeper errors that strict-mode `KeeperMap` writes raise (`Code: 999 ...
/// Transaction failed (Node exists)`, `(Bad version)` or `(No node)`). ClickHouse reports
/// them only as text.
pub(crate) fn keeper_conflict(err: &clickhouse::error::Error) -> Option<KeeperConflict> {
    let clickhouse::error::Error::BadResponse(message) = err else {
        return None;
    };
    if message.contains("(Node exists)") {
        Some(KeeperConflict::NodeExists)
    } else if message.contains("(Bad version)") || message.contains("(No node)") {
        Some(KeeperConflict::Lost)
    } else {
        None
    }
}

/// Checks a `KeeperMap` root path: absolute, `/`-separated components of
/// `[A-Za-z0-9_-]`, no trailing `/`.
pub(crate) fn validate_root_path(path: &str) -> Result<(), String> {
    let Some(rest) = path.strip_prefix('/') else {
        return Err(format!("root path {path:?} must start with '/'"));
    };
    let valid = !rest.is_empty()
        && rest.split('/').all(|component| {
            !component.is_empty()
                && component
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        });
    if valid {
        Ok(())
    } else {
        Err(format!(
            "root path {path:?} must be '/'-separated components of [A-Za-z0-9_-]"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> Key {
        Key::new(s).unwrap()
    }

    #[test]
    fn a_single_component_key_locks_only_itself() {
        assert_eq!(lock_plan(&key("a")), (vec![], "a".to_owned()));
    }

    #[test]
    fn ancestors_are_locked_top_down() {
        let (ancestors, target) = lock_plan(&key("a/b/c"));
        assert_eq!(ancestors, ["a", "a/b"]);
        assert_eq!(target, "a/b/c");
    }

    #[test]
    fn sibling_and_prefix_keys_share_only_whole_component_ancestors() {
        // `a/b` and `a/bc` both hold `a` shared, but neither touches the other's row.
        let (a, b) = lock_plan(&key("a/b"));
        let (c, d) = lock_plan(&key("a/bc"));
        assert_eq!(a, c);
        assert_ne!(b, d);
    }

    #[test]
    fn tokens_name_their_owner() {
        assert_eq!(token_owner("123-456-7#42"), "123-456-7");
        assert_eq!(token_owner("no-separator"), "no-separator");
        assert_eq!(token_owner(""), "");
    }

    #[test]
    fn owner_ids_are_unique_and_never_contain_the_separator() {
        let a = new_owner_id();
        let b = new_owner_id();
        assert_ne!(a, b);
        assert!(!a.contains(TOKEN_SEPARATOR));
    }

    #[test]
    fn keeper_conflicts_are_recognized_from_server_errors() {
        // Messages as ClickHouse 26.9 reports them.
        let exists = clickhouse::error::Error::BadResponse(
            "Code: 999. Coordination::Exception: Transaction failed (Node exists): Op #0, path: \
             /keeper_map_tables/claims/data/AWE=. (KEEPER_EXCEPTION)"
                .into(),
        );
        let version = clickhouse::error::Error::BadResponse(
            "Code: 999. Coordination::Exception: Transaction failed (Bad version)".into(),
        );
        let gone = clickhouse::error::Error::BadResponse(
            "Code: 999. Coordination::Exception: Transaction failed (No node): Op #0, path: \
             /keeper_map_tables/claims/data/BnBhcmVudA==. (KEEPER_EXCEPTION)"
                .into(),
        );
        let other =
            clickhouse::error::Error::BadResponse("Code: 60. DB::Exception: Unknown table".into());
        assert_eq!(keeper_conflict(&exists), Some(KeeperConflict::NodeExists));
        assert_eq!(keeper_conflict(&version), Some(KeeperConflict::Lost));
        assert_eq!(keeper_conflict(&gone), Some(KeeperConflict::Lost));
        assert_eq!(keeper_conflict(&other), None);
        assert_eq!(keeper_conflict(&clickhouse::error::Error::TimedOut), None);
    }

    #[test]
    fn root_paths_are_validated() {
        for ok in ["/lib_vault", "/a/b-c/d_e", "/x1"] {
            assert!(validate_root_path(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "/",
            "relative",
            "/trailing/",
            "/a//b",
            "/quote'",
            "/sp ace",
        ] {
            assert!(validate_root_path(bad).is_err(), "{bad:?}");
        }
    }
}
