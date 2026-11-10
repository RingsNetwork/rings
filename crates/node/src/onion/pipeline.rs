//! Onion Kleisli pipelines: typed morphisms of the free category `Free(G_Σ)` over the static
//! signature `Σ` (#834 D3, D4a, D4b, L1, L2, L3).
//!
//! ```text
//! θ : X ↝ Y    ϑ : Y ↝ Z
//! ────────────────────────
//!    θ ⋙ ϑ : X ↝ Z
//! ```
//!
//! Laws:
//!
//! - **Well-formedness** (D4a). A normal-form pipeline `P = (f₁, ā₁) ⋙ … ⋙ (fₙ, āₙ)` satisfies
//!   `1 ≤ n ≤ MAX_ONION_LOOP_SYMBOLS = 4`, `∀ k < n: Out_{fₖ} = In_{fₖ₊₁}`,
//!   `∀ k < n: pos(fₖ) = Intermediate`, and `fₙ ∈ Σ_W ⇒ n = 1`. Every normal-form pipeline is
//!   therefore either one world-facing session symbol (`Session`) or `1 ..= 4` composable `invoke`
//!   stages (`Invoke`). Empty pipelines, mismatched widths (`Out ≠ In`), world-facing symbols
//!   before the end, `invoke ⋙ world-facing` compositions, and `> 4` symbols are rejected at
//!   construction.
//! - **Identity** (L1). `relay : X ↝ X` is the identity morphism; normal form `nf` removes every
//!   `relay` step, so `nf(relay ⋙ P) = nf(P) = nf(P ⋙ relay)` and `LoopOf(relay ⋙ P) =_d LoopOf(P)`.
//! - **Associativity** (L2). `(f ⋙ g) ⋙ h = f ⋙ (g ⋙ h)` as normal-form pipelines and on carried
//!   values.
//! - **Sealed backend arguments** (D3). An `invoke` application encodes `(ω, ā′)` inside the
//!   fixed `A = 64` argument bytes of its hop's layer; the public symbol word `σ` exposes only
//!   `OnionInvokeShape`s, never backend names `ω` or constants `ā′`.
//! - **Width** (L3). For an `OnionInvokePipeline`, `loop_class` selects the least `OnionLoopClass`
//!   `b` with `|enc(v₀)| < C₀` and `∀ k: W(fₖ) < C₀`.

use std::time::Duration;

use super::signature::OnionBackendName;
use super::signature::OnionInputType;
use super::signature::OnionInvokeShape;
use super::signature::OnionServiceName;
use super::signature::OnionShape;
use super::signature::OnionWidthClass;
use super::signature::ONION_BACKEND_NAME_MAX_BYTES;
use super::sphinx::class::OnionLoopClass;
use super::sphinx::layer::OnionApplication;
use super::sphinx::layer::OnionArguments;
use super::sphinx::layer::ONION_ARGUMENT_BYTES;
use super::MAX_ONION_LOOP_SYMBOLS;
use crate::error::Error;
use crate::error::Result;

/// Two-byte header `|ω|:u8 ‖ |ā′|:u8` of [`OnionInvokeArguments`].
const ONION_INVOKE_ARG_HEADER_BYTES: usize = 2;

/// Maximum inline constant bytes `|ā′|` when `|ω| = 1`: `64 − 2 − 1 = 61` bytes.
pub const ONION_INVOKE_MAX_CONSTANTS_BYTES: usize =
    ONION_ARGUMENT_BYTES - ONION_INVOKE_ARG_HEADER_BYTES - 1;

// Every fixed width class in `𝒲` fits inside `KiB64`'s padded carry slot (#834 L3).
const _: () = assert!(OnionWidthClass::KiB16.bytes() <= OnionLoopClass::KiB64.value_capacity());
// Even a 32-byte backend name leaves 30 bytes of inline constants inside `A = 64` (#834 D3).
const _: () =
    assert!(ONION_INVOKE_ARG_HEADER_BYTES + ONION_BACKEND_NAME_MAX_BYTES < ONION_ARGUMENT_BYTES);

