//! Utils for ring-core
use std::collections::VecDeque;
use std::future::poll_fn;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use chrono::Utc;

/// Atomically add `amount` when the resulting reservation stays within `limit`.
pub(crate) fn try_reserve_atomic(counter: &AtomicUsize, amount: usize, limit: usize) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(amount) else {
            return false;
        };
        if next > limit {
            return false;
        }
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

/// Return whether one class may reserve `amount` while preserving every other
/// class's unmet minimum reservation.
pub(crate) fn fair_reservation_fits<const N: usize>(
    admitted_by_class: &[usize; N],
    admitted: usize,
    class_index: usize,
    amount: usize,
    capacity: usize,
    reservations: &[usize; N],
) -> bool {
    if class_index >= N {
        return false;
    }
    let reserved_for_others = admitted_by_class
        .iter()
        .zip(reservations)
        .enumerate()
        .filter(|(index, _)| *index != class_index)
        .map(|(_, (admitted, reserved))| reserved.saturating_sub(*admitted))
        .sum::<usize>();
    admitted
        .checked_add(amount)
        .is_some_and(|next| next <= capacity.saturating_sub(reserved_for_others))
}

/// Return whether one request still fits entirely inside its class's fixed
/// reservation, without borrowing shared capacity.
pub(crate) fn fixed_reservation_covers<const N: usize>(
    admitted_by_class: &[usize; N],
    class_index: usize,
    amount: usize,
    reservations: &[usize; N],
) -> bool {
    admitted_by_class
        .get(class_index)
        .zip(reservations.get(class_index))
        .and_then(|(admitted, reserved)| admitted.checked_add(amount).map(|next| (next, reserved)))
        .is_some_and(|(next, reserved)| next <= *reserved)
}

/// Capacity shared by a fixed number of traffic classes with minimum reservations.
#[derive(Clone, Copy)]
pub(crate) struct ReservedCapacity<const N: usize> {
    admitted: usize,
    admitted_by_class: [usize; N],
}

impl<const N: usize> ReservedCapacity<N> {
    pub(crate) const fn new() -> Self {
        Self {
            admitted: 0,
            admitted_by_class: [0; N],
        }
    }

    pub(crate) fn reservation_covers(
        &self,
        class_index: usize,
        amount: usize,
        reservations: &[usize; N],
    ) -> bool {
        fixed_reservation_covers(&self.admitted_by_class, class_index, amount, reservations)
    }

    pub(crate) fn try_reserve(
        &mut self,
        class_index: usize,
        amount: usize,
        capacity: usize,
        reservations: &[usize; N],
    ) -> bool {
        if !fair_reservation_fits(
            &self.admitted_by_class,
            self.admitted,
            class_index,
            amount,
            capacity,
            reservations,
        ) {
            return false;
        }
        self.admitted = self.admitted.saturating_add(amount);
        if let Some(class_admitted) = self.admitted_by_class.get_mut(class_index) {
            *class_admitted = class_admitted.saturating_add(amount);
        }
        true
    }

    pub(crate) fn release(&mut self, class_index: usize, amount: usize) {
        self.admitted = self.admitted.saturating_sub(amount);
        if let Some(class_admitted) = self.admitted_by_class.get_mut(class_index) {
            *class_admitted = class_admitted.saturating_sub(amount);
        }
    }

    pub(crate) const fn admitted(self) -> usize {
        self.admitted
    }
}

#[derive(Default)]
struct FairWaitBudgetState {
    waiters: usize,
    cost: usize,
}

/// Shared hard bound for payloads retained while fair admission is pending.
pub(crate) struct FairWaitBudget {
    state: Mutex<FairWaitBudgetState>,
    max_waiters: usize,
    max_cost: usize,
}

impl FairWaitBudget {
    pub(crate) fn new(max_waiters: usize, max_cost: usize) -> Self {
        Self {
            state: Mutex::new(FairWaitBudgetState::default()),
            max_waiters,
            max_cost,
        }
    }

