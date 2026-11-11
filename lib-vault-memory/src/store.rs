use crate::MemoryError;
use futures::future::{self, BoxFuture};
use lib_vault::io_driver::{AsyncIoDriver, IoDriver};
use lib_vault::{Error, Key, Store};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

type Objects = Arc<Mutex<BTreeMap<Key, Vec<u8>>>>;
type ObjectPtr = <AsyncIoDriver as IoDriver>::ObjectPtr;

/// A [`Store`] that keeps every object in memory, as bytes.
///
/// Clones share the same objects, so several `Vault`s can use one store.
///
/// Behavior, per the `Store` contract:
/// - **Objects are `Vec<u8>`**: `claim.store(key, bytes)` and `claim.load::<Vec<u8>>(key)`.
///   Any other type fails with `Error::WrongType`.
/// - **`load`** returns a copy of the object's bytes.
/// - **`store`** replaces the object atomically: it either stores all of the bytes or fails
///   and leaves the key as it was.
/// - **Case collisions**: `store` fails with [`MemoryError::CaseCollision`] if the key differs
///   only by ASCII case from an existing object's key.
/// - **Keys**: an object at `a` and objects beneath it (`a/b`) may coexist; `list` returns the
///   keys strictly beneath the listed key.
#[derive(Clone, Default)]
pub struct MemoryStore {
    objects: Objects,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

// The key of an existing object that differs from `key` only by ASCII case, if any.
fn case_collision(objects: &BTreeMap<Key, Vec<u8>>, key: &Key) -> Option<Key> {
    objects
        .keys()
        .find(|k| *k != key && k.as_str().eq_ignore_ascii_case(key.as_str()))
        .cloned()
}

fn collision_error(key: &Key, existing: Key) -> MemoryError {
    MemoryError::CaseCollision {
        key: key.clone(),
        existing,
    }
}

impl Store<AsyncIoDriver> for MemoryStore {
    fn list(&self, key: &Key) -> BoxFuture<'_, Result<Vec<Key>, Error>> {
        let objects = self.objects.lock().expect("lock poisoned");
        let keys = objects.keys().filter_map(|k| k.strip_prefix(key)).collect();
        Box::pin(future::ready(Ok(keys)))
    }

    fn delete<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<(), Error>> {
        self.objects.lock().expect("lock poisoned").remove(key);
        Box::pin(future::ready(Ok(())))
    }

    fn load<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectPtr, Error>> {
        let objects = self.objects.lock().expect("lock poisoned");
        let result = match objects.get(key) {
            // A new `Arc` per load: the caller takes the bytes out of it.
            Some(data) => Ok(Arc::new(data.clone()) as ObjectPtr),
            None => Err(Error::NotFound { key: key.clone() }),
        };
        Box::pin(future::ready(result))
    }

    fn store<'a>(&'a self, key: &Key, data: ObjectPtr) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(future::ready(self.insert(key, data)))
    }
}

impl MemoryStore {
    fn insert(&self, key: &Key, data: ObjectPtr) -> Result<(), Error> {
        let Ok(data) = data.downcast::<Vec<u8>>() else {
            return Err(Error::WrongType { key: key.clone() });
        };
        let mut objects = self.objects.lock().expect("lock poisoned");
        if let Some(existing) = case_collision(&objects, key) {
            return Err(collision_error(key, existing).into());
        }
        objects.insert(key.clone(), Arc::unwrap_or_clone(data));
        Ok(())
    }
}