/// Carried value types (vertices of `G_Σ`, #834 D1′, D4a).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OnionValueType {
    /// `()`, the empty value `ε` (`0` bytes, #834 D7).
    Unit,
    /// A fixed-width byte string of `W` bytes (#834 D1′, L3).
    Fixed(OnionWidthClass),
    /// A session stream frame (`tcp`, `https`, #834 D2′).
    Frame,
}

impl From<OnionInputType> for OnionValueType {
    fn from(input: OnionInputType) -> Self {
        match input {
            OnionInputType::Unit => Self::Unit,
            OnionInputType::Fixed(width) => Self::Fixed(width),
        }
    }
}

impl From<OnionWidthClass> for OnionValueType {
    fn from(width: OnionWidthClass) -> Self {
        Self::Fixed(width)
    }
}

/// Sealed layer arguments `ā = (ω, ā′)` of an `invoke` hop (#834 D3), encoded canonically in
/// `A = 64` bytes as `|ω|:u8 ‖ |ā′|:u8 ‖ ω ‖ ā′ ‖ 0^{62 − |ω| − |ā′|}`.
///
/// Law (canonical): `decode(encode(args)) = Some(args)`, and `decode(w) = Some(args)` implies
/// `encode(args) = w` (non-zero trailing padding, invalid `ω`, or out-of-range lengths return
/// `None`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionInvokeArguments {
    backend: OnionBackendName,
    constants: Vec<u8>,
}

impl OnionInvokeArguments {
    /// Validate `|enc(ω, ā′)| ≤ A = 64` and construct sealed `invoke` arguments.
    pub fn new(
        backend: OnionBackendName,
        constants: impl Into<Vec<u8>>,
    ) -> std::result::Result<Self, OnionPipelineError> {
        let constants = constants.into();
        let used = ONION_INVOKE_ARG_HEADER_BYTES
            .saturating_add(backend.as_str().len())
            .saturating_add(constants.len());
        if used > ONION_ARGUMENT_BYTES {
            return Err(OnionPipelineError::ArgumentsTooWide {
                backend_bytes: backend.as_str().len(),
                constants_bytes: constants.len(),
                capacity: ONION_ARGUMENT_BYTES - ONION_INVOKE_ARG_HEADER_BYTES,
            });
        }
        Ok(Self { backend, constants })
    }

    /// Return the backend name `ω`.
    pub const fn backend(&self) -> &OnionBackendName {
        &self.backend
    }

    /// Return the inline client constants `ā′`.
    pub fn constants(&self) -> &[u8] {
        self.constants.as_slice()
    }

    /// Destructure into `(backend, constants)`.
    pub fn into_parts(self) -> (OnionBackendName, Vec<u8>) {
        (self.backend, self.constants)
    }

    /// Encode `(ω, ā′)` into the fixed `A = 64`-byte layer argument slot.
    pub(crate) fn encode(&self) -> OnionArguments {
        let mut bytes = [0_u8; ONION_ARGUMENT_BYTES];
        let name = self.backend.as_str().as_bytes();
        let constants = self.constants.as_slice();
        bytes[0] = name.len() as u8;
        bytes[1] = constants.len() as u8;
        bytes
            .iter_mut()
            .skip(ONION_INVOKE_ARG_HEADER_BYTES)
            .zip(name.iter().chain(constants.iter()))
            .for_each(|(slot, byte)| *slot = *byte);
        OnionArguments::new(bytes)
    }

    /// Decode `(ω, ā′)` from `arguments`, rejecting any non-canonical 64-byte string.
    pub(crate) fn decode(arguments: &OnionArguments) -> Option<Self> {
        let bytes = arguments.as_bytes();
        let name_len = usize::from(bytes[0]);
        let const_len = usize::from(bytes[1]);
        if name_len == 0 || name_len > ONION_BACKEND_NAME_MAX_BYTES {
            return None;
        }
        let name_end = ONION_INVOKE_ARG_HEADER_BYTES.checked_add(name_len)?;
        let end = name_end.checked_add(const_len)?;
        if bytes.get(end..)?.iter().any(|byte| *byte != 0) {
            return None;
        }
        let name_str =
            std::str::from_utf8(bytes.get(ONION_INVOKE_ARG_HEADER_BYTES..name_end)?).ok()?;
        let backend = OnionBackendName::parse(name_str).ok()?;
        let constants = bytes.get(name_end..end)?.to_vec();
        Some(Self { backend, constants })
    }
}

