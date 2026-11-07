#![deny(missing_docs)]

//! Exclusive browser storage ownership via the Web Locks API (#1055) and its release witness
//! (#1085).
//!
//! **Ownership law.** At most one open browser node in an origin holds a `storage_name` at a
//! time. Before opening any IndexedDB database for `storage_name`, the browser provider requests
//! the exclusive Web Lock `rings-node/<storage_name>` with `ifAvailable: true`:
//!
//! ```text
//!   navigator.locks.request(
//!       "rings-node/<storage_name>",
//!       { mode: "exclusive", ifAvailable: true },
//!       callback,
//!   )
//! ```
//!
//! - When another tab or worker of the origin (or another open node in the same context) holds
//!   the lock, the browser invokes `callback(null)` immediately; the open fails closed with
//!   [`Error::BrowserStorageLocked`] before any IndexedDB database is opened.
//! - When the lock is granted, `callback(lock)` returns a pending `Promise` whose `resolve`
//!   function is held by [`BrowserStorageLock`], shared across every [`LockedBrowserStorage`] of
//!   the node.
//!
//! **Release law.** Each [`LockedBrowserStorage`] declares its [`IdbStorage`] before its shared
//! [`BrowserStorageLock`], so dropping the last store of the node drops every `IdbStorage` first
//! and then resolves the Web Lock's hold `Promise`. Once the browser's `LockManager` releases the
//! lock and settles the `request` `Promise`, [`BrowserLockRelease`] resolves:
//!
//! ```text
//!   last store drops ─▶ IdbStorage drops ─▶ BrowserStorageLock drops ─▶ hold Promise resolves
//!     ─▶ LockManager releases Web Lock ─▶ request Promise settles ─▶ BrowserLockRelease resolves
//! ```
//!
//! [`Provider::close`](crate::provider::Provider::close) awaits [`BrowserLockRelease`] through
//! [`StoresReleased`](crate::release::StoresReleased), so once `close` returns `Ok`, a subsequent
//! open of the same `storage_name` succeeds at once.

use std::cell::RefCell;
use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::Context;
use std::task::Poll;

use async_trait::async_trait;
use futures::channel::oneshot;
use futures::future::Shared;
use futures::FutureExt;
use js_sys::Function;
use js_sys::Object;
use js_sys::Promise;
use js_sys::Reflect;
use rings_core::error::Result as CoreResult;
use rings_core::storage::idb::IdbStorage;
use rings_core::storage::KvStorageInterface;
use rings_core::storage::KvStorageScan;
use rings_core::storage::RecordAuthority;
use rings_core::storage::ScannedRecord;
use serde::de::DeserializeOwned;
use serde::Serialize;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;

use crate::error::Error;
use crate::error::Result as NodeResult;

/// Prefix of every browser storage Web Lock name (`rings-node/<storage_name>`).
pub(crate) const BROWSER_STORAGE_LOCK_PREFIX: &str = "rings-node";

#[wasm_bindgen]
extern "C" {
    /// A `LockManager` (`navigator.locks`) seen through the `request` overload with options.
    #[wasm_bindgen(js_name = LockManager)]
    type BrowserLockManager;

    /// `LockManager.request(name, options, callback)`.
    #[wasm_bindgen(method, catch, js_name = request)]
    fn request_with_options(
        this: &BrowserLockManager,
        name: &str,
        options: &Object,
        callback: &Function,
    ) -> std::result::Result<Promise, JsValue>;
}

/// The Web Lock name of `storage_name` (pure): `rings-node/<storage_name>`.
pub(crate) fn lock_name(storage_name: &str) -> String {
    format!("{BROWSER_STORAGE_LOCK_PREFIX}/{storage_name}")
}

/// Format a JavaScript error value as [`Error::JsError`].
fn js_error(value: JsValue) -> Error {
    let text = if let Some(error) = value.dyn_ref::<js_sys::Error>() {
        String::from(error.message())
    } else {
        value.as_string().unwrap_or_else(|| format!("{value:?}"))
    };
    Error::JsError(text)
}

/// The global `navigator.locks` manager, in a window or a worker alike.
fn lock_manager() -> NodeResult<BrowserLockManager> {
    let navigator =
        Reflect::get(&js_sys::global(), &JsValue::from_str("navigator")).map_err(js_error)?;
    if navigator.is_undefined() || navigator.is_null() {
        return Err(Error::JsError("navigator is unavailable".to_owned()));
    }
    let locks = Reflect::get(&navigator, &JsValue::from_str("locks")).map_err(js_error)?;
    if locks.is_undefined() || locks.is_null() {
        return Err(Error::JsError("navigator.locks is unavailable".to_owned()));
    }
    Ok(locks.unchecked_into())
}

