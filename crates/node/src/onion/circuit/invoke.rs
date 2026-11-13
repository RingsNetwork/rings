//! Intermediate one-shot `invoke` execution at a hop: argument decoding, per-`(shape, backend)`
//! quota admission, `W(f)` fixed-width sealing (L3), and exact `arr + L(f)` release timing (L4).
//!
//! ```text
//! Consumed(invoke⟨In, Out, L⟩, ā, v, υ) at arr:
//!   (ω, ā′) ← decode(ā)                                            else drop
//!   |v| = In ?                                                      else drop (L3)
//!   (f, ω) ∈ Reg_n ?                                                else drop (D2, silent)
//!   lease ← quota.admit((f, ω), policy, from, arr)                  else drop (D2, silent)
//!   spawn under lease:
//!     race ⟦(f, ω)⟧(ā′, v) against sleep(L(f))                      overrun ⇒ drop (L4)
//!     complete_invoke(f, arr, completed_at, out, υ)                 |out| ≠ W(f) ∨ late ⇒ drop
//!     await remainder of sleep(L(f));  enqueue (next, cell)         exact release at arr + L(f)
//! ```

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use futures::future::select;
use futures::future::Either;
use rings_core::dht::Did;
use rings_core::utils::SharedClock;
use rings_core::utils::SystemClock;
use rings_runtime::sleep;
use rings_runtime::MaybeSend;
use rings_runtime::MaybeSendSync;
use web_time::Instant;

use super::OnionApplicationInput;
use super::OnionExpiry;
use super::OnionLink;
use super::OnionLinkSender;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::onion::invoke_accounting::OnionInvokeAccounting;
use crate::onion::invoke_accounting::OnionInvokeLease;
use crate::onion::invoke_descriptor::OnionInvokePolicy;
use crate::onion::pipeline::OnionInvokeArguments;
use crate::onion::role::OnionInvokeOffer;
use crate::onion::signature::OnionBackendName;
use crate::onion::signature::OnionInvokeShape;
use crate::onion::sphinx::cell::OnionCell;
use crate::onion::sphinx::cell::OnionProduceError;
use crate::onion::sphinx::cell::OnionSurb;

/// Decoded arguments and unpadded input handed to a registered `invoke` backend (#834 D2, D3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionInvokeCall {
    /// Shape `f ∈ Σ_I` of the invocation.
    pub shape: OnionInvokeShape,
    /// Canonical registered backend name `ω ∈ Ω`.
    pub backend: OnionBackendName,
    /// Client-supplied inline constants `ā′` sealed after `ω` inside `ā`.
    pub constants: Vec<u8>,
    /// Unpadded carried input `v̂_i` of exact length `shape.input().bytes()`.
    pub input: Vec<u8>,
}

/// Interpretation `⟦(f, ω)⟧ : Args_f × In_f → M Out_f` of one registered `(shape, backend)` pair
/// (#834 D2).
///
/// Law (L3, L4): `invoke` must complete within `shape.latency().duration()` and return a byte
/// vector of exact width `shape.output_bytes()` (`W(f)`). Any error, wrong output width, or
/// overrun drops the cell silently without emitting on the wire.
#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
pub trait OnionInvokeBackend: MaybeSendSync {
    /// Evaluate one invocation and return its `W(f)`-byte output.
    async fn invoke(&self, call: OnionInvokeCall) -> Result<Vec<u8>>;
}

/// Adapter lifting an async closure `Fn(OnionInvokeCall) -> Fut` into an [`OnionInvokeBackend`].
pub struct OnionInvokeFn<F>(pub F);

#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
impl<F, Fut> OnionInvokeBackend for OnionInvokeFn<F>
where
    F: Fn(OnionInvokeCall) -> Fut + MaybeSendSync,
    Fut: Future<Output = Result<Vec<u8>>> + MaybeSend,
{
    async fn invoke(&self, call: OnionInvokeCall) -> Result<Vec<u8>> {
        (self.0)(call).await
    }
}