/// Errors when constructing or composing an onion Kleisli pipeline (#834 D3, D4a).
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OnionPipelineError {
    /// Empty normal-form pipeline (`n = 0`, e.g., `relay` alone with no symbol application).
    #[error("onion pipeline must apply at least one non-relay symbol")]
    Empty,
    /// More than `MAX_ONION_LOOP_SYMBOLS` non-relay symbol applications.
    #[error("onion pipeline has {symbols} symbols; maximum is {max_symbols}")]
    TooManySymbols {
        /// Requested non-relay symbol count.
        symbols: usize,
        /// Protocol bound `MAX_ONION_LOOP_SYMBOLS`.
        max_symbols: usize,
    },
    /// Sequential composition `θ ⋙ ϑ` where `Out_θ ≠ In_ϑ` (#834 D4a).
    #[error("onion pipeline type mismatch: {left_out:?} cannot feed {right_in:?}")]
    TypeMismatch {
        /// Output type of the left morphism.
        left_out: OnionValueType,
        /// Input type of the right morphism.
        right_in: OnionValueType,
    },
    /// A world-facing symbol appeared before the end of the pipeline (#834 D4a).
    #[error("world-facing onion symbol must be terminal")]
    WorldFacingNotTerminal,
    /// A world-facing session symbol was composed after an intermediate symbol (#834 D4a, Phase 2b).
    #[error("world-facing session symbol must stand alone (n = 1)")]
    SessionSymbolMustStandAlone,
    /// Inline `(ω, ā′)` exceeds the 64-byte layer argument field (#834 D3).
    #[error(
        "onion invoke arguments ({backend_bytes}B backend + {constants_bytes}B constants) exceed {capacity}B inline capacity"
    )]
    ArgumentsTooWide {
        /// Backend name length in bytes.
        backend_bytes: usize,
        /// Inline constant length in bytes.
        constants_bytes: usize,
        /// Combined payload byte capacity (`62` bytes).
        capacity: usize,
    },
}

impl From<OnionPipelineError> for Error {
    fn from(error: OnionPipelineError) -> Self {
        Self::InvalidConfig(error.to_string())
    }
}

/// One placed-to-be `invoke` stage `(f, (ω, ā′))` of a Kleisli pipeline (#834 D3, D4a).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionInvokeStage {
    shape: OnionInvokeShape,
    arguments: OnionInvokeArguments,
}

impl OnionInvokeStage {
    /// Construct one `invoke` application `(shape, (backend, constants))`, validating `|enc(ω, ā′)| ≤ A`.
    pub fn new(
        shape: OnionInvokeShape,
        backend: OnionBackendName,
        constants: impl Into<Vec<u8>>,
    ) -> std::result::Result<Self, OnionPipelineError> {
        Ok(Self {
            shape,
            arguments: OnionInvokeArguments::new(backend, constants)?,
        })
    }

    /// Return the shape `f ∈ Σ_I`.
    pub const fn shape(&self) -> OnionInvokeShape {
        self.shape
    }

    /// Return the backend name `ω ∈ Ω`.
    pub const fn backend(&self) -> &OnionBackendName {
        self.arguments.backend()
    }

    /// Return the inline constants `ā′`.
    pub fn constants(&self) -> &[u8] {
        self.arguments.constants()
    }

    /// Lift this single stage into a 1-stage [`OnionInvokePipeline`].
    pub fn into_pipeline(self) -> OnionInvokePipeline {
        OnionInvokePipeline { stages: vec![self] }
    }

    /// Kleisli-compose `self ⋙ next` into a 2-stage [`OnionInvokePipeline`] (#834 D4a).
    pub fn then(
        self,
        next: OnionInvokeStage,
    ) -> std::result::Result<OnionInvokePipeline, OnionPipelineError> {
        self.into_pipeline().then_stage(next)
    }

