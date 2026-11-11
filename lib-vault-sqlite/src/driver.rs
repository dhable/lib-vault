use crate::Dispatcher;
use futures::future::BoxFuture;
use lib_vault::Store;
use lib_vault::io_driver::{IntoObjectPtr, IntoStorePtr, IoDriver};
use std::any::Any;
use std::marker::PhantomData;
use std::sync::Arc;

/// The I/O driver for a SQLite vault (D77, D86): objects travel as the dispatcher's
/// [`Value`](Dispatcher::Value), not as type-erased pointers.
///
/// `claim.store(key, value)` and `claim.load::<T>(key)` accept every `T` that converts into
/// the value type and back (`Into<Value>` and `TryFrom<Value>`), including the value type
/// itself. A stored value that doesn't convert to the requested `T` is `Error::WrongType`.
///
/// Futures are `Send` (`BoxFuture`), and the store is held in an `Arc`, so
/// `Vault<SqliteDriver<D>>` and its claims can move between tokio's worker threads.
pub struct SqliteDriver<D>(PhantomData<D>);

impl<D: Dispatcher> IoDriver for SqliteDriver<D> {
    type Fut<'a, T: 'a> = BoxFuture<'a, T>;
    type ObjectPtr = D::Value;
    type StorePtr = Arc<dyn Store<SqliteDriver<D>> + Send + Sync>;
}

impl<D: Dispatcher, S: Store<SqliteDriver<D>> + Send + Sync> IntoStorePtr<S> for SqliteDriver<D> {
    fn into_store_ptr(store: S) -> Self::StorePtr {
        Arc::new(store)
    }
}

impl<D, T> IntoObjectPtr<T> for SqliteDriver<D>
where
    D: Dispatcher,
    T: Any + Into<D::Value> + TryFrom<D::Value>,
{
    fn into_object_ptr(obj: T) -> Self::ObjectPtr {
        obj.into()
    }

    fn from_object_ptr(ptr: Self::ObjectPtr) -> Option<T> {
        T::try_from(ptr).ok()
    }
}
