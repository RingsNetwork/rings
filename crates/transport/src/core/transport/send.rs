use std::sync::Arc;
use std::time::Duration;

use crate::core::admission::AdmissionEvent;
use crate::core::admission::AdmissionPhase;
use crate::core::admission::AtomicAdmission;

/// Maximum time a native backend drives an irrevocable send to completion.
pub const IRREVOCABLE_SEND_COMPLETION_TIMEOUT: Duration = Duration::from_secs(25);
/// Maximum cleanup interval after a connection generation becomes terminal.
pub const CONNECTION_RETIRE_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(target_family = "wasm")]
type SendPermitPredicate = dyn Fn() -> bool;
#[cfg(not(target_family = "wasm"))]
type SendPermitPredicate = dyn Fn() -> bool + Send + Sync;

#[cfg(target_family = "wasm")]
type SendPermitIrrevocableGuard = dyn for<'a> Fn(SendPermitClaim<'a>);
#[cfg(not(target_family = "wasm"))]
type SendPermitIrrevocableGuard = dyn for<'a> Fn(SendPermitClaim<'a>) + Send + Sync;

/// A one-send predicate checked at the backend's final cancellable send-admission boundary.
///
/// The permit is intentionally not `Clone`: one constructed value authorizes at
/// most one call to [`super::ConnectionInterface::send_message_with_permit`]. Returning
/// `false` means the higher-level condition that authorized the send no longer
/// holds, so the backend must not start its send primitive. The backend marks
/// acceptance only after its send primitive confirms queue admission.
pub struct SendPermit {
    predicate: Arc<SendPermitPredicate>,
    irrevocable_guard: Arc<SendPermitIrrevocableGuard>,
    state: AtomicAdmission,
}

/// One-use capability that linearizes backend admission with an external guard.
pub struct SendPermitClaim<'a> {
    state: &'a AtomicAdmission,
}

/// Proof that a backend crossed the final cancellation-safe send boundary.
pub struct IrrevocableSendPermit {
    state: AtomicAdmission,
}

/// Retires a connection generation when an irrevocable send does not reach acceptance.
#[cfg(any(
    feature = "dummy",
    feature = "native-webrtc",
    feature = "web-sys-webrtc",
    test
))]
pub(crate) struct IrrevocableSendGuard<F: FnOnce()> {
    acceptance: SendAcceptance,
    permit: Option<IrrevocableSendPermit>,
    retire: Option<F>,
}

/// Shared observation of whether a one-send permit reached its linearization point.
#[derive(Clone)]
pub struct SendAcceptance {
    state: AtomicAdmission,
}

impl SendAcceptance {
    /// Return whether the backend crossed its final cancellation-safe boundary.
    pub fn is_irrevocable(&self) -> bool {
        matches!(
            self.state.phase(),
            AdmissionPhase::Irrevocable | AdmissionPhase::Accepted
        )
    }

    /// Return whether the backend accepted the send permit.
    pub fn is_accepted(&self) -> bool {
        self.state.phase() == AdmissionPhase::Accepted
    }

    /// Return whether an irrevocable send failed before backend acceptance.
    pub fn failed_after_irrevocable(&self) -> bool {
        self.state.phase() == AdmissionPhase::Irrevocable
    }

    /// Atomically cancel a send that has not crossed its irrevocable boundary.
    pub fn try_cancel(&self) -> bool {
        self.state.try_transition(AdmissionEvent::Cancel).is_ok()
    }
}

impl SendPermitClaim<'_> {
    /// Claim the final cancellation-safe boundary while the caller's guards are held.
    pub fn try_claim(self) -> bool {
        self.state
            .try_transition(AdmissionEvent::MarkIrrevocable)
            .is_ok()
    }
}

impl SendPermit {
    /// Construct a send permit for a single-threaded wasm transport.
    #[cfg(target_family = "wasm")]
    pub fn new(predicate: impl Fn() -> bool + 'static) -> Self {
        Self {
            predicate: Arc::new(predicate),
            irrevocable_guard: Arc::new(|claim| {
                let _claimed = claim.try_claim();
            }),
            state: AtomicAdmission::new(),
        }
    }