    fn try_acquire(self: &Arc<Self>, cost: usize) -> Option<FairWaitBudgetPermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next_waiters = state.waiters.checked_add(1)?;
        let next_cost = state.cost.checked_add(cost)?;
        if next_waiters > self.max_waiters || next_cost > self.max_cost {
            return None;
        }
        state.waiters = next_waiters;
        state.cost = next_cost;
        Some(FairWaitBudgetPermit {
            budget: self.clone(),
            cost,
        })
    }
}

struct FairWaitBudgetPermit {
    budget: Arc<FairWaitBudget>,
    cost: usize,
}

impl Drop for FairWaitBudgetPermit {
    fn drop(&mut self) {
        let mut state = self
            .budget
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.waiters = state.waiters.saturating_sub(1);
        state.cost = state.cost.saturating_sub(self.cost);
    }
}

struct FairWaiterEntry {
    id: u64,
    waker: Option<Waker>,
    _budget: Option<FairWaitBudgetPermit>,
}

#[derive(Default)]
struct FairWaitQueueState {
    next_id: u64,
    queue: VecDeque<FairWaiterEntry>,
}

/// FIFO gate used by bounded admissions that only queue requests larger than
/// their fixed class reservation.
///
/// Requests within a fixed reservation never join the borrower queue. They
/// bypass it when their reservation covers them and otherwise fail fast, so a
/// large borrower cannot retain an arbitrary number of smaller payloads behind
/// itself.
pub(crate) struct FairWaitQueue {
    state: Mutex<FairWaitQueueState>,
    budget: Option<Arc<FairWaitBudget>>,
}

pub(crate) enum FairAdmission<T> {
    Ready(T),
    Waiting(FairWaiter),
}