    pub(crate) fn to_application(&self) -> OnionApplication {
        OnionApplication {
            symbol: OnionShape::Invoke(self.shape),
            arguments: self.arguments.encode(),
        }
    }
}

/// Well-formed one-shot Kleisli pipeline `(f₁, ā₁) ⋙ … ⋙ (fₙ, āₙ)` over `Σ_I` (#834 D4a, D4b).
///
/// Invariants:
/// - `1 ≤ stages.len() ≤ MAX_ONION_LOOP_SYMBOLS`;
/// - `∀ k < n: OnionValueType::Fixed(stages[k - 1].shape.output()) == OnionValueType::from(stages[k].shape.input())`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionInvokePipeline {
    stages: Vec<OnionInvokeStage>,
}

impl OnionInvokePipeline {
    /// Build a pipeline from a non-empty sequence of stages, verifying `Out_{fₖ} = In_{fₖ₊₁}` and
    /// `n ≤ MAX_ONION_LOOP_SYMBOLS`.
    pub fn from_stages(
        stages: impl IntoIterator<Item = OnionInvokeStage>,
    ) -> std::result::Result<Self, OnionPipelineError> {
        let mut iter = stages.into_iter();
        let first = iter.next().ok_or(OnionPipelineError::Empty)?;
        iter.try_fold(first.into_pipeline(), Self::then_stage)
    }

    /// Append one `invoke` stage (`self ⋙ next`).
    pub fn then_stage(
        mut self,
        next: OnionInvokeStage,
    ) -> std::result::Result<Self, OnionPipelineError> {
        let left_out = OnionValueType::Fixed(self.output());
        let right_in = OnionValueType::from(next.shape.input());
        if left_out != right_in {
            return Err(OnionPipelineError::TypeMismatch { left_out, right_in });
        }
        let symbols = self.stages.len() + 1;
        if symbols > MAX_ONION_LOOP_SYMBOLS {
            return Err(OnionPipelineError::TooManySymbols {
                symbols,
                max_symbols: MAX_ONION_LOOP_SYMBOLS,
            });
        }
        self.stages.push(next);
        Ok(self)
    }

    /// Kleisli-compose two pipelines (`self ⋙ other`), checking boundary types and total length.
    pub fn then(self, other: Self) -> std::result::Result<Self, OnionPipelineError> {
        other.stages.into_iter().try_fold(self, Self::then_stage)
    }

    /// Return the non-empty slice of stages in pipeline order.
    pub fn stages(&self) -> &[OnionInvokeStage] {
        self.stages.as_slice()
    }

    /// Return `n ∈ [1, MAX_ONION_LOOP_SYMBOLS]`, the number of `invoke` stages.
    pub fn len(&self) -> usize {
        self.stages.len()
    }

    /// Always `false`: a constructed [`OnionInvokePipeline`] has `n ≥ 1` stages.
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    /// Input specification `In_{f₁}` consumed from the client.
    pub fn input(&self) -> OnionInputType {
        self.stages
            .first()
            .map_or(OnionInputType::Unit, |stage| stage.shape.input())
    }

    /// Exact byte length of the initial carried input `v₀` required from the client.
    pub fn input_bytes(&self) -> usize {
        self.input().bytes()
    }

    /// Output width class `Out_{fₙ}` returned to the client.
    pub fn output(&self) -> OnionWidthClass {
        self.stages
            .last()
            .map_or(OnionWidthClass::B32, |stage| stage.shape.output())
    }

    /// Exact byte length `W(fₙ)` of the result returned to the client.
    pub fn output_bytes(&self) -> usize {
        self.output().bytes()
    }

    /// Sum `Σ_{k=1..n} L(fₖ)` of per-hop computation delays along the pipeline (L4).
    pub fn total_latency(&self) -> Duration {
        self.stages.iter().fold(Duration::ZERO, |acc, stage| {
            acc.saturating_add(stage.shape.latency().duration())
        })
    }

    /// Public symbol word `σ` of shapes only, omitting backend names `ω` and constants `ā′` (#834 D3).
    pub fn symbol_word(&self) -> Vec<OnionInvokeShape> {
        self.stages.iter().map(OnionInvokeStage::shape).collect()
    }

