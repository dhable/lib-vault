use crate::MemoryError;
use futures::future::{self, BoxFuture};
use futures::io::{AsyncWrite, Cursor};
use lib_vault::io_driver::async_io::{ObjectReader, ObjectWriter};
use lib_vault::{Error, Key, Store, io_driver};
use std::collections::BTreeMap;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

type Objects = Arc<Mutex<BTreeMap<Key, Arc<[u8]>>>>;

/// A [`Store`] that keeps every object in memory.
///
/// Clones share the same objects, so several `Vault`s can use one store.
///
/// Behavior, per the `Store` contract:
/// - **Reads** return a snapshot of the object's bytes when it was opened.
/// - **Writes** are buffered and **committed on `close()`**, replacing the object atomically.
///   A writer dropped without `close()` discards its data.
/// - **Case collisions**: `open_write` (and the commit in `close()`) fail with
///   [`MemoryError::CaseCollision`] if the key differs only by ASCII case from an existing
///   object's key.
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
fn case_collision(objects: &BTreeMap<Key, Arc<[u8]>>, key: &Key) -> Option<Key> {
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

impl Store<io_driver::AsyncIoDriver> for MemoryStore {
    fn list(&self, key: &Key) -> BoxFuture<'_, Result<Vec<Key>, Error>> {
        let objects = self.objects.lock().expect("lock poisoned");
        let keys = objects.keys().filter_map(|k| k.strip_prefix(key)).collect();
        Box::pin(future::ready(Ok(keys)))
    }

    fn delete<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<(), Error>> {
        self.objects.lock().expect("lock poisoned").remove(key);
        Box::pin(future::ready(Ok(())))
    }

    fn open_read<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectReader<'a>, Error>> {
        let data = self
            .objects
            .lock()
            .expect("lock poisoned")
            .get(key)
            .cloned();
        let result = match data {
            Some(data) => Ok(Box::pin(Cursor::new(data)) as ObjectReader<'a>),
            None => Err(Error::NotFound { key: key.clone() }),
        };
        Box::pin(future::ready(result))
    }

    fn open_write<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectWriter<'a>, Error>> {
        let objects = self.objects.lock().expect("lock poisoned");
        let result = match case_collision(&objects, key) {
            Some(existing) => Err(collision_error(key, existing).into()),
            None => Ok(Box::pin(MemoryWriter {
                key: key.clone(),
                buf: Vec::new(),
                objects: self.objects.clone(),
                closed: false,
            }) as ObjectWriter<'a>),
        };
        Box::pin(future::ready(result))
    }
}

// Buffers writes and commits them to the store on `close()`.
struct MemoryWriter {
    key: Key,
    buf: Vec<u8>,
    objects: Objects,
    closed: bool,
}

impl MemoryWriter {
    fn commit(&mut self) -> Result<(), MemoryError> {
        let mut objects = self.objects.lock().expect("lock poisoned");
        // Re-check: another writer may have created a colliding key since `open_write`.
        if let Some(existing) = case_collision(&objects, &self.key) {
            return Err(collision_error(&self.key, existing));
        }
        let data: Arc<[u8]> = std::mem::take(&mut self.buf).into();
        objects.insert(self.key.clone(), data);
        Ok(())
    }
}

impl AsyncWrite for MemoryWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.closed {
            return Poll::Ready(Err(MemoryError::WriterClosed.into()));
        }
        this.buf.extend_from_slice(data);
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.closed {
            this.closed = true;
            this.commit()?;
        }
        Poll::Ready(Ok(()))
    }
}
