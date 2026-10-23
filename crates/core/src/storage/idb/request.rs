//! The IndexedDB effects of [`IdbStorage`](super::IdbStorage) over `web-sys` directly: opening
//! a database at a schema, opening a transaction with a durability hint, and awaiting a request
//! or a commit.
//!
//! Durability law (#912; durable-records, Law 6): a read-write transaction of a store whose
//! completed writes must survive a crash is opened with `{ durability: "strict" }`, and a write
//! reports success only on the transaction's `complete` event ([`commit`]). The default hint
//! does not survive a system crash in any engine: Chromium's default is relaxed since 121, and
//! Gecko and WebKit run SQLite in WAL mode without a flush per commit; `"strict"` is each
//! engine's strongest commit. `web-sys` exposes the options argument of
//! `IDBDatabase.transaction` only under `web_sys_unstable_apis`, a flag every dependent build
//! would have to set, so that one call is bound here ([`transaction`]).
//!
//! Settlement law: every request and every commit is settled by handlers registered before the
//! event can fire (a commit's when its transaction opens, [`CommitWatch`]), resolves exactly
//! once on the first of them, and unregisters them when its watch is dropped, whether its
//! future ran to completion or was dropped mid-await, so a handler never outlives its watch and
//! a later event of the same target finds no stale handler.

use std::cell::RefCell;
use std::rc::Rc;

use futures::channel::oneshot;
use js_sys::Array;
use js_sys::Object;
use js_sys::Reflect;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::DomException;
use web_sys::Event;
use web_sys::IdbDatabase;
use web_sys::IdbFactory;
use web_sys::IdbObjectStore;
use web_sys::IdbObjectStoreParameters;
use web_sys::IdbOpenDbRequest;
use web_sys::IdbRequest;
use web_sys::IdbTransaction;
use web_sys::IdbTransactionMode;

use crate::error::Error;
use crate::error::Result;

#[wasm_bindgen]
extern "C" {
    /// An `IDBDatabase` seen through the one call bound here; every `IdbDatabase` is one.
    #[wasm_bindgen(js_name = IDBDatabase)]
    type OptionedDatabase;

    /// `IDBDatabase.transaction(storeNames, mode, options)`: the call `web-sys` exposes only
    /// under `web_sys_unstable_apis`.
    #[wasm_bindgen(method, catch, js_name = transaction)]
    fn transaction_with_options(
        this: &OptionedDatabase,
        store_names: &Array,
        mode: IdbTransactionMode,
        options: &Object,
    ) -> std::result::Result<IdbTransaction, JsValue>;
}

/// The durability hint a read-write transaction is opened with (the durability law).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Durability {
    /// The engine's default: `complete` may fire before the data is on stable storage.
    Default,
    /// The engine's strongest commit: `complete` fires once the data is flushed.
    Strict,
}

impl Durability {
    /// The hint's value in `IDBTransactionOptions` (pure).
    const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Strict => "strict",
        }
    }
}

/// The schema [`open`] establishes: one row store keyed inline by `key_path` and indexed by
/// `index`, and one store with out-of-line keys.
pub(super) struct Schema {
    /// The database's version.
    pub(super) version: u32,
    /// Name of the row store.
    pub(super) rows: String,
    /// Inline key path of the row store.
    pub(super) key_path: &'static str,
    /// Name, and key path, of the row store's only index.
    pub(super) index: &'static str,
    /// Name of the out-of-line store.
    pub(super) companion: String,
}

/// An [`Error::IDBError`] naming a JavaScript error value: a `DOMException`, an `Error`, or
/// anything else thrown.
pub(super) fn js_error(value: JsValue) -> Error {
    let text = if let Some(exception) = value.dyn_ref::<DomException>() {
        format!("{}: {}", exception.name(), exception.message())
    } else if let Some(error) = value.dyn_ref::<js_sys::Error>() {
        String::from(error.message())
    } else {
        value.as_string().unwrap_or_else(|| format!("{value:?}"))
    };
    Error::IDBError(text)
}

/// The global `indexedDB` factory, of a window and of a worker alike.
fn factory() -> Result<IdbFactory> {
    Reflect::get(&js_sys::global(), &JsValue::from("indexedDB"))
        .map_err(js_error)?
        .dyn_into()
        .map_err(js_error)
}

/// The one outcome a target settles with, taken by whichever of its handlers fires first.
type Outcome = Rc<RefCell<Option<oneshot::Sender<Result<JsValue>>>>>;

/// A handler that settles `outcome` with `settle()` on its first call; later calls do nothing.
fn handler(
    outcome: &Outcome,
    settle: impl Fn() -> Result<JsValue> + 'static,
) -> Closure<dyn FnMut(Event)> {
    let outcome = outcome.clone();
    Closure::new(move |_event: Event| {
        if let Some(sender) = outcome.borrow_mut().take() {
            // A receiver dropped before settlement belongs to a caller that stopped waiting.
            sender.send(settle()).ok();
        }
    })
}