    /// The `(shape, backend)` registration pairs required along the pipeline, in stage order (D2).
    pub fn required_symbols(&self) -> Vec<(OnionInvokeShape, OnionBackendName)> {
        self.stages
            .iter()
            .map(|stage| (stage.shape, stage.backend().clone()))
            .collect()
    }

    /// Least loop class `b` satisfying `|v₀| < C₀` and `∀ k: W(fₖ) < C₀` (#834 L3).
    pub fn loop_class(&self) -> OnionLoopClass {
        let widest = self.stages.iter().fold(self.input_bytes(), |acc, stage| {
            acc.max(stage.shape.output_bytes())
        });
        OnionLoopClass::least_for_value_bytes(widest).unwrap_or(OnionLoopClass::MiB12)
    }

    pub(crate) fn applications(&self) -> Vec<OnionApplication> {
        self.stages
            .iter()
            .map(OnionInvokeStage::to_application)
            .collect()
    }
}

impl From<OnionInvokeStage> for OnionInvokePipeline {
    fn from(stage: OnionInvokeStage) -> Self {
        stage.into_pipeline()
    }
}

/// Morphism in `Free(G_Σ)` before normal-form `relay` elimination (#834 D4a, L1, L2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionPipelineTerm {
    input: OnionValueType,
    output: OnionValueType,
    non_relay: Vec<TermAtom>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TermAtom {
    Invoke(OnionInvokeStage),
    WorldFacing(OnionServiceName),
}

impl OnionPipelineTerm {
    /// Identity morphism `relay : ty ↝ ty` (`L(relay) = 0`, #834 L1).
    pub const fn relay(ty: OnionValueType) -> Self {
        Self {
            input: ty,
            output: ty,
            non_relay: Vec::new(),
        }
    }

    /// Atomic `invoke` morphism `In_f ↝ Out_f`.
    pub fn invoke(stage: OnionInvokeStage) -> Self {
        let input = OnionValueType::from(stage.shape.input());
        let output = OnionValueType::Fixed(stage.shape.output());
        Self {
            input,
            output,
            non_relay: vec![TermAtom::Invoke(stage)],
        }
    }

    /// Atomic world-facing session morphism `Frame ↝ Frame`.
    pub fn world_facing(service: OnionServiceName) -> Self {
        Self {
            input: OnionValueType::Frame,
            output: OnionValueType::Frame,
            non_relay: vec![TermAtom::WorldFacing(service)],
        }
    }

    /// Return the domain `In` of this morphism.
    pub const fn input_type(&self) -> OnionValueType {
        self.input
    }

    /// Return the codomain `Out` of this morphism.
    pub const fn output_type(&self) -> OnionValueType {
        self.output
    }

    /// Sequential composition `self ⋙ next` in `Free(G_Σ)` (#834 D4a, L1, L2).
    pub fn then(mut self, next: Self) -> std::result::Result<Self, OnionPipelineError> {
        if self.output != next.input {
            return Err(OnionPipelineError::TypeMismatch {
                left_out: self.output,
                right_in: next.input,
            });
        }
        if !next.non_relay.is_empty()
            && self
                .non_relay
                .iter()
                .any(|atom| matches!(atom, TermAtom::WorldFacing(_)))
        {
            return Err(OnionPipelineError::WorldFacingNotTerminal);
        }
        self.non_relay.extend(next.non_relay);
        if self.non_relay.len() > 1
            && matches!(self.non_relay.last(), Some(TermAtom::WorldFacing(_)))
        {
            return Err(OnionPipelineError::SessionSymbolMustStandAlone);
        }
        if self.non_relay.len() > MAX_ONION_LOOP_SYMBOLS {
            return Err(OnionPipelineError::TooManySymbols {
                symbols: self.non_relay.len(),
                max_symbols: MAX_ONION_LOOP_SYMBOLS,
            });
        }
        self.output = next.output;
        Ok(self)
    }

