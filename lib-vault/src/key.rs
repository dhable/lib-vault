use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

/// A hierarchical name for an object in a `Store`, in unix path syntax: components separated
/// by `/`, e.g. `sessions/123/notes.md`.
///
/// Keys are always relative to some scope (a `Store`, or a `Claim`'s key) and are strictly
/// canonical, so each key has exactly one spelling:
/// - not empty (a `Claim` names its own key with `None` instead);
/// - no leading or trailing `/`, and no empty components (`a//b`);
/// - no `.` or `..` components, so a key can never name something outside its scope;
/// - no NUL bytes.
///
/// Every component also follows a portable name policy, the same for every `Store`:
/// - only ASCII letters, digits, `.`, `_`, ` ` and `-`;
/// - no leading `-` or space, no trailing `.` or space;
/// - not a Windows device name (`CON`, `PRN`, `AUX`, `NUL`, `COM0`-`COM9`, `LPT0`-`LPT9`,
///   with or without an extension, in any case) and not starting with `.vault` (reserved);
/// - at most [`MAX_COMPONENT_LEN`] bytes.
///
/// A key has at most [`MAX_DEPTH`] components and [`MAX_KEY_LEN`] bytes. Stores also refuse to
/// create a key that differs only by ASCII case from an existing one (see `Store::open_write`).
///
/// Cloning is cheap (an `Arc<str>` refcount bump), so implementations can clone a `&Key`
/// into a `'static` future instead of borrowing it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key(Arc<str>);

/// The longest allowed key component, in bytes.
pub const MAX_COMPONENT_LEN: usize = 255;
/// The most components a key may have.
pub const MAX_DEPTH: usize = 32;
/// The longest allowed key, in bytes.
pub const MAX_KEY_LEN: usize = 1024;

/// Why a string was rejected by `Key::new`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyError {
    /// The empty string. There is no root key.
    Empty,
    /// Starts with `/`. Keys are always relative to their scope.
    LeadingSlash,
    /// Ends with `/`.
    TrailingSlash,
    /// Contains `//`.
    EmptyComponent,
    /// Contains a `.` or `..` component.
    DotComponent,
    /// Contains a NUL byte.
    Nul,
    /// Contains a character other than ASCII letters, digits, `.`, `_`, ` ` and `-`.
    InvalidChar,
    /// A component starts with `-` or a space.
    BadComponentStart,
    /// A component ends with `.` or a space.
    BadComponentEnd,
    /// A component is a Windows device name or starts with `.vault`.
    Reserved,
    /// A component is longer than `MAX_COMPONENT_LEN` bytes.
    ComponentTooLong,
    /// More than `MAX_DEPTH` components.
    TooDeep,
    /// Longer than `MAX_KEY_LEN` bytes.
    TooLong,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            KeyError::Empty => "key must not be empty",
            KeyError::LeadingSlash => "key must not start with '/'",
            KeyError::TrailingSlash => "key must not end with '/'",
            KeyError::EmptyComponent => "key must not contain empty components ('//')",
            KeyError::DotComponent => "key must not contain '.' or '..' components",
            KeyError::Nul => "key must not contain NUL bytes",
            KeyError::InvalidChar => {
                "key may only contain ASCII letters, digits, '.', '_', ' ' and '-'"
            }
            KeyError::BadComponentStart => "key components must not start with '-' or a space",
            KeyError::BadComponentEnd => "key components must not end with '.' or a space",
            KeyError::Reserved => "key contains a reserved name",
            KeyError::ComponentTooLong => "key component is too long",
            KeyError::TooDeep => "key has too many components",
            KeyError::TooLong => "key is too long",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for KeyError {}