/// A fresh outcome and the receiver it settles.
fn outcome() -> (Outcome, oneshot::Receiver<Result<JsValue>>) {
    let (sender, receiver) = oneshot::channel();
    (Rc::new(RefCell::new(Some(sender))), receiver)
}

/// Await `receiver`; a sender dropped unsettled is an error, never a hang.
async fn settled(receiver: &mut oneshot::Receiver<Result<JsValue>>) -> Result<JsValue> {
    receiver
        .await
        .map_err(|_| Error::IDBError("request dropped before it settled".to_owned()))?
}

/// The error a failed request reports.
fn request_error(request: &IdbRequest) -> Error {
    match request.error() {
        Ok(Some(exception)) => js_error(exception.into()),
        Ok(None) => Error::IDBError("request failed without an error".to_owned()),
        Err(error) => js_error(error),
    }
}

/// The handlers one request is settled by, unregistered when this watch is dropped, whether its
/// future ran to completion or was dropped mid-await (the settlement law).
struct RequestWatch {
    /// The watched request.
    request: IdbRequest,
    /// Its `success` and `error` handlers, kept alive while they are registered.
    _handlers: [Closure<dyn FnMut(Event)>; 2],
    /// The outcome the first handler to fire settles.
    receiver: oneshot::Receiver<Result<JsValue>>,
}

impl RequestWatch {
    /// Register the handlers that settle `request`.
    fn new(request: &IdbRequest) -> Self {
        let (outcome, receiver) = outcome();
        let on_success = handler(&outcome, {
            let request = request.clone();
            move || request.result().map_err(js_error)
        });
        let on_error = handler(&outcome, {
            let request = request.clone();
            move || Err(request_error(&request))
        });
        request.set_onsuccess(Some(on_success.as_ref().unchecked_ref()));
        request.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        Self {
            request: request.clone(),
            _handlers: [on_success, on_error],
            receiver,
        }
    }
}

impl Drop for RequestWatch {
    /// Unregister both handlers before they are dropped.
    fn drop(&mut self) {
        self.request.set_onsuccess(None);
        self.request.set_onerror(None);
    }
}

/// Await `request`: its result on `success`, its error on `error` (the settlement law). A
/// failed request also aborts its transaction, unless a handler prevents that; none does.
pub(super) async fn request(request: &IdbRequest) -> Result<JsValue> {
    let mut watch = RequestWatch::new(request);
    settled(&mut watch.receiver).await
}

/// The commit of one transaction, watched from the moment it opens: `complete`, `error` and
/// `abort` handlers are registered before any request is issued, so no event can precede them
/// whatever the caller awaits between its last request and [`Self::committed`], and they are
/// unregistered when the watch is dropped (the settlement law).
pub(super) struct CommitWatch {
    /// The watched transaction.
    transaction: IdbTransaction,
    /// Its `complete`, `error` and `abort` handlers, kept alive while they are registered.
    _handlers: [Closure<dyn FnMut(Event)>; 3],
    /// The outcome the first handler to fire settles.
    receiver: oneshot::Receiver<Result<JsValue>>,
}

impl CommitWatch {
    /// Register the handlers that settle the commit of `transaction`.
    fn new(transaction: &IdbTransaction) -> Self {
        let (outcome, receiver) = outcome();
        let on_complete = handler(&outcome, || Ok(JsValue::UNDEFINED));
        let failed = {
            let transaction = transaction.clone();
            move || {
                Err(transaction.error().map_or_else(
                    || Error::IDBError("transaction aborted".to_owned()),
                    |exception| js_error(exception.into()),
                ))
            }
        };
        let on_error = handler(&outcome, failed.clone());
        let on_abort = handler(&outcome, failed);
        transaction.set_oncomplete(Some(on_complete.as_ref().unchecked_ref()));
        transaction.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        transaction.set_onabort(Some(on_abort.as_ref().unchecked_ref()));
        Self {
            transaction: transaction.clone(),
            _handlers: [on_complete, on_error, on_abort],
            receiver,
        }
    }

    /// The transaction this watch observes.
    pub(super) fn transaction(&self) -> &IdbTransaction {
        &self.transaction
    }

    /// Await the commit: `Ok` on `complete`, the transaction's error on `error` or `abort`.
    /// Under [`Durability::Strict`], `complete` is the flushed commit.
    pub(super) async fn committed(mut self) -> Result<()> {
        settled(&mut self.receiver).await.map(|_| ())
    }
}

impl Drop for CommitWatch {
    /// Unregister the three handlers before they are dropped.
    fn drop(&mut self) {
        self.transaction.set_oncomplete(None);
        self.transaction.set_onerror(None);
        self.transaction.set_onabort(None);
    }
}

/// Open a transaction over `stores` in `mode`, read-write ones with `durability` (the
/// durability law), and watch its commit from that moment; a read-only transaction takes no
/// hint.
pub(super) fn transaction(
    db: &IdbDatabase,
    stores: &[&str],
    mode: IdbTransactionMode,
    durability: Durability,
) -> Result<CommitWatch> {
    open_transaction(db, stores, mode, durability).map(|transaction| CommitWatch::new(&transaction))
}