    /// Normalize (`nf`, deleting `relay` identities) into a well-formed [`OnionPipeline`],
    /// rejecting an empty normal form (`relay` alone).
    pub fn into_pipeline(self) -> std::result::Result<OnionPipeline, OnionPipelineError> {
        if self.non_relay.is_empty() {
            return Err(OnionPipelineError::Empty);
        }
        if self.non_relay.len() == 1 {
            if let Some(TermAtom::WorldFacing(service)) = self.non_relay.first() {
                return Ok(OnionPipeline::Session(service.clone()));
            }
        }
        let mut stages = Vec::with_capacity(self.non_relay.len());
        for (index, atom) in self.non_relay.into_iter().enumerate() {
            match atom {
                TermAtom::Invoke(stage) => stages.push(stage),
                TermAtom::WorldFacing(_) => {
                    return Err(if index + 1 == stages.capacity() {
                        OnionPipelineError::SessionSymbolMustStandAlone
                    } else {
                        OnionPipelineError::WorldFacingNotTerminal
                    });
                }
            }
        }
        OnionInvokePipeline::from_stages(stages).map(OnionPipeline::Invoke)
    }
}

/// Normal-form well-formed pipeline `nf(P)` (#834 D4a, L1): either one world-facing session
/// symbol (`n = 1`) or a 1..=4 stage `invoke` Kleisli pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OnionPipeline {
    /// One-shot intermediate computation pipeline `(f₁, ā₁) ⋙ … ⋙ (fₙ, āₙ)`.
    Invoke(OnionInvokePipeline),
    /// Single world-facing session symbol (`tcp`, `https`).
    Session(OnionServiceName),
}

impl OnionPipeline {
    /// Public symbol word `σ` of shapes only (#834 D3).
    pub fn symbol_word(&self) -> Vec<OnionShape> {
        match self {
            Self::Invoke(pipeline) => pipeline
                .symbol_word()
                .into_iter()
                .map(OnionShape::Invoke)
                .collect(),
            Self::Session(service) => vec![OnionShape::WorldFacing(service.clone())],
        }
    }
}