impl Key {
    /// Parses `s` as a key, rejecting anything that isn't in canonical form.
    pub fn new(s: &str) -> Result<Self, KeyError> {
        Self::validate(s)?;
        Ok(Self(Arc::from(s)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Iterates over the `/`-separated components.
    pub fn components(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.0.split('/').filter(|c| !c.is_empty())
    }

    /// Appends `other` below `self`.
    ///
    /// Unlike `Path::join`, `other` can never replace `self`: keys are always relative, so
    /// the result is always `self` or something beneath it. `ScopedStore` relies on this to
    /// keep callers inside their scope. Fails only if the result would exceed `MAX_DEPTH` or
    /// `MAX_KEY_LEN`.
    pub fn join(&self, other: &Key) -> Result<Key, KeyError> {
        let joined = format!("{}/{}", self.0, other.0);
        Self::check_caps(&joined)?;
        Ok(Key(Arc::from(joined)))
    }

    /// True if `base` is `self` or one of its ancestors. Compares whole components, so
    /// `a/bc` does not start with `a/b`.
    pub fn starts_with(&self, base: &Key) -> bool {
        self.strip_prefix_str(base).is_some()
    }

    /// Returns `self` relative to `base`, or `None` if `base` isn't a proper ancestor.
    /// `a/b/c` stripped of `a` is `b/c`; a key stripped of itself is `None`.
    pub fn strip_prefix(&self, base: &Key) -> Option<Key> {
        self.strip_prefix_str(base)
            .filter(|rest| !rest.is_empty())
            .map(|rest| Key(Arc::from(rest)))
    }

    /// The key one level up, or `None` for a single-component key.
    pub fn parent(&self) -> Option<Key> {
        self.0
            .rsplit_once('/')
            .map(|(parent, _)| Key(Arc::from(parent)))
    }

    /// The last component. Always `Some` now that keys can't be empty.
    pub fn object_name(&self) -> Option<&str> {
        self.components().next_back()
    }

    fn strip_prefix_str(&self, base: &Key) -> Option<&str> {
        match self.0.strip_prefix(&*base.0)? {
            "" => Some(""),
            rest => rest.strip_prefix('/'),
        }
    }

    fn validate(s: &str) -> Result<(), KeyError> {
        if s.is_empty() {
            return Err(KeyError::Empty);
        }
        if s.len() > MAX_KEY_LEN {
            return Err(KeyError::TooLong);
        }
        if s.contains('\0') {
            return Err(KeyError::Nul);
        }
        if s.starts_with('/') {
            return Err(KeyError::LeadingSlash);
        }
        if s.ends_with('/') {
            return Err(KeyError::TrailingSlash);
        }
        for component in s.split('/') {
            Self::validate_component(component)?;
        }
        Self::check_caps(s)
    }

    fn validate_component(c: &str) -> Result<(), KeyError> {
        match c {
            "" => return Err(KeyError::EmptyComponent),
            "." | ".." => return Err(KeyError::DotComponent),
            _ => {}
        }
        if c.len() > MAX_COMPONENT_LEN {
            return Err(KeyError::ComponentTooLong);
        }
        if !c
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b' ' | b'-'))
        {
            return Err(KeyError::InvalidChar);
        }
        if c.starts_with(['-', ' ']) {
            return Err(KeyError::BadComponentStart);
        }
        if c.ends_with(['.', ' ']) {
            return Err(KeyError::BadComponentEnd);
        }
        if Self::is_reserved(c) {
            return Err(KeyError::Reserved);
        }
        Ok(())
    }

    // Windows device names (with any extension) and the `.vault` prefix, ignoring ASCII case.
    fn is_reserved(c: &str) -> bool {
        let lower = c.to_ascii_lowercase();
        if lower.starts_with(".vault") {
            return true;
        }
        let stem = lower.split('.').next().unwrap_or("");
        match stem {
            "con" | "prn" | "aux" | "nul" => true,
            _ => {
                let (prefix, digit) = stem.split_at(stem.len().min(3));
                matches!(prefix, "com" | "lpt")
                    && digit.len() == 1
                    && digit.as_bytes()[0].is_ascii_digit()
            }
        }
    }

    // The whole-key caps, shared with `join`.
    fn check_caps(s: &str) -> Result<(), KeyError> {
        if s.len() > MAX_KEY_LEN {
            return Err(KeyError::TooLong);
        }
        if s.split('/').count() > MAX_DEPTH {
            return Err(KeyError::TooDeep);
        }
        Ok(())
    }
}

impl AsRef<str> for Key {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&Key> for Key {
    fn from(value: &Key) -> Self {
        value.clone()
    }
}

impl FromStr for Key {
    type Err = KeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Key::new(s)
    }
}

impl TryFrom<&str> for Key {
    type Error = KeyError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Key::new(s)
    }
}

impl TryFrom<String> for Key {
    type Error = KeyError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Key::validate(&s)?;
        Ok(Key(Arc::from(s)))
    }
}

/// Anything a `Claim` method accepts as a key relative to the claim: `&str` (validated with
/// `Key::new`), `&Key`, or `Option<&Key>` (`None` means the claimed key itself).
pub trait AsKeyRef {
    fn into_key(self) -> Result<Option<Key>, KeyError>;
}

impl AsKeyRef for &str {
    fn into_key(self) -> Result<Option<Key>, KeyError> {
        Key::new(self).map(Some)
    }
}

impl AsKeyRef for &Key {
    fn into_key(self) -> Result<Option<Key>, KeyError> {
        Ok(Some(self.clone()))
    }
}