/// Open the transaction of [`transaction`].
fn open_transaction(
    db: &IdbDatabase,
    stores: &[&str],
    mode: IdbTransactionMode,
    durability: Durability,
) -> Result<IdbTransaction> {
    let names = stores.iter().copied().map(JsValue::from).collect::<Array>();
    if mode == IdbTransactionMode::Readonly {
        return db
            .transaction_with_str_sequence_and_mode(&names, mode)
            .map_err(js_error);
    }
    let options = Object::new();
    Reflect::set(
        &options,
        &JsValue::from("durability"),
        &JsValue::from(durability.as_str()),
    )
    .map_err(js_error)?;
    db.unchecked_ref::<OptionedDatabase>()
        .transaction_with_options(&names, mode, &options)
        .map_err(js_error)
}

/// Bring the database the upgrade transaction of `open` holds to `schema`: create what is
/// missing, and delete every store and index the schema does not name (a row index of an older
/// schema included).
fn upgrade(open: &IdbOpenDbRequest, schema: &Schema) -> Result<()> {
    let db: IdbDatabase = open.result().map_err(js_error)?.unchecked_into();
    let existing = db.object_store_names();
    let stale = (0..existing.length())
        .filter_map(|position| existing.get(position))
        .filter(|name| *name != schema.rows && *name != schema.companion)
        .collect::<Vec<_>>();
    for name in stale {
        db.delete_object_store(&name).map_err(js_error)?;
    }
    let rows = store_in_upgrade(open, &db, &schema.rows, Some(schema.key_path))?;
    let indexes = rows.index_names();
    let stale = (0..indexes.length())
        .filter_map(|position| indexes.get(position))
        .filter(|name| name != schema.index)
        .collect::<Vec<_>>();
    for name in stale {
        rows.delete_index(&name).map_err(js_error)?;
    }
    if !rows.index_names().contains(schema.index) {
        rows.create_index_with_str(schema.index, schema.index)
            .map_err(js_error)?;
    }
    store_in_upgrade(open, &db, &schema.companion, None)?;
    Ok(())
}

/// The store `name` of the upgrading database, created with `key_path` when it is missing.
fn store_in_upgrade(
    open: &IdbOpenDbRequest,
    db: &IdbDatabase,
    name: &str,
    key_path: Option<&str>,
) -> Result<IdbObjectStore> {
    if db.object_store_names().contains(name) {
        let upgrading = open
            .transaction()
            .ok_or_else(|| Error::IDBError("upgrade without a transaction".to_owned()))?;
        return upgrading.object_store(name).map_err(js_error);
    }
    let parameters = IdbObjectStoreParameters::new();
    if let Some(key_path) = key_path {
        parameters.set_key_path(&JsValue::from(key_path));
    }
    db.create_object_store_with_optional_parameters(name, &parameters)
        .map_err(js_error)
}

/// The `upgradeneeded` and `blocked` handlers of one open, unregistered when dropped (the
/// settlement law).
struct OpenWatch {
    /// The watched open request.
    open: IdbOpenDbRequest,
    /// Its handlers, kept alive while they are registered.
    _handlers: [Closure<dyn FnMut(Event)>; 2],
}

impl Drop for OpenWatch {
    /// Unregister both handlers before they are dropped.
    fn drop(&mut self) {
        self.open.set_onupgradeneeded(None);
        self.open.set_onblocked(None);
    }
}

/// Open the database `name` at `schema`, upgrading it in the version-change transaction when
/// its stored version is lower. An upgrade that fails aborts that transaction, so the open
/// fails and the stored database is unchanged.
///
/// An upgrade waits while another connection holds the database at an older version (another
/// tab of the origin still running an earlier release): the open stays pending until that
/// connection closes, and `blocked` is logged at `warn` so a stuck open is diagnosable.
pub(super) async fn open(name: &str, schema: Schema) -> Result<IdbDatabase> {
    let open = factory()?
        .open_with_u32(name, schema.version)
        .map_err(js_error)?;
    let failure: Rc<RefCell<Option<Error>>> = Rc::new(RefCell::new(None));
    let on_upgrade = {
        let open = open.clone();
        let failure = failure.clone();
        Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
            if let Err(error) = upgrade(&open, &schema) {
                failure.borrow_mut().replace(error);
                if let Some(upgrading) = open.transaction() {
                    upgrading.abort().ok();
                }
            }
        })
    };
    let on_blocked = {
        let name = name.to_owned();
        Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
            tracing::warn!(
                database = %name,
                "IndexedDB upgrade blocked by another connection at an older version; waiting"
            );
        })
    };
    open.set_onupgradeneeded(Some(on_upgrade.as_ref().unchecked_ref()));
    open.set_onblocked(Some(on_blocked.as_ref().unchecked_ref()));
    let watch = OpenWatch {
        open: open.clone(),
        _handlers: [on_upgrade, on_blocked],
    };
    let opened = request(&open).await;
    drop(watch);
    if let Some(error) = failure.borrow_mut().take() {
        return Err(error);
    }
    Ok(opened?.unchecked_into())
}