/// The sending side of a browser lock's release: dropped once the `navigator.locks.request`
/// promise settles after the lock is released (the release law).
struct ReleaseSignal {
    /// Never sent on: its drop is the release event.
    _dropped: oneshot::Sender<Infallible>,
}

/// Resolves once its browser storage lock is released by the origin's `LockManager` (the release
/// law, #1055, #1085).
///
/// Cloning shares the witness: every clone resolves at the release.
#[derive(Clone)]
pub struct BrowserLockRelease(Shared<oneshot::Receiver<Infallible>>);

/// A browser lock's release signal and the witness of its drop.
fn release_signal() -> (ReleaseSignal, BrowserLockRelease) {
    let (dropped, released) = oneshot::channel();
    (
        ReleaseSignal { _dropped: dropped },
        BrowserLockRelease(released.shared()),
    )
}

impl Future for BrowserLockRelease {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // The sender is never sent on: the only outcome is `Canceled`, the release.
        Pin::new(&mut self.0).poll(cx).map(|_| ())
    }
}

impl fmt::Debug for BrowserLockRelease {
    /// The witness is opaque: whether it resolved is learned only by awaiting it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BrowserLockRelease")
    }
}

/// The exclusive Web Lock held for one `storage_name` while its node's stores are open.
///
/// Dropping it resolves the `Promise` returned to `navigator.locks.request`, releasing the Web
/// Lock in the browser's `LockManager`.
pub(crate) struct BrowserStorageLock {
    /// The `resolve` function of the pending hold `Promise`.
    release: Function,
}

impl Drop for BrowserStorageLock {
    /// Resolve the hold `Promise` so the browser's `LockManager` releases the Web Lock.
    fn drop(&mut self) {
        if let Err(error) = self.release.call0(&JsValue::UNDEFINED) {
            tracing::warn!(?error, "failed to resolve browser storage Web Lock promise");
        }
    }
}

/// Acquire the exclusive non-blocking Web Lock `rings-node/<storage_name>` (the ownership law).
///
/// Post: `Ok((lock, released))` holds the Web Lock until `lock` is dropped, and `released`
/// resolves once the browser's `LockManager` has released it; `Err(BrowserStorageLocked)` iff
/// another holder in the origin already holds `rings-node/<storage_name>`.
pub(crate) async fn lock_browser_storage(
    storage_name: &str,
) -> NodeResult<(BrowserStorageLock, BrowserLockRelease)> {
    let locks = lock_manager()?;
    let options = Object::new();
    Reflect::set(
        &options,
        &JsValue::from_str("mode"),
        &JsValue::from_str("exclusive"),
    )
    .map_err(js_error)?;
    Reflect::set(&options, &JsValue::from_str("ifAvailable"), &JsValue::TRUE).map_err(js_error)?;

    let (outcome_tx, outcome_rx) = oneshot::channel::<NodeResult<Option<BrowserStorageLock>>>();
    let outcome = Rc::new(RefCell::new(Some(outcome_tx)));
    let callback = {
        let outcome = Rc::clone(&outcome);
        Closure::<dyn FnMut(JsValue) -> Promise>::new(move |lock: JsValue| {
            if lock.is_null() || lock.is_undefined() {
                if let Some(sender) = outcome.borrow_mut().take() {
                    let _ = sender.send(Ok(None));
                }
                return Promise::resolve(&JsValue::UNDEFINED);
            }
            let mut release_fn = None;
            let hold_promise = Promise::new(&mut |resolve, _reject| {
                release_fn = Some(resolve);
            });
            let lock_guard = release_fn
                .map(|release| BrowserStorageLock { release })
                .ok_or_else(|| {
                    Error::JsError(
                        "Web Lock hold promise executor did not supply resolve".to_owned(),
                    )
                });
            if let Some(sender) = outcome.borrow_mut().take() {
                let _ = sender.send(lock_guard.map(Some));
            }
            hold_promise
        })
    };

    let name = lock_name(storage_name);
    let callback_fn: &Function = callback.as_ref().unchecked_ref();
    let request_promise = locks
        .request_with_options(&name, &options, callback_fn)
        .map_err(js_error)?;

    let (signal, released) = release_signal();
    let on_settle = {
        let outcome = Rc::clone(&outcome);
        Closure::once_into_js(move |value: JsValue| {
            drop(callback);
            if let Some(sender) = outcome.borrow_mut().take() {
                let _ = sender.send(Err(js_error(value)));
            }
            drop(signal);
        })
    };
    let on_settle_fn: &Function = on_settle.unchecked_ref();
    let then_fn = Reflect::get(&request_promise, &JsValue::from_str("then"))
        .map_err(js_error)?
        .dyn_into::<Function>()
        .map_err(js_error)?;
    let _ = then_fn
        .call2(&request_promise, on_settle_fn, on_settle_fn)
        .map_err(js_error)?;

    let granted = outcome_rx.await.map_err(|_| {
        Error::JsError("Web Lock request settled before reporting lock outcome".to_owned())
    })??;
    match granted {
        Some(lock) => Ok((lock, released)),
        None => Err(Error::BrowserStorageLocked(storage_name.to_owned())),
    }
}