/// Pure outcome of `complete_invoke`: a sealed cell ready to be released after `wait` at
/// `release_at_ms = arr + L(f)` (#834 L3, L4).
#[derive(Debug)]
pub(crate) struct OnionPreparedInvoke {
    /// Next hop `next_i` from `υ`.
    pub(crate) next: Did,
    /// Sealed `b`-byte cell carrying `pad(output)` under `σ_out`'s segment keys.
    pub(crate) cell: OnionCell,
    /// Exact release deadline `arr + L(f)` in milliseconds.
    pub(crate) release_at_ms: u128,
    /// Remaining delay `release_at_ms − completed_at_ms` under the injected clock.
    pub(crate) wait: Duration,
    /// Expiry `x_υ` of the consumed SURB.
    pub(crate) expiry: OnionExpiry,
}

/// Reason why `complete_invoke` dropped an `invoke` result (L3, L4).
#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum OnionInvokeDrop {
    /// Evaluation finished after `release_at_ms = arr + L(f)` (L4: never release late).
    #[error("invoke completed at {completed_at_ms} ms past release deadline {release_at_ms} ms")]
    Overrun {
        completed_at_ms: u128,
        release_at_ms: u128,
    },
    /// Output length did not match `W(f)` (L3: fail closed, never truncate or improperly pad).
    #[error("invoke output of {actual} bytes does not match W(f) = {expected} bytes")]
    OutputWidth { actual: usize, expected: usize },
    /// SURB expired at or before `release_at_ms`.
    #[error("invoke SURB expires before release deadline")]
    Expired,
    /// Sealing the output into `υ` failed.
    #[error("invoke SURB seal failed: {0}")]
    Produce(String),
}

/// Pure L3/L4 judge over an `invoke` output at `completed_at_ms` (#834 Algorithm Hop, lines 17–21).
///
/// Rejects late completion (`completed_at_ms > arr + L(f)`), expired SURBs (`x ≤ arr + L(f)`), and
/// any output whose byte width differs from `W(f) = shape.output_bytes()`, then seals `output`
/// into `surb` at the loop's constant class `b`.
pub(crate) fn complete_invoke(
    shape: OnionInvokeShape,
    received_at_ms: u128,
    completed_at_ms: u128,
    output: &[u8],
    surb: OnionSurb,
) -> std::result::Result<OnionPreparedInvoke, OnionInvokeDrop> {
    let release_at_ms = received_at_ms.saturating_add(u128::from(shape.latency().millis()));
    if completed_at_ms > release_at_ms {
        return Err(OnionInvokeDrop::Overrun {
            completed_at_ms,
            release_at_ms,
        });
    }
    if surb.expiry().has_passed_at(release_at_ms) {
        return Err(OnionInvokeDrop::Expired);
    }
    let expected = shape.output_bytes();
    if output.len() != expected {
        return Err(OnionInvokeDrop::OutputWidth {
            actual: output.len(),
            expected,
        });
    }
    let expiry = surb.expiry();
    let (next, cell) = surb
        .produce(output)
        .map_err(|error: OnionProduceError| OnionInvokeDrop::Produce(error.to_string()))?;
    let wait_ms = u64::try_from(release_at_ms - completed_at_ms).unwrap_or(u64::MAX);
    Ok(OnionPreparedInvoke {
        next,
        cell,
        release_at_ms,
        wait: Duration::from_millis(wait_ms),
        expiry,
    })
}

struct RegisteredInvoke {
    policy: OnionInvokePolicy,
    backend: Arc<rings_runtime::maybe_send_sync!(dyn OnionInvokeBackend)>,
}

/// Shared `(shape, backend)` interpretation table of one node's Σ-algebra (#834 D2).
#[derive(Clone)]
pub(crate) struct OnionInvokeRegistry {
    offered_policies: Arc<BTreeMap<(OnionInvokeShape, OnionBackendName), OnionInvokePolicy>>,
    entries: Arc<RwLock<BTreeMap<(OnionInvokeShape, OnionBackendName), RegisteredInvoke>>>,
    accounting: OnionInvokeAccounting,
    link_sender: OnionLinkSender,
    clock: SharedClock,
}

impl Default for OnionInvokeRegistry {
    fn default() -> Self {
        Self::new_with_clock(None, OnionLinkSender::default(), Arc::new(SystemClock))
    }
}

