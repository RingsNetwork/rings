#![warn(missing_docs)]
//! Integration with the existing extension protocol registry.

use bytes::Bytes;

use super::runtime::accept_step_output;
use super::GuestContext;
use super::GuestEffect;
use super::GuestError;
use super::GuestEvent;
use super::GuestManifest;
use super::GuestPublicInput;
use super::GuestReceiptVerifier;
use super::GuestRuntime;
use super::GuestState;
use super::GuestStepInput;
use crate::error::Result;
use crate::extension::ext::Ctx;
use crate::extension::ext::Extensions;
use crate::extension::ext::Interpret;
use crate::extension::ext::MaybeSend;
use crate::extension::ext::Protocol;
use crate::extension::ext::Reject;
use crate::extension::ext::Scope;
use crate::extension::ext::Transition;
use crate::extension::ext::Wire;

/// Guest protocol state stored by the extension registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestProtocolState {
    guest_state: GuestState,
    last_error: Option<GuestError>,
}

impl GuestProtocolState {
    /// Build a protocol state from an initial guest state.
    pub fn new(guest_state: GuestState) -> Self {
        Self {
            guest_state,
            last_error: None,
        }
    }

    /// Current guest state.
    pub fn guest_state(&self) -> &GuestState {
        &self.guest_state
    }

    /// Last runtime or output-validation error, when the previous transition failed.
    pub fn last_error(&self) -> Option<&GuestError> {
        self.last_error.as_ref()
    }

    fn accepted(&self, guest_state: GuestState) -> Self {
        Self {
            guest_state,
            last_error: None,
        }
    }

    fn rejected(&self, error: GuestError) -> Self {
        Self {
            guest_state: self.guest_state.clone(),
            last_error: Some(error),
        }
    }
}

/// Guest protocol adapter.
pub struct GuestProtocol<R, V> {
    manifest: GuestManifest,
    runtime: R,
    verifier: V,
    initial_state: GuestState,
}

impl<R, V> GuestProtocol<R, V> {
    /// Build a guest protocol from a validated manifest, runtime and receipt verifier.
    pub fn new(
        manifest: GuestManifest,
        runtime: R,
        verifier: V,
        initial_state: GuestState,
    ) -> Self {
        Self {
            manifest,
            runtime,
            verifier,
            initial_state,
        }
    }

    /// Validated manifest for this protocol.
    pub fn manifest(&self) -> &GuestManifest {
        &self.manifest
    }
}

impl<R, V> Protocol for GuestProtocol<R, V>
where
    R: GuestRuntime,
    V: GuestReceiptVerifier,
{
    type State = GuestProtocolState;
    type Event = GuestEvent;
    type Effect = GuestEffect;

    fn namespace(&self) -> &str {
        self.manifest.namespace()
    }

    fn init(&self) -> GuestProtocolState {
        GuestProtocolState::new(self.initial_state.clone())
    }

    fn decode(&self, wire: Wire<'_>) -> std::result::Result<GuestEvent, Reject> {
        Ok(GuestEvent {
            from: wire.from,
            payload: Bytes::copy_from_slice(wire.payload),
        })
    }

    fn step(
        &self,
        ctx: Ctx<'_, GuestProtocolState>,
        event: GuestEvent,
    ) -> Transition<GuestProtocolState, GuestEffect> {
        let input = GuestStepInput {
            state: ctx.state.guest_state().clone(),
            public_input: GuestPublicInput::new(event.payload.clone()),
            event,
            context: GuestContext::from_manifest(&self.manifest, ctx.did),
        };
        let output = match self.runtime.step(input.clone()) {
            Ok(output) => output,
            Err(error) => return Transition::pure(ctx.state.rejected(error)),
        };
        match accept_step_output(&self.manifest, &input, output, &self.verifier) {
            Ok(accepted) => Transition::with(ctx.state.accepted(accepted.state), accepted.effects),
            Err(error) => Transition::pure(ctx.state.rejected(error)),
        }
    }
}

/// Guest effect interpreter.
#[derive(Clone, Copy, Debug, Default)]
pub struct GuestShell;

#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
impl Interpret for GuestShell {
    type Effect = GuestEffect;

    async fn run(&self, scope: &Scope, effect: GuestEffect) -> Result<Vec<Bytes>> {
        match effect {
            GuestEffect::Send { to, payload } => {
                scope.send(to, payload).await?;
                Ok(Vec::new())
            }
            GuestEffect::Inject { payload } => Ok(vec![payload]),
        }
    }
}

/// Register a guest protocol through the normal extension registry.
pub fn register_guest_extension<R, V>(
    extensions: &Extensions,
    protocol: GuestProtocol<R, V>,
) -> Result<()>
where
    R: GuestRuntime + MaybeSend + 'static,
    V: GuestReceiptVerifier + MaybeSend + 'static,
{
    extensions.register(protocol, GuestShell)
}

#[cfg(test)]
mod tests {
    use rings_core::dht::Did;

    use super::*;
    use crate::extension::guest::GuestCapability;
    use crate::extension::guest::GuestManifestSpec;
    use crate::extension::guest::GuestProgramHash;
    use crate::extension::guest::GuestPublicOutput;
    use crate::extension::guest::GuestRuntimeKind;
    use crate::extension::guest::GuestStepOutput;
    use crate::extension::guest::ProofPolicy;
    use crate::extension::guest::SUPPORTED_GUEST_ABI_VERSION;

    fn hash(seed: u8, field: &'static str) -> GuestProgramHash {
        GuestProgramHash::new([seed; 32], field).expect("non-zero test hash")
    }