/// Validate `route` and `pipeline` compatibility helper returning crate [`Result`].
pub(crate) fn ensure_pipeline_input_width(
    pipeline: &OnionInvokePipeline,
    input: &[u8],
) -> Result<()> {
    let expected = pipeline.input_bytes();
    if input.len() != expected {
        return Err(Error::InvalidConfig(format!(
            "onion pipeline expects {expected}-byte input, got {} bytes",
            input.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;
    use rand_chacha::ChaCha20Rng;
    use rings_core::dht::Did;

    use super::OnionInvokeArguments;
    use super::OnionInvokePipeline;
    use super::OnionInvokeStage;
    use super::OnionPipeline;
    use super::OnionPipelineError;
    use super::OnionPipelineTerm;
    use super::OnionValueType;
    use crate::onion::signature::OnionBackendName;
    use crate::onion::signature::OnionInputType;
    use crate::onion::signature::OnionInvokeShape;
    use crate::onion::signature::OnionLatencyClass;
    use crate::onion::signature::OnionServiceName;
    use crate::onion::signature::OnionShape;
    use crate::onion::signature::OnionWidthClass;
    use crate::onion::sphinx::builder::build_loop;
    use crate::onion::sphinx::class::OnionLoopClass;
    use crate::onion::sphinx::layer::OnionArguments;
    use crate::onion::sphinx::layer::ONION_ARGUMENT_BYTES;
    use crate::onion::tests::fixture::fixture_secret;
    use crate::onion::tests::fixture::grid_expiry;
    use crate::onion::tests::fixture::session_loop;
    use crate::onion::OnionProcessEpoch;
    use crate::onion::OnionRouteHop;

    fn stage(
        input: OnionInputType,
        output: OnionWidthClass,
        latency: OnionLatencyClass,
        backend: &str,
        constants: &[u8],
    ) -> OnionInvokeStage {
        OnionInvokeStage::new(
            OnionInvokeShape::new(input, output, latency),
            OnionBackendName::parse(backend).expect("backend"),
            constants,
        )
        .expect("valid stage")
    }

    /// D3: `(ω, ā′)` round-trips canonically through `A = 64` bytes, rejects non-zero trailing
    /// padding or invalid backend names, and `symbol_word` never exposes `ω`.
    #[test]
    fn test_invoke_arguments_are_canonical_and_hidden_from_symbol_word() {
        let backend = OnionBackendName::parse("jev.typesafe").expect("backend");
        let constants = vec![0x00, 0x11, 0x22, 0x00];
        let args = OnionInvokeArguments::new(backend.clone(), constants.clone()).expect("fits");
        let encoded = args.encode();
        let decoded = OnionInvokeArguments::decode(&encoded).expect("decodes");
        assert_eq!(decoded, args);
        assert_eq!(decoded.encode(), encoded);

        // Non-zero trailing padding is rejected so every application has one canonical encoding.
        let mut non_canonical = *encoded.as_bytes();
        non_canonical[ONION_ARGUMENT_BYTES - 1] = 1;
        assert!(OnionInvokeArguments::decode(&OnionArguments::new(non_canonical)).is_none());

        // Oversized inline constants fail closed at construction.
        assert!(matches!(
            OnionInvokeArguments::new(backend, vec![0xaa; 60]),
            Err(OnionPipelineError::ArgumentsTooWide { .. })
        ));

        // Public symbol word `σ` exposes shapes only, never `ω`.
        let p = stage(
            OnionInputType::Fixed(OnionWidthClass::B32),
            OnionWidthClass::B64,
            OnionLatencyClass::Ms500,
            "jev.typesafe",
            b"secret-const",
        )
        .into_pipeline();
        let word = OnionPipeline::Invoke(p).symbol_word();
        assert_eq!(word.len(), 1);
        assert!(matches!(word[0], OnionShape::Invoke(_)));
        assert!(!word[0].to_string().contains("jev"));
    }

    /// D4a: `Out ≠ In`, `WorldFacing` before the last symbol, `invoke ⋙ WorldFacing`, empty
    /// pipeline, and `> 4` symbols are all rejected at construction.
    #[test]
    fn test_pipeline_type_and_position_checking() {
        // Empty normal form (`relay` alone) rejected.
        let only_relay = OnionPipelineTerm::relay(OnionValueType::Fixed(OnionWidthClass::B32))
            .then(OnionPipelineTerm::relay(OnionValueType::Fixed(
                OnionWidthClass::B32,
            )))
            .expect("relay ⋙ relay composes as term");
        assert_eq!(only_relay.into_pipeline(), Err(OnionPipelineError::Empty));
        assert_eq!(
            OnionInvokePipeline::from_stages(Vec::new()),
            Err(OnionPipelineError::Empty)
        );

        // Out ≠ In rejected.
        let f = stage(
            OnionInputType::Fixed(OnionWidthClass::B32),
            OnionWidthClass::B64,
            OnionLatencyClass::Ms10,
            "add",
            b"1",
        );
        let g_bad = stage(
            OnionInputType::Fixed(OnionWidthClass::B32),
            OnionWidthClass::B32,
            OnionLatencyClass::Ms10,
            "mul",
            b"2",
        );
        assert_eq!(
            f.clone().then(g_bad),
            Err(OnionPipelineError::TypeMismatch {
                left_out: OnionValueType::Fixed(OnionWidthClass::B64),
                right_in: OnionValueType::Fixed(OnionWidthClass::B32),
            })
        );

        // WorldFacing before last symbol rejected.
        let tcp = OnionPipelineTerm::world_facing(OnionServiceName::tcp());
        let https = OnionPipelineTerm::world_facing(OnionServiceName::https());
        assert_eq!(
            tcp.then(https),
            Err(OnionPipelineError::WorldFacingNotTerminal)
        );

        // > 4 symbols rejected.
        let id_stage = || {
            stage(
                OnionInputType::Fixed(OnionWidthClass::B32),
                OnionWidthClass::B32,
                OnionLatencyClass::Ms10,
                "step",
                b"",
            )
        };
        let four = OnionInvokePipeline::from_stages((0..4).map(|_| id_stage())).expect("4 fits");
        assert_eq!(
            four.then_stage(id_stage()),
            Err(OnionPipelineError::TooManySymbols {
                symbols: 5,
                max_symbols: 4,
            })
        );
    }

    /// L1 & L2: `nf(relay ⋙ P) = P = nf(P ⋙ relay)`, `(f ⋙ g) ⋙ h = f ⋙ (g ⋙ h)`, and
    /// `LoopOf(relay ⋙ P)` produces byte-identical loop cells to `LoopOf(P)` under the same seed.
    #[test]
    fn test_identity_normal_form_and_associativity_laws() {
        let f = stage(
            OnionInputType::Unit,
            OnionWidthClass::B32,
            OnionLatencyClass::Ms10,
            "seed",
            b"a",
        );
        let g = stage(
            OnionInputType::Fixed(OnionWidthClass::B32),
            OnionWidthClass::B64,
            OnionLatencyClass::Ms100,
            "expand",
            b"b",
        );
        let h = stage(
            OnionInputType::Fixed(OnionWidthClass::B64),
            OnionWidthClass::B256,
            OnionLatencyClass::Ms500,
            "finalize",
            b"c",
        );

        // L2: (f ⋙ g) ⋙ h == f ⋙ (g ⋙ h)
        let left = f
            .clone()
            .then(g.clone())
            .expect("f ⋙ g")
            .then_stage(h.clone())
            .expect("(f ⋙ g) ⋙ h");
        let right = f
            .clone()
            .into_pipeline()
            .then(g.clone().then(h).expect("g ⋙ h"))
            .expect("f ⋙ (g ⋙ h)");
        assert_eq!(left, right);

        // L1: relay ⋙ f ⋙ relay normalizes to f, and builds identical loop bytes.
        let plain = f.clone().into_pipeline();
        let wrapped_term = OnionPipelineTerm::relay(OnionValueType::Unit)
            .then(OnionPipelineTerm::invoke(f))
            .expect("relay ⋙ f")
            .then(OnionPipelineTerm::relay(OnionValueType::Fixed(
                OnionWidthClass::B32,
            )))
            .expect("(relay ⋙ f) ⋙ relay");
        let OnionPipeline::Invoke(normalized) = wrapped_term.into_pipeline().expect("nf") else {
            panic!("invoke pipeline");
        };
        assert_eq!(normalized, plain);

        let hops = session_loop([1_u8, 2, 3, 4].map(|seed| {
            OnionRouteHop::new(
                Did::from(u32::from(seed)),
                fixture_secret(seed).pubkey(),
                OnionProcessEpoch::new([0x31; 16]),
            )
        }));
        let built_plain = build_loop(
            &hops,
            &plain.applications(),
            Did::from(99_u32),
            plain.loop_class(),
            grid_expiry(5),
            b"",
            &mut ChaCha20Rng::from_seed([0x77; 32]),
        )
        .expect("build plain");
        let built_normalized = build_loop(
            &hops,
            &normalized.applications(),
            Did::from(99_u32),
            normalized.loop_class(),
            grid_expiry(5),
            b"",
            &mut ChaCha20Rng::from_seed([0x77; 32]),
        )
        .expect("build normalized");
        assert_eq!(
            built_plain.cell.into_bytes(),
            built_normalized.cell.into_bytes()
        );
        assert_eq!(built_plain.reply.tag, built_normalized.reply.tag);
    }

    /// L3: `loop_class` selects `KiB16` while widths stay below `C₀(KiB16) = 13,449` and steps up
    /// to `KiB64` once any stage or input uses `OnionWidthClass::KiB16` (`16,384` bytes).
    #[test]
    fn test_loop_class_selects_least_admissible_bucket() {
        let small = stage(
            OnionInputType::Fixed(OnionWidthClass::B32),
            OnionWidthClass::KiB1,
            OnionLatencyClass::Ms10,
            "small",
            b"",
        )
        .into_pipeline();
        assert_eq!(small.loop_class(), OnionLoopClass::KiB16);

        let wide = small
            .then_stage(stage(
                OnionInputType::Fixed(OnionWidthClass::KiB1),
                OnionWidthClass::KiB16,
                OnionLatencyClass::Ms100,
                "wide",
                b"",
            ))
            .expect("compose wide");
        assert_eq!(wide.loop_class(), OnionLoopClass::KiB64);
    }
}