impl OnionInvokeRegistry {
    pub(crate) fn new_with_clock(
        offer: Option<&OnionInvokeOffer>,
        link_sender: OnionLinkSender,
        clock: SharedClock,
    ) -> Self {
        let offered_policies = offer
            .map(|offer| {
                offer
                    .entries()
                    .map(|(key, policy)| (key.clone(), *policy))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            offered_policies: Arc::new(offered_policies),
            entries: Arc::new(RwLock::new(BTreeMap::new())),
            accounting: OnionInvokeAccounting::default(),
            link_sender,
            clock,
        }
    }

    /// Register `backend_impl` for `(shape, backend)` using the policy from the node's offer (if
    /// offered) or `fallback_policy`.
    pub(crate) fn register(
        &self,
        shape: OnionInvokeShape,
        backend: OnionBackendName,
        fallback_policy: OnionInvokePolicy,
        backend_impl: impl OnionInvokeBackend + 'static,
    ) -> Result<()> {
        let policy = self
            .offered_policies
            .get(&(shape, backend.clone()))
            .copied()
            .unwrap_or(fallback_policy);
        let mut guard = self.entries.write().map_err(|_| Error::Lock)?;
        guard.insert(shape_key(shape, backend), RegisteredInvoke {
            policy,
            backend: Arc::new(backend_impl),
        });
        Ok(())
    }

    /// Validate, charge quota, and spawn the timed `L(f)` evaluation of one consumed `invoke`
    /// layer. Unregistered backends, malformed arguments, wrong input widths, or exhausted quotas
    /// drop silently under the transition gate without emitting on the wire.
    pub(crate) fn evaluate(
        &self,
        scope: &Scope,
        shape: OnionInvokeShape,
        input: OnionApplicationInput,
    ) -> Result<()> {
        let started = Instant::now();
        let Some(decoded_args) = OnionInvokeArguments::decode(&input.arguments) else {
            tracing::debug!(?shape, "drop onion invoke with malformed layer arguments");
            return Ok(());
        };
        let (backend, constants) = decoded_args.into_parts();
        if input.value.as_slice().len() != shape.input().bytes() {
            tracing::debug!(
                ?shape,
                %backend,
                actual = input.value.as_slice().len(),
                expected = shape.input().bytes(),
                "drop onion invoke with mismatched carried input width"
            );
            return Ok(());
        }
        let Some((policy, backend_impl)) = ({
            let guard = self.entries.read().map_err(|_| Error::Lock)?;
            guard
                .get(&(shape, backend.clone()))
                .map(|entry| (entry.policy, Arc::clone(&entry.backend)))
        }) else {
            tracing::debug!(
                ?shape,
                %backend,
                "drop onion invoke for unregistered (shape, backend)"
            );
            return Ok(());
        };
        let Ok(lease) =
            self.accounting
                .admit(shape, &backend, &policy, input.from, input.received_at_ms)
        else {
            tracing::debug!(?shape, %backend, "drop onion invoke exceeding quota");
            return Ok(());
        };
        let latency = shape.latency().duration();
        let release_at_ms = input
            .received_at_ms
            .saturating_add(u128::from(shape.latency().millis()));
        if input.surb.expiry().has_passed_at(release_at_ms) {
            tracing::debug!(?shape, %backend, "drop onion invoke expiring before release");
            return Ok(());
        }

        let carried_input = input.value.as_slice().to_vec();
        let surb = input.surb;
        let received_at_ms = input.received_at_ms;
        let link_sender = self.link_sender.clone();
        let clock = self.clock.clone();
        let send_scope = scope.clone();

        scope.tasks().spawn(async move {
            let call = OnionInvokeCall {
                shape,
                backend,
                constants,
                input: carried_input,
            };
            let Some(prepared) = run_invoke_under_lease(
                lease,
                backend_impl,
                call,
                latency,
                received_at_ms,
                started,
                *surb,
            )
            .await
            else {
                return;
            };

            if prepared.expiry.has_passed_at(clock.now_ms()) {
                tracing::debug!(?shape, "drop onion invoke cell after SURB expiry");
                return;
            }
            if let Err(error) =
                link_sender.enqueue(send_scope, OnionLink::new(prepared.next), prepared.cell)
            {
                tracing::debug!(%error, next = %prepared.next, "drop onion invoke cell on lane");
            }
        })?;
        Ok(())
    }
}

async fn run_invoke_under_lease(
    _lease: OnionInvokeLease,
    backend_impl: Arc<rings_runtime::maybe_send_sync!(dyn OnionInvokeBackend)>,
    call: OnionInvokeCall,
    latency: Duration,
    received_at_ms: u128,
    started: Instant,
    surb: OnionSurb,
) -> Option<OnionPreparedInvoke> {
    let shape = call.shape;
    let mut timer = pin!(sleep(latency));
    let output = {
        let eval_fut = pin!(backend_impl.invoke(call));
        match select(eval_fut, timer.as_mut()).await {
            Either::Left((Ok(output), _)) => output,
            Either::Left((Err(error), _)) => {
                tracing::debug!(%error, ?shape, "drop failed onion invoke evaluation");
                let _ = timer.await;
                return None;
            }
            Either::Right((_, _)) => {
                tracing::debug!(?shape, "drop onion invoke that overran L(f)");
                return None;
            }
        }
    };

    let completed_at_ms = received_at_ms.saturating_add(started.elapsed().as_millis());
    let prepared = match complete_invoke(
        shape,
        received_at_ms,
        completed_at_ms,
        output.as_slice(),
        surb,
    ) {
        Ok(prepared) => prepared,
        Err(drop_reason) => {
            tracing::debug!(%drop_reason, ?shape, "drop onion invoke output");
            let _ = timer.await;
            return None;
        }
    };

    // Wait out the exact remainder of the single `L(f)` timer started at receipt (L4).
    tracing::trace!(
        ?shape,
        release_at_ms = prepared.release_at_ms,
        wait_ms = prepared.wait.as_millis(),
        "awaiting L(f) release deadline for onion invoke cell"
    );
    let _ = timer.await;
    Some(prepared)
}

const fn shape_key(
    shape: OnionInvokeShape,
    backend: OnionBackendName,
) -> (OnionInvokeShape, OnionBackendName) {
    (shape, backend)
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::Arc;
    use std::time::Duration;

    use futures::future::select;
    use futures::future::Either;
    use rings_core::dht::Did;
    use rings_runtime::sleep;
    use web_time::Instant;

    use super::complete_invoke;
    use super::run_invoke_under_lease;
    use super::OnionInvokeBackend;
    use super::OnionInvokeCall;
    use super::OnionInvokeDrop;
    use super::OnionInvokeFn;
    use crate::error::Error;
    use crate::onion::invoke_accounting::OnionInvokeAccounting;
    use crate::onion::invoke_descriptor::OnionInvokePolicy;
    use crate::onion::signature::OnionBackendName;
    use crate::onion::signature::OnionInputType;
    use crate::onion::signature::OnionInvokeShape;
    use crate::onion::signature::OnionLatencyClass;
    use crate::onion::signature::OnionWidthClass;
    use crate::onion::sphinx::cell::OnionSurb;
    use crate::onion::sphinx::class::OnionLoopClass;
    use crate::onion::tests::fixture::grid_expiry;

    fn sample_surb() -> OnionSurb {
        let block = [
            [0x11_u8; 20].as_slice(),
            vec![0x22_u8; 2919].as_slice(),
            [0x33_u8; 32].as_slice(),
            grid_expiry(5).to_wire_ms().to_be_bytes().as_slice(),
        ]
        .concat();
        OnionSurb::decode(OnionLoopClass::DEFAULT, &block).expect("valid surb")
    }

    fn shape_64_500ms() -> OnionInvokeShape {
        OnionInvokeShape::new(
            OnionInputType::Fixed(OnionWidthClass::B32),
            OnionWidthClass::B64,
            OnionLatencyClass::Ms500,
        )
    }

    /// L3 & L4 under an injected clock: fast evaluations compute the exact remainder up to
    /// `arr + L(f)`, boundary `arr + L(f)` waits `0`, `> arr + L(f)` drops on overrun, and any
    /// output width `≠ W(f)` drops fail-closed while preserving constant cell geometry `b`.
    #[test]
    fn test_complete_invoke_enforces_l3_width_and_l4_timing_under_injected_clock() {
        let shape = shape_64_500ms();
        let arr_ms = 60_000_u128;

        // Fast completion at arr + 120 ms waits 380 ms until release_at_ms = 60_500 ms.
        let prepared = complete_invoke(shape, arr_ms, 60_120, &[0xab; 64], sample_surb())
            .expect("admissible invoke completion");
        assert_eq!(prepared.release_at_ms, 60_500);
        assert_eq!(prepared.wait, Duration::from_millis(380));
        assert_eq!(
            prepared.cell.into_bytes().len(),
            OnionLoopClass::DEFAULT.cell_bytes()
        );

        // Exact deadline completion waits 0 ms.
        let on_deadline = complete_invoke(shape, arr_ms, 60_500, &[0xcd; 64], sample_surb())
            .expect("exact deadline completion");
        assert_eq!(on_deadline.wait, Duration::ZERO);

        // Overrun by 1 ms is dropped, never released late (L4).
        assert_eq!(
            complete_invoke(shape, arr_ms, 60_501, &[0xab; 64], sample_surb()).unwrap_err(),
            OnionInvokeDrop::Overrun {
                completed_at_ms: 60_501,
                release_at_ms: 60_500,
            }
        );

        // Output wider or narrower than W(f) = 64 bytes is dropped (L3).
        for bad_len in [0_usize, 63, 65, 128] {
            assert_eq!(
                complete_invoke(shape, arr_ms, 60_010, &vec![0x11; bad_len], sample_surb())
                    .unwrap_err(),
                OnionInvokeDrop::OutputWidth {
                    actual: bad_len,
                    expected: 64,
                }
            );
        }
    }

    /// Early backend errors and output-width mismatches still hold their `OnionInvokeLease` until
    /// the full `L(f)` timer expires so concurrency quota release timing never leaks early failure.
    #[cfg_attr(rings_native, tokio::test)]
    #[cfg_attr(rings_browser, wasm_bindgen_test::wasm_bindgen_test)]
    async fn test_early_invoke_error_and_width_mismatch_hold_lease_until_release_deadline() {
        let shape = OnionInvokeShape::new(
            OnionInputType::Fixed(OnionWidthClass::B32),
            OnionWidthClass::B64,
            OnionLatencyClass::Ms100,
        );
        let backend_name = OnionBackendName::parse("fail.early").expect("backend name");
        let policy = OnionInvokePolicy {
            max_concurrent: 1,
            max_invocations_per_minute: 0,
        };
        let accounting = OnionInvokeAccounting::default();
        let peer = Did::from(7_u32);
        let latency = shape.latency().duration();

        let err_backend: Arc<rings_runtime::maybe_send_sync!(dyn OnionInvokeBackend)> =
            Arc::new(OnionInvokeFn(|_call: OnionInvokeCall| async move {
                Err(Error::NoPermission)
            }));
        let bad_width_backend: Arc<rings_runtime::maybe_send_sync!(dyn OnionInvokeBackend)> =
            Arc::new(OnionInvokeFn(|_call: OnionInvokeCall| async move {
                Ok(vec![0xaa; 7])
            }));

        for backend_impl in [err_backend, bad_width_backend] {
            let lease = accounting
                .admit(shape, &backend_name, &policy, peer, 60_000)
                .expect("initial lease admitted");
            let call = OnionInvokeCall {
                shape,
                backend: backend_name.clone(),
                constants: Vec::new(),
                input: vec![0x11; 32],
            };
            let mut eval_fut = pin!(run_invoke_under_lease(
                lease,
                backend_impl,
                call,
                latency,
                60_000,
                Instant::now(),
                sample_surb(),
            ));

            // After 15 ms (< L(f) = 100 ms), the backend has already failed, but the task must
            // still be waiting on `timer` and holding its concurrency lease.
            let probe_timer = pin!(sleep(Duration::from_millis(15)));
            let probe = select(eval_fut.as_mut(), probe_timer).await;
            assert!(
                matches!(probe, Either::Right(_)),
                "early failure must not complete before L(f)"
            );
            assert!(
                accounting
                    .admit(shape, &backend_name, &policy, peer, 60_015)
                    .is_err(),
                "concurrency lease must remain held until L(f) expires"
            );

            // Once L(f) completes, the dropped evaluation returns None and frees the lease.
            assert!(eval_fut.await.is_none());
            let next_lease = accounting
                .admit(shape, &backend_name, &policy, peer, 60_120)
                .expect("lease released at L(f)");
            drop(next_lease);
        }
    }
}
