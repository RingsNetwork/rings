use std::sync::Arc;
use std::time::Duration;

use crate::callback::link_credit::LaneCreditReservation;
use crate::core::admission::AdmissionEvent;
use crate::core::admission::AdmissionPhase;
use crate::core::admission::AtomicAdmission;

/// Maximum time a native backend drives an irrevocable send to completion.
pub const IRREVOCABLE_SEND_COMPLETION_TIMEOUT: Duration = Duration::from_secs(25);
/// Maximum cleanup interval after a connection generation becomes terminal.
pub const CONNECTION_RETIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Predicate checked at the backend's final cancellable send boundary.
#[cfg(target_family = "wasm")]
type SendPermitPredicate = dyn Fn() -> bool;
/// Thread-safe predicate checked at the backend's final cancellable send boundary.
#[cfg(not(target_family = "wasm"))]
type SendPermitPredicate = dyn Fn() -> bool + Send + Sync;

/// Guard callback invoked to linearize an irrevocable send claim.
#[cfg(target_family = "wasm")]
type SendPermitIrrevocableGuard = dyn for<'a> Fn(SendPermitClaim<'a>);
/// Thread-safe guard callback invoked to linearize an irrevocable send claim.
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
    /// Higher-level authorization predicate checked before crossing the irrevocable boundary.
    predicate: Arc<SendPermitPredicate>,
    /// Linearization hook executed around marking the send irrevocable.
    irrevocable_guard: Arc<SendPermitIrrevocableGuard>,
    /// Shared atomic admission state machine for this send.
    state: AtomicAdmission,
    /// The lane credit the send is made under, when the caller reserved it ahead of the send.
    credit: Option<LaneCreditReservation>,
}

/// One-use capability that linearizes backend admission with an external guard.
pub struct SendPermitClaim<'a> {
    /// Borrowed admission state transitioned to irrevocable during linearization.
    state: &'a AtomicAdmission,
}

/// Proof that a backend crossed the final cancellation-safe send boundary.
pub struct IrrevocableSendPermit {
    /// Shared admission state awaiting final acceptance or rollback.
    state: AtomicAdmission,
}

/// Linear witness token proving that [`IrrevocableSendPermit::accept`] was called before returning
/// success from [`crate::callback::InnerTransportCallback::send_custom_wire`].
pub struct AcceptedSendPermit(());

/// RAII guard that retires a connection generation when an irrevocable send fails or is dropped
/// before reaching [`Self::mark_accepted`].
pub struct IrrevocableSendGuard<F: FnOnce()> {
    /// Shared acceptance observer checked on drop.
    acceptance: SendAcceptance,
    /// Irrevocable permit consumed when [`Self::mark_accepted`] succeeds.
    permit: Option<IrrevocableSendPermit>,
    /// Retirement callback invoked on drop if the send was not accepted.
    retire: Option<F>,
}

/// Shared observation of whether a one-send permit reached its linearization point.
#[derive(Clone)]
pub struct SendAcceptance {
    /// Underlying atomic admission phase for the tracked send.
    state: AtomicAdmission,
}

impl SendAcceptance {
    /// One coherent snapshot for the shared lifecycle's pure failure decision.
    #[cfg(any(
        feature = "native-webrtc",
        all(feature = "web-sys-webrtc", target_family = "wasm")
    ))]
    pub(crate) fn phase(&self) -> AdmissionPhase {
        self.state.phase()
    }

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
            credit: None,
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
            credit: None,
        }
    }

    /// Construct an unconditional permit for direct low-level transport users.
    pub fn always() -> Self {
        Self::new(|| true)
    }

    /// Carry `credit`, reserved ahead of the send, into it: a custom frame is then sent under this
    /// credit instead of waiting for one inside the send.
    pub fn with_credit(mut self, credit: LaneCreditReservation) -> Self {
        self.credit = Some(credit);
        self
    }

    /// The credit carried by [`Self::with_credit`], taken by the backend that sends under it.
    pub(crate) fn take_credit(&mut self) -> Option<LaneCreditReservation> {
        self.credit.take()
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

    /// Consume the proof, publish successful backend queue admission, and return the linear
    /// [`AcceptedSendPermit`] witness required by
    /// [`crate::callback::InnerTransportCallback::send_custom_wire`].
    pub fn accept(self) -> AcceptedSendPermit {
        self.mark_accepted();
        AcceptedSendPermit(())
    }
}

impl<F: FnOnce()> IrrevocableSendGuard<F> {
    /// Construct an [`IrrevocableSendGuard`] that invokes `retire` on drop if `acceptance` crossed
    /// the irrevocable boundary without reaching acceptance.
    pub fn new(acceptance: SendAcceptance, retire: F) -> Self {
        Self {
            acceptance,
            permit: None,
            retire: Some(retire),
        }
    }

    /// Bind `permit` into this guard after claiming [`SendPermit::try_mark_irrevocable`].
    pub fn bind(&mut self, permit: IrrevocableSendPermit) {
        self.permit = Some(permit);
    }

    /// Consume the bound permit, mark backend acceptance, and disarm retirement.
    pub fn mark_accepted(mut self) {
        if let Some(permit) = self.permit.take() {
            permit.mark_accepted();
        }
        self.retire = None;
    }
}

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