impl FairWaitQueue {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(FairWaitQueueState::default()),
            budget: None,
        }
    }

    pub(crate) fn with_budget(budget: Arc<FairWaitBudget>) -> Self {
        Self {
            state: Mutex::new(FairWaitQueueState::default()),
            budget: Some(budget),
        }
    }

    pub(crate) fn try_admit_unqueued<T, E>(
        &self,
        blocked_error: E,
        attempt: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.queue.is_empty() {
            return Err(blocked_error);
        }
        attempt()
    }

    pub(crate) fn admit_or_wait<T, E>(
        self: &Arc<Self>,
        cost: usize,
        budget_error: E,
        attempt: impl FnOnce() -> Option<T>,
    ) -> std::result::Result<FairAdmission<T>, E> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.queue.is_empty() {
            if let Some(value) = attempt() {
                return Ok(FairAdmission::Ready(value));
            }
        }
        let budget = match &self.budget {
            Some(budget) => Some(budget.try_acquire(cost).ok_or(budget_error)?),
            None => None,
        };
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        state.queue.push_back(FairWaiterEntry {
            id,
            waker: None,
            _budget: budget,
        });
        Ok(FairAdmission::Waiting(FairWaiter {
            queue: self.clone(),
            id,
            active: true,
        }))
    }

    pub(crate) fn wake_front(&self) {
        let waker = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue
            .front()
            .and_then(|waiter| waiter.waker.clone());
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// Acquire from a FIFO fair-admission queue after the caller validates the request ceiling.
pub(crate) async fn acquire_fair<T>(
    queue: &Arc<FairWaitQueue>,
    cost: usize,
    budget_error: crate::error::Error,
    closed_error: impl Fn() -> crate::error::Error,
    mut attempt: impl FnMut() -> crate::error::Result<T>,
) -> crate::error::Result<T> {
    let mut try_acquire = || attempt().ok();
    match queue.admit_or_wait(cost, budget_error, &mut try_acquire)? {
        FairAdmission::Ready(value) => Ok(value),
        FairAdmission::Waiting(mut waiter) => {
            poll_fn(|context| match waiter.poll(context, &mut try_acquire) {
                Poll::Ready(Some(value)) => Poll::Ready(Ok(value)),
                Poll::Ready(None) => Poll::Ready(Err(closed_error())),
                Poll::Pending => Poll::Pending,
            })
            .await
        }
    }
}

/// Yield one executor poll without depending on a particular async runtime.
pub(crate) async fn yield_executor_once() {
    let mut yielded = false;
    poll_fn(move |context| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

pub(crate) struct FairWaiter {
    queue: Arc<FairWaitQueue>,
    id: u64,
    active: bool,
}

impl FairWaiter {
    pub(crate) fn poll<T>(
        &mut self,
        context: &mut Context<'_>,
        attempt: impl FnOnce() -> Option<T>,
    ) -> Poll<Option<T>> {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(position) = state.queue.iter().position(|waiter| waiter.id == self.id) else {
            return Poll::Ready(None);
        };
        if let Some(waiter) = state.queue.get_mut(position) {
            waiter.waker = Some(context.waker().clone());
        }
        if position != 0 {
            return Poll::Pending;
        }
        let Some(value) = attempt() else {
            return Poll::Pending;
        };
        state.queue.pop_front();
        let next = state.queue.front().and_then(|waiter| waiter.waker.clone());
        self.active = false;
        drop(state);
        if let Some(waker) = next {
            waker.wake();
        }
        Poll::Ready(Some(value))
    }
}

impl Drop for FairWaiter {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was_front = state
            .queue
            .front()
            .is_some_and(|waiter| waiter.id == self.id);
        state.queue.retain(|waiter| waiter.id != self.id);
        let next = was_front
            .then(|| state.queue.front().and_then(|waiter| waiter.waker.clone()))
            .flatten();
        drop(state);
        if let Some(waker) = next {
            waker.wake();
        }
    }
}

/// Get local utc timestamp (millisecond)
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub fn get_epoch_ms() -> u128 {
    Utc::now().timestamp_millis() as u128
}

/// Get local utc timestamp (millisecond)
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub fn get_epoch_ms() -> u128 {
    let now = js_sys::Date::now();
    if now.is_finite() && now > 0.0 {
        now as u128
    } else {
        0
    }
}

pub(crate) fn get_epoch_ms_i64() -> i64 {
    i64::try_from(get_epoch_ms()).unwrap_or(i64::MAX)
}

/// Sleep for `duration` on the active runtime.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(crate) async fn sleep(duration: std::time::Duration) {
    futures_timer::Delay::new(duration).await;
}

/// Sleep for `duration` on the JavaScript event loop.
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(crate) async fn sleep(duration: std::time::Duration) {
    let millis = i32::try_from(duration.as_millis()).unwrap_or(i32::MAX);
    if let Err(error) = js_utils::window_sleep(millis).await {
        tracing::error!("failed to wait for timeout: {:?}", error);
    }
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
/// Toolset for wasm
pub mod js_value {
    use serde::de::DeserializeOwned;
    use serde::Serialize;
    use serde::Serializer;
    use wasm_bindgen::JsValue;

    use crate::error::Error;
    use crate::error::Result;

    /// From serde to JsValue
    pub fn serialize(obj: &impl Serialize) -> Result<JsValue> {
        let serializer = serde_wasm_bindgen::Serializer::json_compatible();
        serializer
            .serialize_some(&obj)
            .map_err(Error::SerdeWasmBindgenError)
    }

    /// From JsValue to serde
    pub fn deserialize<T: DeserializeOwned>(obj: impl Into<JsValue>) -> Result<T> {
        serde_wasm_bindgen::from_value(obj.into()).map_err(Error::SerdeWasmBindgenError)
    }

    /// From JsValue to serde_json::Value
    pub fn json_value(obj: impl Into<JsValue>) -> Result<serde_json::Value> {
        let s = js_sys::JSON::stringify(&obj.into())
            .map_err(|_| Error::JsError("failed to stringify obj".to_string()))?;

        serde_json::from_str(&String::from(s)).map_err(Error::Deserialize)
    }
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
/// Helpers for adapting JavaScript functions into async Rust callbacks.
pub mod js_func {
    /// This macro will generate a wrapper for mapping a js_sys::Function with type fn(T, T, T, T) -> Promise<()>
    /// to native function
    /// # Example:
    /// For macro calling: of!(of2, a: T0, b: T1);
    /// Will generate code:
    /// ```rust,no_run
    /// pub fn of2<
    ///     'a,
    ///     'b: 'a,
    ///     T0: TryInto<wasm_bindgen::JsValue> + Clone,
    ///     T1: TryInto<wasm_bindgen::JsValue> + Clone,
    /// >(
    ///     func: &js_sys::Function,
    /// ) -> Box<
    ///     dyn Fn(
    ///         T0,
    ///         T1,
    ///     ) -> std::pin::Pin<
    ///         Box<dyn std::future::Future<Output = rings_core::error::Result<()>> + 'b>,
    ///     >,
    /// >
    /// where
    ///     T0::Error: std::fmt::Debug,
    ///     T1::Error: std::fmt::Debug,
    ///     T0: 'b,
    ///     T1: 'b,
    /// {
    ///     let func = func.clone();
    ///     Box::new(
    ///         move |a: T0,
    ///               b: T1|
    ///               -> std::pin::Pin<
    ///             Box<dyn std::future::Future<Output = rings_core::error::Result<()>>>,
    ///         > {
    ///             let func = func.clone();
    ///             Box::pin(async move {
    ///                 let func = func.clone();
    ///                 let params = js_sys::Array::new();
    ///                 let a: wasm_bindgen::JsValue = a
    ///                     .clone()
    ///                     .try_into()
    ///                     .map_err(|e| rings_core::error::Error::JsError(format!("{:?}", e)))?;
    ///                 params.push(&a);
    ///                 let b: wasm_bindgen::JsValue = b
    ///                     .clone()
    ///                     .try_into()
    ///                     .map_err(|e| rings_core::error::Error::JsError(format!("{:?}", e)))?;
    ///                 params.push(&b);
    ///                 wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(
    ///                     func.apply(&wasm_bindgen::JsValue::NULL, &params)
    ///                         .map_err(|e| rings_core::error::Error::from(js_sys::Error::from(e)))?,
    ///                 ))
    ///                 .await
    ///                 .map_err(|e| rings_core::error::Error::from(js_sys::Error::from(e)))?;
    ///                 Ok(())
    ///             })
    ///         },
    ///     )
    /// }
    /// ```
    #[macro_export]
    macro_rules! of {
	($func: ident, $($name:ident: $type: ident),+$(,)?) => {
            #[doc = "Wrap a JavaScript function in an async Rust callback."]
	    pub fn $func<'a, 'b: 'a, $($type: TryInto<wasm_bindgen::JsValue> + Clone),+>(
	        func: &js_sys::Function,
	    ) -> Box<dyn Fn($($type),+) -> std::pin::Pin<Box<dyn std::future::Future<Output = $crate::error::Result<()>> + 'b>>>
	    where  $($type::Error: std::fmt::Debug),+,
		$($type: 'b),+
	    {
		let func = func.clone();
		Box::new(
		    move |$($name: $type,)+| -> std::pin::Pin<Box<dyn std::future::Future<Output = $crate::error::Result<()>>>> {
			let func = func.clone();
			Box::pin(async move {
			    let func = func.clone();
			    let params = js_sys::Array::new();
			    $(
				let $name: wasm_bindgen::JsValue = $name.clone().try_into().map_err(|e| $crate::error::Error::JsError(format!("{:?}", e)))?;
				params.push(&$name);
			    )+
			    wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(
				func.apply(
				    &wasm_bindgen::JsValue::NULL,
				    &params
				)
				    .map_err(|e| $crate::error::Error::from(js_sys::Error::from(e)))?,
			    ))
				.await
				.map_err(|e| $crate::error::Error::from(js_sys::Error::from(e)))?;
			    Ok(())
			})
		    },
		)
	    }
	}
    }

    of!(of1, a: T0);
    of!(of2, a: T0, b: T1);
    of!(of3, a: T0, b: T1, c: T2);
    of!(of4, a: T0, b: T1, c: T2, d: T3);
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
/// Browser and worker utility functions for wasm runtimes.
pub mod js_utils {
    use std::future::Future;

    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast;
    use wasm_bindgen::JsValue;

    /// JavaScript global scope variants supported by Rings wasm utilities.
    pub enum Global {
        /// Browser window global scope.
        Window(web_sys::Window),
        /// Dedicated or shared worker global scope.
        WorkerGlobal(web_sys::WorkerGlobalScope),
        /// Service worker global scope.
        ServiceWorkerGlobal(web_sys::ServiceWorkerGlobalScope),
    }

    impl Global {
        /// Schedule a zero-argument timeout callback on this global scope.
        pub fn set_timeout_0(
            &self,
            callback: &js_sys::Function,
            millis: i32,
        ) -> Result<i32, JsValue> {
            match self {
                Global::Window(global) => {
                    global.set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
                }
                Global::WorkerGlobal(global) => {
                    global.set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
                }
                Global::ServiceWorkerGlobal(global) => {
                    global.set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
                }
            }
        }
    }

    /// Detect the current JavaScript global scope.
    pub fn global() -> Option<Global> {
        let obj = JsValue::from(js_sys::global());
        if obj.has_type::<web_sys::Window>() {
            return Some(Global::Window(web_sys::Window::from(obj)));
        }
        if obj.has_type::<web_sys::WorkerGlobalScope>() {
            return Some(Global::WorkerGlobal(web_sys::WorkerGlobalScope::from(obj)));
        }
        if obj.has_type::<web_sys::ServiceWorkerGlobalScope>() {
            return Some(Global::ServiceWorkerGlobal(
                web_sys::ServiceWorkerGlobalScope::from(obj),
            ));
        }
        None
    }

    fn resolve_sleep(resolve: &js_sys::Function) {
        if let Err(error) = resolve.call0(&JsValue::NULL) {
            tracing::error!("Failed to resolve sleep promise: {:?}", error);
        }
    }

    fn reject_sleep(reject: &js_sys::Function, error: JsValue) {
        if let Err(reject_error) = reject.call1(&JsValue::NULL, &error) {
            tracing::error!("Failed to reject sleep promise: {:?}", reject_error);
        }
    }

    fn schedule_sleep<F>(resolve: js_sys::Function, reject: js_sys::Function, schedule: F)
    where F: FnOnce(&js_sys::Function) -> Result<i32, JsValue> {
        let func = Closure::once_into_js(move || {
            resolve_sleep(&resolve);
        });
        let callback = func.as_ref().unchecked_ref();
        if let Err(error) = schedule(callback) {
            tracing::error!("Failed to schedule sleep timeout: {:?}", error);
            reject_sleep(&reject, error);
        }
    }

    /// Return a JavaScript future that resolves after `millis` milliseconds.
    pub fn window_sleep(millis: i32) -> wasm_bindgen_futures::JsFuture {
        let promise = match global() {
            None => js_sys::Promise::reject(&JsValue::from_str("No global scope for window_sleep")),
            Some(global) => js_sys::Promise::new(&mut move |resolve, reject| {
                schedule_sleep(resolve, reject, |callback| {
                    global.set_timeout_0(callback, millis)
                });
            }),
        };
        wasm_bindgen_futures::JsFuture::from(promise)
    }

    /// Spawn a wasm-local interval loop that waits for each tick task to finish.
    pub fn spawn_interval<F, Fut>(millis: i32, mut task: F)
    where
        F: FnMut() -> Fut + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                if let Err(error) = window_sleep(millis).await {
                    tracing::error!("failed to wait for interval tick: {:?}", error);
                    return;
                }

                task().await;
            }
        });
    }
}