    fn manifest(capabilities: Vec<GuestCapability>) -> GuestManifest {
        GuestManifest::validate(GuestManifestSpec {
            namespace: "guest.protocol".to_string(),
            runtime: GuestRuntimeKind::Wasm,
            abi_version: SUPPORTED_GUEST_ABI_VERSION,
            module_hash: hash(1, "module_hash"),
            state_schema_hash: hash(2, "state_schema_hash"),
            event_schema_hash: hash(3, "event_schema_hash"),
            effect_schema_hash: hash(4, "effect_schema_hash"),
            capabilities,
            memory_limit: 16,
            fuel_limit: 100,
            proof_policy: ProofPolicy::None,
        })
        .expect("valid guest manifest")
    }

    struct AllowVerifier;

    impl GuestReceiptVerifier for AllowVerifier {
        fn verify(
            &self,
            _claim: &crate::extension::guest::GuestReceiptClaim,
            _receipt: &crate::extension::guest::GuestReceipt,
        ) -> std::result::Result<(), GuestError> {
            Ok(())
        }
    }

    struct StaticRuntime {
        output: std::result::Result<GuestStepOutput, GuestError>,
    }

    impl GuestRuntime for StaticRuntime {
        fn step(&self, _input: GuestStepInput) -> std::result::Result<GuestStepOutput, GuestError> {
            self.output.clone()
        }
    }

    fn output_with(effects: Vec<GuestEffect>) -> GuestStepOutput {
        GuestStepOutput {
            state: GuestState::new(Bytes::from_static(b"next")),
            effects,
            public_output: GuestPublicOutput::new(Bytes::from_static(b"out")),
            receipt: None,
            fuel_used: 1,
            memory_pages_used: 1,
        }
    }

    #[test]
    fn guest_protocol_uses_manifest_namespace() {
        let protocol = GuestProtocol::new(
            manifest(Vec::new()),
            StaticRuntime {
                output: Ok(output_with(Vec::new())),
            },
            AllowVerifier,
            GuestState::new(Bytes::new()),
        );

        assert_eq!(protocol.namespace(), "guest.protocol");
    }

    #[test]
    fn guest_protocol_records_runtime_error_without_effects() {
        let protocol = GuestProtocol::new(
            manifest(Vec::new()),
            StaticRuntime {
                output: Err(GuestError::RuntimeRejected {
                    reason: "boom".to_string(),
                }),
            },
            AllowVerifier,
            GuestState::new(Bytes::from_static(b"initial")),
        );
        let state = protocol.init();
        let transition = protocol.step(
            Ctx {
                did: Did::from(1u32),
                state: &state,
            },
            GuestEvent {
                from: Did::from(2u32),
                payload: Bytes::from_static(b"event"),
            },
        );

        assert!(transition.effects.is_empty());
        assert_eq!(
            transition.state.last_error(),
            Some(&GuestError::RuntimeRejected {
                reason: "boom".to_string()
            })
        );
        assert_eq!(
            transition.state.guest_state(),
            &GuestState::new(Bytes::from_static(b"initial"))
        );
    }

    #[test]
    fn guest_protocol_denies_undeclared_effect_before_shell_boundary() {
        let protocol = GuestProtocol::new(
            manifest(Vec::new()),
            StaticRuntime {
                output: Ok(output_with(vec![GuestEffect::Send {
                    to: Did::from(3u32),
                    payload: Bytes::from_static(b"send"),
                }])),
            },
            AllowVerifier,
            GuestState::new(Bytes::from_static(b"initial")),
        );
        let state = protocol.init();
        let transition = protocol.step(
            Ctx {
                did: Did::from(1u32),
                state: &state,
            },
            GuestEvent {
                from: Did::from(2u32),
                payload: Bytes::from_static(b"event"),
            },
        );

        assert!(transition.effects.is_empty());
        assert_eq!(
            transition.state.last_error(),
            Some(&GuestError::CapabilityDenied {
                capability: GuestCapability::Send
            })
        );
    }

    #[test]
    fn guest_protocol_accepts_declared_effects_as_host_shell_work() {
        let effects = vec![GuestEffect::Inject {
            payload: Bytes::from_static(b"loop"),
        }];
        let protocol = GuestProtocol::new(
            manifest(vec![GuestCapability::Inject]),
            StaticRuntime {
                output: Ok(output_with(effects.clone())),
            },
            AllowVerifier,
            GuestState::new(Bytes::from_static(b"initial")),
        );
        let state = protocol.init();
        let transition = protocol.step(
            Ctx {
                did: Did::from(1u32),
                state: &state,
            },
            GuestEvent {
                from: Did::from(2u32),
                payload: Bytes::from_static(b"event"),
            },
        );

        assert_eq!(transition.effects, effects);
        assert_eq!(
            transition.state.guest_state(),
            &GuestState::new(Bytes::from_static(b"next"))
        );
        assert_eq!(transition.state.last_error(), None);
    }

    #[cfg(feature = "node")]
    #[tokio::test]
    async fn guest_registration_rejects_duplicate_namespace() {
        use std::sync::Arc;

        use crate::provider::Provider;
        use crate::tests::native::prepare_processor;

        let manifest = manifest(Vec::new());
        let provider = Provider::from_processor(Arc::new(prepare_processor().await));
        let first = GuestProtocol::new(
            manifest.clone(),
            StaticRuntime {
                output: Ok(output_with(Vec::new())),
            },
            AllowVerifier,
            GuestState::new(Bytes::new()),
        );
        let second = GuestProtocol::new(
            manifest,
            StaticRuntime {
                output: Ok(output_with(Vec::new())),
            },
            AllowVerifier,
            GuestState::new(Bytes::new()),
        );

        register_guest_extension(&provider.extensions(), first)
            .expect("first guest registration succeeds");
        assert!(register_guest_extension(&provider.extensions(), second).is_err());
    }
}