    /// Construct a send permit for a native transport.
    #[cfg(not(target_family = "wasm"))]
    pub fn new(predicate: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            predicate: Arc::new(predicate),
            irrevocable_guard: Arc::new(|claim| {
                let _claimed = claim.try_claim();
            }),
            state: AtomicAdmission::new(),
        }
    }

    /// Construct an unconditional permit for direct low-level transport users.
    pub fn always() -> Self {
        Self::new(|| true)
    }

    /// Evaluate this permit where a backend is about to start its send.
    pub fn allows(&self) -> bool {
        (self.predicate)()
    }

    /// Add a final-boundary guard that calls `claim.try_claim()` only while its
    /// external invariants hold. A successful claim always produces the proof
    /// returned by [`Self::try_mark_irrevocable`].
    #[cfg(target_family = "wasm")]
    pub fn with_irrevocable_guard(
        mut self,
        guard: impl for<'a> Fn(SendPermitClaim<'a>) + 'static,
    ) -> Self {
        self.irrevocable_guard = Arc::new(guard);
        self
    }

    /// Add a final-boundary guard that calls `claim.try_claim()` only while its
    /// external invariants hold. A successful claim always produces the proof
    /// returned by [`Self::try_mark_irrevocable`].
    #[cfg(not(target_family = "wasm"))]
    pub fn with_irrevocable_guard(
        mut self,
        guard: impl for<'a> Fn(SendPermitClaim<'a>) + Send + Sync + 'static,
    ) -> Self {
        self.irrevocable_guard = Arc::new(guard);
        self
    }

    /// Evaluate the permit and cross the final cancellation-safe boundary.
    ///
    /// A backend must call this synchronously before its first non-cancellation-safe
    /// yield, write, or spawned task. After this returns a proof token, the write
    /// must be driven to completion while its connection remains usable. A caller
    /// may abandon the returned future only after permanently retiring that
    /// connection generation and initiating connection close.
    pub fn try_mark_irrevocable(self) -> Option<IrrevocableSendPermit> {
        if !self.allows() {
            return None;
        }
        (self.irrevocable_guard)(SendPermitClaim { state: &self.state });
        if self.state.phase() != AdmissionPhase::Irrevocable {
            return None;
        }
        Some(IrrevocableSendPermit { state: self.state })
    }

    /// Return a shared observer for the backend acceptance boundary.
    pub fn acceptance(&self) -> SendAcceptance {
        SendAcceptance {
            state: self.state.clone(),
        }
    }
}

impl IrrevocableSendPermit {
    /// Consume the proof and publish successful backend queue admission.
    pub fn mark_accepted(self) {
        let transitioned = self.state.try_transition(AdmissionEvent::Accept);
        debug_assert!(
            transitioned.is_ok(),
            "send acceptance requires irrevocable state"
        );
    }
}

#[cfg(any(
    feature = "dummy",
    feature = "native-webrtc",
    feature = "web-sys-webrtc",
    test
))]
impl<F: FnOnce()> IrrevocableSendGuard<F> {
    pub(crate) fn new(acceptance: SendAcceptance, retire: F) -> Self {
        Self {
            acceptance,
            permit: None,
            retire: Some(retire),
        }
    }

    pub(crate) fn bind(&mut self, permit: IrrevocableSendPermit) {
        self.permit = Some(permit);
    }

    pub(crate) fn mark_accepted(mut self) {
        if let Some(permit) = self.permit.take() {
            permit.mark_accepted();
        }
        self.retire = None;
    }
}

#[cfg(any(
    feature = "dummy",
    feature = "native-webrtc",
    feature = "web-sys-webrtc",
    test
))]
impl<F: FnOnce()> Drop for IrrevocableSendGuard<F> {
    fn drop(&mut self) {
        let must_retire = self.acceptance.failed_after_irrevocable();
        drop(self.permit.take());
        if must_retire {
            if let Some(retire) = self.retire.take() {
                retire();
            }
        }
    }
}