/// An [`IdbStorage`] paired with a share of its node's [`BrowserStorageLock`].
///
/// Field declaration order ensures `inner` drops before `_lock` (the release law).
pub(crate) struct LockedBrowserStorage {
    /// The underlying IndexedDB store.
    inner: IdbStorage,
    /// Shared ownership of the `rings-node/<storage_name>` Web Lock.
    _lock: Rc<BrowserStorageLock>,
}

impl LockedBrowserStorage {
    /// Bind `inner` to the shared `lock`.
    pub(crate) fn new(inner: IdbStorage, lock: Rc<BrowserStorageLock>) -> Self {
        Self { inner, _lock: lock }
    }

    /// Open an [`IdbStorage`] of `(cap, name, authority)` and bind it to `lock`.
    pub(crate) async fn open(
        cap: u32,
        name: &str,
        authority: RecordAuthority,
        lock: &Rc<BrowserStorageLock>,
    ) -> NodeResult<Self> {
        IdbStorage::new_with_cap_name_and_authority(cap, name, authority)
            .await
            .map(|storage| Self::new(storage, Rc::clone(lock)))
            .map_err(|source| Error::BrowserStorageOpen {
                name: name.to_owned(),
                source,
            })
    }

    /// Open an [`IdbStorage`] of `(cap, name, authority)` bound to `lock`, logging `fallback_warning`
    /// and returning `None` if IndexedDB cannot open.
    pub(crate) async fn open_or_warn(
        cap: u32,
        name: &str,
        authority: RecordAuthority,
        lock: &Rc<BrowserStorageLock>,
        fallback_warning: &'static str,
    ) -> Option<Self> {
        match Self::open(cap, name, authority, lock).await {
            Ok(storage) => Some(storage),
            Err(error) => {
                tracing::warn!(storage_name = %name, %error, "{fallback_warning}");
                None
            }
        }
    }
}

impl fmt::Debug for LockedBrowserStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LockedBrowserStorage")
            .field("inner", &self.inner)
            .finish()
    }
}

#[async_trait(?Send)]
impl<V> KvStorageInterface<V> for LockedBrowserStorage
where V: DeserializeOwned + Serialize + Sized
{
    async fn get(&self, key: &str) -> CoreResult<Option<V>> {
        self.inner.get(key).await
    }

    async fn put(&self, key: &str, value: &V) -> CoreResult<()> {
        self.inner.put(key, value).await
    }

    async fn get_all(&self) -> CoreResult<Vec<(String, V)>> {
        self.inner.get_all().await
    }

    async fn remove(&self, key: &str) -> CoreResult<()> {
        <IdbStorage as KvStorageInterface<V>>::remove(&self.inner, key).await
    }

    async fn clear(&self) -> CoreResult<()> {
        <IdbStorage as KvStorageInterface<V>>::clear(&self.inner).await
    }

    async fn count(&self) -> CoreResult<u32> {
        <IdbStorage as KvStorageInterface<V>>::count(&self.inner).await
    }
}

#[async_trait(?Send)]
impl<V> KvStorageScan<V> for LockedBrowserStorage
where V: DeserializeOwned + Serialize + Sized
{
    async fn scan(&self) -> CoreResult<Vec<ScannedRecord<V>>> {
        self.inner.scan().await
    }

    fn record_name(&self, key: &str) -> String {
        <IdbStorage as KvStorageScan<V>>::record_name(&self.inner, key)
    }

    async fn put_batch(&self, records: &[(String, V)]) -> CoreResult<()> {
        self.inner.put_batch(records).await
    }

    async fn remove_batch(&self, keys: &[String]) -> CoreResult<()> {
        <IdbStorage as KvStorageScan<V>>::remove_batch(&self.inner, keys).await
    }
}