impl AsKeyRef for Option<&Key> {
    fn into_key(self) -> Result<Option<Key>, KeyError> {
        Ok(self.cloned())
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    fn key(s: &str) -> Key {
        Key::new(s).unwrap()
    }

    #[test]
    fn accepts_canonical_keys() {
        for s in [
            "a",
            "a/b",
            "sessions/123/notes.md",
            ".hidden",
            "a/..b",
            "a b/c",
        ] {
            assert_eq!(Key::new(s).unwrap().as_str(), s);
        }
    }

    #[test]
    fn rejects_non_canonical_keys() {
        let cases = [
            ("", KeyError::Empty),
            ("/", KeyError::LeadingSlash),
            ("/a", KeyError::LeadingSlash),
            ("a/", KeyError::TrailingSlash),
            ("a//b", KeyError::EmptyComponent),
            (".", KeyError::DotComponent),
            ("..", KeyError::DotComponent),
            ("a/./b", KeyError::DotComponent),
            ("a/../b", KeyError::DotComponent),
            ("a/..", KeyError::DotComponent),
            ("a\0b", KeyError::Nul),
        ];
        for (s, err) in cases {
            assert_eq!(Key::new(s), Err(err), "{s:?}");
            assert_eq!(Key::try_from(s.to_string()), Err(err), "{s:?}");
        }
    }

    #[test]
    fn rejects_names_outside_the_policy() {
        let cases = [
            ("a/b*c", KeyError::InvalidChar),
            ("caf\u{e9}", KeyError::InvalidChar),
            ("a\tb", KeyError::InvalidChar),
            ("-a", KeyError::BadComponentStart),
            ("x/ a", KeyError::BadComponentStart),
            ("a.", KeyError::BadComponentEnd),
            ("x/a ", KeyError::BadComponentEnd),
            ("CON", KeyError::Reserved),
            ("x/con.txt", KeyError::Reserved),
            ("Nul.tar.gz", KeyError::Reserved),
            ("lpt1", KeyError::Reserved),
            ("COM9.log", KeyError::Reserved),
            (".vault", KeyError::Reserved),
            ("x/.VAULTdata", KeyError::Reserved),
        ];
        for (s, err) in cases {
            assert_eq!(Key::new(s), Err(err), "{s:?}");
        }
    }

    #[test]
    fn accepts_names_near_reserved_ones() {
        for s in [
            "console",
            "com",
            "com10",
            "lpt",
            "auxiliary",
            "x.vault",
            "a-b_c.d e",
        ] {
            assert!(Key::new(s).is_ok(), "{s:?}");
        }
    }

    #[test]
    fn enforces_caps() {
        let long = "a".repeat(MAX_COMPONENT_LEN);
        assert!(Key::new(&long).is_ok());
        assert_eq!(
            Key::new(&format!("{long}a")),
            Err(KeyError::ComponentTooLong)
        );

        let deep = vec!["a"; MAX_DEPTH].join("/");
        assert!(Key::new(&deep).is_ok());
        assert_eq!(Key::new(&format!("{deep}/a")), Err(KeyError::TooDeep));

        let max = vec!["a".repeat(204); 5].join("/");
        assert_eq!(max.len(), MAX_KEY_LEN);
        assert!(Key::new(&max).is_ok());

        let huge = vec!["a".repeat(100); 11].join("/");
        assert!(huge.len() > MAX_KEY_LEN);
        assert_eq!(Key::new(&huge), Err(KeyError::TooLong));
    }

    #[test]
    fn join_enforces_caps() {
        let twenty = key(&vec!["a"; 20].join("/"));
        let twelve = key(&["a"; 12].join("/"));
        assert_eq!(
            twenty.join(&twelve).unwrap().components().count(),
            MAX_DEPTH
        );
        assert_eq!(twenty.join(&twenty), Err(KeyError::TooDeep));

        let big = key(&vec!["a".repeat(200); 3].join("/"));
        assert_eq!(big.join(&big), Err(KeyError::TooLong));
    }

    #[test]
    fn join() {
        assert_eq!(key("a").join(&key("b/c")).unwrap(), key("a/b/c"));
        assert_eq!(key("a/b").join(&key("c")).unwrap(), key("a/b/c"));

        let (a, b, c) = (key("a"), key("b/x"), key("c"));
        assert_eq!(
            a.join(&b).unwrap().join(&c).unwrap(),
            a.join(&b.join(&c).unwrap()).unwrap()
        );
    }

    #[test]
    fn join_always_nests() {
        let base = key("scope");
        for other in ["a", "a/b", "scope", "x/scope"] {
            assert!(
                base.join(&key(other)).unwrap().starts_with(&base),
                "{other:?}"
            );
        }
    }

    #[test]
    fn components() {
        let k = key("a/b/c");
        let parts: Vec<_> = k.components().collect();
        assert_eq!(parts, ["a", "b", "c"]);
    }

    #[test]
    fn starts_with_compares_components() {
        assert!(key("a/b/c").starts_with(&key("a/b")));
        assert!(key("a/b").starts_with(&key("a/b")));
        assert!(!key("a/bc").starts_with(&key("a/b")));
        assert!(!key("a").starts_with(&key("a/b")));
    }

    #[test]
    fn strip_prefix() {
        assert_eq!(key("a/b/c").strip_prefix(&key("a")), Some(key("b/c")));
        assert_eq!(key("a/b").strip_prefix(&key("a/b")), None);
        assert_eq!(key("a/bc").strip_prefix(&key("a/b")), None);
    }

    #[test]
    fn parent_and_object_name() {
        assert_eq!(key("a/b/c").parent(), Some(key("a/b")));
        assert_eq!(key("a").parent(), None);
        assert_eq!(key("a/b/c").object_name(), Some("c"));
        assert_eq!(key("a").object_name(), Some("a"));
    }

    #[test]
    fn parse_and_display_round_trip() {
        let k: Key = "a/b".parse().unwrap();
        assert_eq!(k.to_string(), "a/b");
        assert_eq!(Key::try_from("a/b").unwrap(), k);
        assert_eq!(k.as_ref(), "a/b");
    }
}
