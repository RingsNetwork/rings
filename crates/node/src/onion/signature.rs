//! The onion signature `Σ`: the static table of operation shape symbols a loop applies, and the
//! open backend name alphabet `Ω` (#834 D1′, D2, D3).
//!
//! A loop evaluates a term over `Σ`, one symbol per hop:
//!
//! ```text
//! Σ   = {relay} ⊎ Σ_I ⊎ Σ_W
//! Σ_I = { invoke⟨In, Out, L⟩ | In ∈ {0} ∪ 𝒲, Out ∈ 𝒲, L ∈ ℒ }      pos = Intermediate (one-shot)
//! Σ_W = { tcp, https }                                              pos = WorldFacing  (session)
//! Ω   = { ω ∈ [a-z0-9._-]⁺ | |ω| ≤ 32 }                            open registration/arg data
//! ```
//!
//! Laws:
//!
//! - **Identity** (#834 L1). `relay : X → X` is the identity (`L(relay) = 0`), preserving the
//!   carried width. The pure hop step interprets `relay`; it is registered through the online-node
//!   relay capability, never as a symbol descriptor (#834 D2).
//! - **Position** (#834 D1′, D4a). Every `invoke` shape is `Intermediate` and one-shot, with
//!   protocol-fixed input width `In`, output width `W(f) = Out` and latency class `L(f) = L`.
//!   World-facing symbols (`tcp`, `https`) exchange bytes with the outside world as sessions and
//!   stand alone (`n = 1`).
//! - **Closure** (#834 D1′). Both `Σ_I` ([`OnionInvokeShape`]) and `Σ_W` ([`OnionServiceName`]) are
//!   finite and statically closed: an unknown shape is rejected at configuration, at descriptor
//!   decode, at layer decode and at route construction, with no fallback. Open backend names
//!   ([`OnionBackendName`]) are admitted only paired with an [`OnionInvokeShape`], never as a
//!   world-facing or session symbol.
//! - **One interpretation per world-facing symbol** (#834 D1′). `https` is a request/response
//!   fetch only, and every byte tunnel, TLS included, is `tcp`.
//! - **Code** (#834 D6″). `Σ = 1 + Σ_I + Σ_W` has `|Σ| = 1 + 120 + 2 = 123` rows, and the `f`
//!   byte of the uniform layer is its position in table order: `0` for `relay`, `1 ..= |Σ_I|` for
//!   `Σ_I` in lexicographic `(In, Out, L)` order, and `1 + |Σ_I| ..` for `Σ_W`, with
//!   `from_code ∘ code = Some` and `from_code` undefined past `Σ`.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;

/// Maximum byte length of a canonical backend name `ω ∈ Ω` (#834 D1′, D2).
pub const ONION_BACKEND_NAME_MAX_BYTES: usize = 32;

/// Protocol-fixed discrete output width classes `𝒲` in bytes (#834 D1′, L3).
///
/// Every fixed-width `invoke` result is encoded to `W(f)` bytes before sealing, independent of its
/// runtime value. Including `16 KiB` alongside sub-KiB widths exercises L3's loop-class selection
/// (`W + τ < C_b` requires `KiB64` once `W = 16 KiB` exceeds `KiB16`'s `C₀ = 13,449` bytes).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum OnionWidthClass {
    /// 32 bytes (e.g., a 256-bit scalar or digest).
    B32 = 0,
    /// 64 bytes (e.g., a fixed-width judgement or signature record).
    B64 = 1,
    /// 256 bytes.
    B256 = 2,
    /// 1,024 bytes (1 KiB).
    KiB1 = 3,
    /// 16,384 bytes (16 KiB; requires at least `OnionLoopClass::KiB64` under L3).
    KiB16 = 4,
}

impl OnionWidthClass {
    /// All discrete width classes in table order.
    pub const ALL: [Self; 5] = [Self::B32, Self::B64, Self::B256, Self::KiB1, Self::KiB16];

    /// Exact byte width `W` of values in this class.
    pub const fn bytes(self) -> usize {
        match self {
            Self::B32 => 32,
            Self::B64 => 64,
            Self::B256 => 256,
            Self::KiB1 => 1_024,
            Self::KiB16 => 16_384,
        }
    }

    /// Canonical decimal token of this width class in an `invoke<in,out,latency>` shape name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::B32 => "32",
            Self::B64 => "64",
            Self::B256 => "256",
            Self::KiB1 => "1024",
            Self::KiB16 => "16384",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }

    fn parse_token(token: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|class| class.as_str().eq_ignore_ascii_case(token))
    }
}

/// Carried input specification `In_f` of an `invoke` shape (#834 D1′, D7, L3): either `()` (`0`
/// bytes, encoded as `ε` and authenticated via `pad(ε)`) or a fixed-width byte slice from `𝒲`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OnionInputType {
    /// `()`, the empty input `ε` (`0` bytes).
    Unit,
    /// Fixed-width input of `W` bytes.
    Fixed(OnionWidthClass),
}

impl OnionInputType {
    /// All discrete input sorts in table order (`Unit`, then `OnionWidthClass::ALL`).
    pub const ALL: [Self; 1 + OnionWidthClass::ALL.len()] = [
        Self::Unit,
        Self::Fixed(OnionWidthClass::B32),
        Self::Fixed(OnionWidthClass::B64),
        Self::Fixed(OnionWidthClass::B256),
        Self::Fixed(OnionWidthClass::KiB1),
        Self::Fixed(OnionWidthClass::KiB16),
    ];

    /// Exact unpadded byte length carried into a hop of this input type.
    pub const fn bytes(self) -> usize {
        match self {
            Self::Unit => 0,
            Self::Fixed(width) => width.bytes(),
        }
    }

    /// Canonical token in an `invoke<in,out,latency>` shape name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unit => "0",
            Self::Fixed(width) => width.as_str(),
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Unit => 0,
            Self::Fixed(width) => 1 + width.index(),
        }
    }

    fn parse_token(token: &str) -> Option<Self> {
        if token == "0" {
            return Some(Self::Unit);
        }
        OnionWidthClass::parse_token(token).map(Self::Fixed)
    }
}

impl From<OnionWidthClass> for OnionInputType {
    fn from(width: OnionWidthClass) -> Self {
        Self::Fixed(width)
    }
}

/// Protocol-fixed discrete latency classes `ℒ` (#834 D1′, L4).
///
/// A hop evaluating an `invoke` shape releases its output at `arr + L(f)` and drops any evaluation
/// unfinished by then.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum OnionLatencyClass {
    /// `10 ms` release delay.
    Ms10 = 0,
    /// `100 ms` release delay.
    Ms100 = 1,
    /// `500 ms` release delay.
    Ms500 = 2,
    /// `2,000 ms` release delay.
    Ms2000 = 3,
}

impl OnionLatencyClass {
    /// All discrete latency classes in table order.
    pub const ALL: [Self; 4] = [Self::Ms10, Self::Ms100, Self::Ms500, Self::Ms2000];

    /// Release delay `L(f)` in milliseconds.
    pub const fn millis(self) -> u64 {
        match self {
            Self::Ms10 => 10,
            Self::Ms100 => 100,
            Self::Ms500 => 500,
            Self::Ms2000 => 2_000,
        }
    }

    /// Release delay `L(f)` as a [`Duration`].
    pub const fn duration(self) -> Duration {
        Duration::from_millis(self.millis())
    }

    /// Canonical token in an `invoke<in,out,latency>` shape name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ms10 => "10ms",
            Self::Ms100 => "100ms",
            Self::Ms500 => "500ms",
            Self::Ms2000 => "2000ms",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }

    fn parse_token(token: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|latency| latency.as_str().eq_ignore_ascii_case(token))
    }
}

/// Number `|Σ_I|` of closed `invoke` shapes in `Σ`.
pub(crate) const INVOKE_SHAPES: usize =
    OnionInputType::ALL.len() * OnionWidthClass::ALL.len() * OnionLatencyClass::ALL.len();

/// One intermediate one-shot computation shape `invoke⟨In, Out, W, L⟩ ∈ Σ_I` (#834 D1′), with
/// `W(f) = output.bytes()` and `L(f) = latency.duration()`.
///
/// Encoded in descriptors and configs as the canonical string `"invoke<in,out,latency>"`
/// (for example, `"invoke<32,64,500ms>"`).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct OnionInvokeShape {
    input: OnionInputType,
    output: OnionWidthClass,
    latency: OnionLatencyClass,
}

impl OnionInvokeShape {
    /// Construct the shape `invoke⟨input, output, latency⟩`.
    pub const fn new(
        input: OnionInputType,
        output: OnionWidthClass,
        latency: OnionLatencyClass,
    ) -> Self {
        Self {
            input,
            output,
            latency,
        }
    }

    /// Enumerate every `invoke` shape of `Σ_I` in table order.
    pub fn all() -> impl Iterator<Item = Self> {
        (0..INVOKE_SHAPES).filter_map(Self::from_index)
    }

    /// Return the carried input specification `In_f`.
    pub const fn input(self) -> OnionInputType {
        self.input
    }

    /// Return the carried output width class `Out_f`, fixing `W(f) = output.bytes()`.
    pub const fn output(self) -> OnionWidthClass {
        self.output
    }

    /// Return `W(f)`, the exact encoded output byte width (L3).
    pub const fn output_bytes(self) -> usize {
        self.output.bytes()
    }

    /// Return the latency class `L(f)` (L4).
    pub const fn latency(self) -> OnionLatencyClass {
        self.latency
    }

    /// Parse a canonical or ASCII case-insensitive `"invoke<in,out,latency>"` shape name,
    /// rejecting any name outside `Σ_I`.
    pub fn parse(raw: impl AsRef<str>) -> Result<Self> {
        let raw = raw.as_ref();
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed != raw {
            return Err(Error::InvalidConfig(
                "onion invoke shape must be non-empty and trimmed".to_string(),
            ));
        }
        let invalid = || {
            Error::InvalidConfig(format!(
                "unknown onion invoke shape {raw:?}; expected invoke<in,out,latency> over the closed signature"
            ))
        };
        let Some((prefix, rest)) = trimmed.split_once('<') else {
            return Err(invalid());
        };
        if !prefix.eq_ignore_ascii_case("invoke") {
            return Err(invalid());
        }
        let Some(inner) = rest.strip_suffix('>') else {
            return Err(invalid());
        };
        let mut parts = inner.split(',');
        let (Some(input), Some(output), Some(latency), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid());
        };
        Ok(Self {
            input: OnionInputType::parse_token(input).ok_or_else(invalid)?,
            output: OnionWidthClass::parse_token(output).ok_or_else(invalid)?,
            latency: OnionLatencyClass::parse_token(latency).ok_or_else(invalid)?,
        })
    }

    pub(crate) const fn index(self) -> usize {
        let outputs = OnionWidthClass::ALL.len();
        let latencies = OnionLatencyClass::ALL.len();
        (self.input.index() * outputs + self.output.index()) * latencies + self.latency.index()
    }

    pub(crate) fn from_index(index: usize) -> Option<Self> {
        if index >= INVOKE_SHAPES {
            return None;
        }
        let latencies = OnionLatencyClass::ALL.len();
        let outputs = OnionWidthClass::ALL.len();
        let latency_idx = index % latencies;
        let output_idx = (index / latencies) % outputs;
        let input_idx = index / (latencies * outputs);
        Some(Self {
            input: *OnionInputType::ALL.get(input_idx)?,
            output: *OnionWidthClass::ALL.get(output_idx)?,
            latency: *OnionLatencyClass::ALL.get(latency_idx)?,
        })
    }
}

impl fmt::Display for OnionInvokeShape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invoke<{},{},{}>",
            self.input.as_str(),
            self.output.as_str(),
            self.latency.as_str()
        )
    }
}

impl TryFrom<String> for OnionInvokeShape {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::parse(&value).map_err(|error| error.to_string())
    }
}

impl From<OnionInvokeShape> for String {
    fn from(shape: OnionInvokeShape) -> Self {
        shape.to_string()
    }
}

/// Canonical open backend name `ω ∈ Ω` of an `invoke` registration and application (#834 D1′, D2,
/// D3): `1 ≤ |ω| ≤ 32` over `[a-z0-9._-]`.
///
/// Validated against signed descriptors, never against a closed code table, and admitted only
/// paired with an [`OnionInvokeShape`].
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct OnionBackendName(String);

impl OnionBackendName {
    /// Parse a canonical backend name `ω`, requiring `1 ≤ |ω| ≤ 32` and every byte in
    /// `[a-z0-9._-]`.
    pub fn parse(name: impl AsRef<str>) -> Result<Self> {
        let raw = name.as_ref();
        if raw.is_empty() || raw.len() > ONION_BACKEND_NAME_MAX_BYTES {
            return Err(Error::InvalidConfig(format!(
                "onion backend name {raw:?} must be 1..={ONION_BACKEND_NAME_MAX_BYTES} bytes"
            )));
        }
        if !raw.bytes().all(is_backend_name_byte) {
            return Err(Error::InvalidConfig(format!(
                "onion backend name {raw:?} must contain only [a-z0-9._-]"
            )));
        }
        Ok(Self(raw.to_owned()))
    }

    /// Return the canonical backend name slice.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

const fn is_backend_name_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
}

impl fmt::Display for OnionBackendName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<String> for OnionBackendName {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::parse(&value).map_err(|error| error.to_string())
    }
}

impl From<OnionBackendName> for String {
    fn from(name: OnionBackendName) -> Self {
        name.0
    }
}

/// Declares `Σ_W` once, in table order, one row per world-facing symbol (`relay` and `Σ_I`
/// precede `Σ_W` in the coproduct `1 + Σ_I + Σ_W`).
macro_rules! onion_signature {
    (
        world_facing: [
            $($(#[doc = $doc:literal])* $variant:ident $field:ident $name:literal,)+
        ] $(,)?
    ) => {
        /// `|Σ_W|`, the number of world-facing rows.
        pub(crate) const WORLD_FACING_SYMBOLS: usize = [$(onion_signature!(@unit $variant),)+].len();

        /// `Σ_W` as a type: one constructor per world-facing row, in table order; the
        /// discriminant is the position within `Σ_W`.
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        #[repr(u8)]
        enum OnionWorldSymbol {
            $($(#[doc = $doc])* $variant,)+
        }

        impl OnionWorldSymbol {
            /// `Σ_W` in table order.
            const ALL: [Self; WORLD_FACING_SYMBOLS] = [$(Self::$variant,)+];

            /// The canonical name of this symbol.
            const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)+
                }
            }
        }

        impl OnionServiceName {
            /// Return `Σ_W`, the world-facing symbols of `Σ`, in table order.
            pub fn world_facing() -> impl Iterator<Item = Self> {
                OnionWorldSymbol::ALL.into_iter().map(Self)
            }

            $(
                #[doc = concat!("Return the name of the world-facing `", $name, "` symbol.")]
                pub const fn $field() -> Self {
                    Self(OnionWorldSymbol::$variant)
                }
            )+
        }
    };
    (@unit $variant:ident) => {
        ()
    };
}

onion_signature! {
    world_facing: [
        /// `tcp`, a world-facing byte stream.
        Tcp tcp "tcp",
        /// `https`, a world-facing request/response (fetch).
        Https https "https",
    ],
}

// `|Σ| = 1 + |Σ_I| + |Σ_W|` fits in the single-byte `f` field of the uniform layer (#834 D6″).
const _: () = assert!(1 + INVOKE_SHAPES + WORLD_FACING_SYMBOLS <= (u8::MAX as usize) + 1);

/// A non-relay shape symbol of `Σ \ {relay} = Σ_I ⊎ Σ_W`: what a symbol hop evaluates.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OnionShape {
    /// An intermediate one-shot computation `invoke⟨In, Out, W, L⟩ ∈ Σ_I`.
    Invoke(OnionInvokeShape),
    /// A world-facing session symbol `f ∈ Σ_W`.
    WorldFacing(OnionServiceName),
}

impl OnionShape {
    /// Enumerate every non-relay shape in `Σ_I ⊎ Σ_W` table order.
    pub fn all() -> impl Iterator<Item = Self> {
        OnionInvokeShape::all()
            .map(Self::Invoke)
            .chain(OnionServiceName::world_facing().map(Self::WorldFacing))
    }
}

impl fmt::Display for OnionShape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invoke(shape) => shape.fmt(f),
            Self::WorldFacing(service) => f.write_str(service.as_str()),
        }
    }
}

impl From<OnionInvokeShape> for OnionShape {
    fn from(shape: OnionInvokeShape) -> Self {
        Self::Invoke(shape)
    }
}

impl From<OnionServiceName> for OnionShape {
    fn from(service: OnionServiceName) -> Self {
        Self::WorldFacing(service)
    }
}

/// A symbol of `Σ = {relay} ⊎ Σ_I ⊎ Σ_W`, as the coproduct `1 + (Σ_I + Σ_W)`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum OnionSymbol {
    /// The identity symbol `relay`.
    Relay,
    /// A non-relay shape symbol of `Σ_I ⊎ Σ_W`.
    Shape(OnionShape),
}

impl OnionSymbol {
    /// Total number `|Σ|` of symbols in the closed signature.
    #[cfg(test)]
    pub(crate) const COUNT: usize = 1 + INVOKE_SHAPES + WORLD_FACING_SYMBOLS;

    /// `code : 1 + Σ_I + Σ_W → u8`: `0` for `relay`, `1 ..= |Σ_I|` for `Σ_I`, and
    /// `1 + |Σ_I| ..` for `Σ_W`.
    pub(crate) const fn code(&self) -> u8 {
        match self {
            Self::Relay => 0,
            Self::Shape(OnionShape::Invoke(shape)) => (1 + shape.index()) as u8,
            Self::Shape(OnionShape::WorldFacing(OnionServiceName(symbol))) => {
                (1 + INVOKE_SHAPES) as u8 + *symbol as u8
            }
        }
    }

    /// The left inverse of [`Self::code`]; `None` past the end of `Σ`.
    pub(crate) fn from_code(code: u8) -> Option<Self> {
        let Some(non_relay) = usize::from(code).checked_sub(1) else {
            return Some(Self::Relay);
        };
        if let Some(shape) = OnionInvokeShape::from_index(non_relay) {
            return Some(Self::Shape(OnionShape::Invoke(shape)));
        }
        let world = non_relay.checked_sub(INVOKE_SHAPES)?;
        OnionWorldSymbol::ALL
            .into_iter()
            .nth(world)
            .map(|symbol| Self::Shape(OnionShape::WorldFacing(OnionServiceName(symbol))))
    }
}

/// Canonical name of a world-facing symbol: the closed name type `Σ_W` of onion exit services.
///
/// Invariant: every value denotes a world-facing row of the signature table (see the closure
/// law). It is encoded as that row's canonical name string.
#[derive(Clone, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct OnionServiceName(OnionWorldSymbol);

impl OnionServiceName {
    /// Parse and canonicalize a service name, admitting exactly the names of `Σ_W`.
    pub fn parse(name: impl AsRef<str>) -> Result<Self> {
        let name = name.as_ref();
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed != name {
            return Err(Error::InvalidConfig(
                "onion exit service name must be non-empty and trimmed".to_string(),
            ));
        }
        OnionServiceName::world_facing()
            .find(|service| service.as_str().eq_ignore_ascii_case(trimmed))
            .ok_or_else(|| {
                Error::InvalidConfig(format!(
                    "unknown onion service {name:?}; the onion signature is closed: expected one of {}",
                    OnionServiceName::world_facing()
                        .map(|service| service.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    /// Return the canonical name as a string slice.
    pub const fn as_str(&self) -> &'static str {
        self.0.name()
    }

    /// Return whether this name equals `service` after service-name canonicalization.
    pub fn matches(&self, service: &str) -> bool {
        Self::parse(service).is_ok_and(|candidate| candidate == *self)
    }
}

impl fmt::Debug for OnionServiceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("OnionServiceName")
            .field(&self.as_str())
            .finish()
    }
}

impl PartialOrd for OnionServiceName {
    /// Names order by their canonical string, so `Σ_W` sorts the same wherever it is keyed.
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OnionServiceName {
    /// Names order by their canonical string.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl TryFrom<String> for OnionServiceName {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::parse(&value).map_err(|error| error.to_string())
    }
}

impl From<OnionServiceName> for String {
    fn from(name: OnionServiceName) -> Self {
        name.as_str().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::OnionBackendName;
    use super::OnionInputType;
    use super::OnionInvokeShape;
    use super::OnionLatencyClass;
    use super::OnionServiceName;
    use super::OnionShape;
    use super::OnionSymbol;
    use super::OnionWidthClass;
    use super::INVOKE_SHAPES;
    use super::ONION_BACKEND_NAME_MAX_BYTES;

    /// Every world-facing name parses to its own entry, case-insensitively, and nothing else
    /// parses: neither names outside `Σ_W`, nor `relay`, nor `invoke` shapes or backend names.
    #[test]
    fn test_service_names_are_exactly_the_world_facing_signature() {
        for service in OnionServiceName::world_facing() {
            assert_eq!(
                OnionServiceName::parse(service.as_str()).ok(),
                Some(service.clone())
            );
            assert_eq!(
                OnionServiceName::parse(service.as_str().to_ascii_uppercase()).ok(),
                Some(service)
            );
        }
        for outside in [
            "relay",
            "invoke<32,64,500ms>",
            "jev.typesafe",
            "web",
            "api",
            "custom",
            "tcp2",
            "",
            " tcp",
            "tcp!",
        ] {
            assert!(OnionServiceName::parse(outside).is_err());
        }
    }

    /// Every `invoke` shape round-trips through its canonical string and codec, and any unknown
    /// width, latency or syntax is rejected (D1′ closure).
    #[test]
    fn test_invoke_shapes_are_closed_and_round_trip() {
        let all = OnionInvokeShape::all().collect::<Vec<_>>();
        assert_eq!(all.len(), INVOKE_SHAPES);
        assert_eq!(INVOKE_SHAPES, 120);

        for (index, shape) in all.iter().copied().enumerate() {
            assert_eq!(shape.index(), index);
            assert_eq!(OnionInvokeShape::from_index(index), Some(shape));
            let rendered = shape.to_string();
            assert_eq!(OnionInvokeShape::parse(&rendered).ok(), Some(shape));
            assert_eq!(
                OnionInvokeShape::parse(rendered.to_ascii_uppercase()).ok(),
                Some(shape)
            );
            let encoded = rings_codec::serialize(&shape).expect("encode invoke shape");
            assert_eq!(
                rings_codec::deserialize::<OnionInvokeShape>(&encoded).ok(),
                Some(shape)
            );
        }

        let sample = OnionInvokeShape::new(
            OnionInputType::Fixed(OnionWidthClass::B32),
            OnionWidthClass::B64,
            OnionLatencyClass::Ms500,
        );
        assert_eq!(sample.to_string(), "invoke<32,64,500ms>");

        for invalid in [
            "",
            "tcp",
            "relay",
            " invoke<32,64,500ms>",
            "invoke<32,0,10ms>",
            "invoke<16,32,10ms>",
            "invoke<32,48,10ms>",
            "invoke<32,64,25ms>",
            "invoke<32,64>",
            "invoke<32,64,10ms,extra>",
        ] {
            assert!(
                OnionInvokeShape::parse(invalid).is_err(),
                "expected {invalid:?} to fail"
            );
            let bytes = rings_codec::serialize(&invalid).expect("encode raw string");
            assert!(rings_codec::deserialize::<OnionInvokeShape>(&bytes).is_err());
        }
    }

    /// Backend names admit canonical `[a-z0-9._-]` strings of `1..=32` bytes and reject empty,
    /// overlong, uppercase or non-canonical strings (D1′, D2).
    #[test]
    fn test_backend_names_enforce_canonical_bounds() {
        for valid in [
            "a",
            "add",
            "jev.typesafe",
            "hash_256-v1",
            &"x".repeat(ONION_BACKEND_NAME_MAX_BYTES),
        ] {
            let parsed = OnionBackendName::parse(valid).expect("valid backend name");
            assert_eq!(parsed.as_str(), valid);
            let encoded = rings_codec::serialize(&parsed).expect("encode backend name");
            assert_eq!(
                rings_codec::deserialize::<OnionBackendName>(&encoded).ok(),
                Some(parsed)
            );
        }
        for invalid in [
            "",
            " add",
            "add ",
            "Jev",
            "jev/typesafe",
            "jev:1",
            "jev\0",
            &"x".repeat(ONION_BACKEND_NAME_MAX_BYTES + 1),
        ] {
            assert!(
                OnionBackendName::parse(invalid).is_err(),
                "expected {invalid:?} to fail"
            );
        }
    }

    /// Decoding admits only names of `Σ_W`, and the encoding is the bare canonical name string.
    #[test]
    fn test_codec_is_the_name_string_and_rejects_names_outside_the_world_facing_signature() {
        let encoded = rings_codec::serialize(&OnionServiceName::https()).expect("encode name");

        assert_eq!(
            encoded,
            rings_codec::serialize(&"https").expect("encode string")
        );
        for outside in ["web", "relay", "invoke<32,64,10ms>"] {
            let bytes = rings_codec::serialize(&outside).expect("encode string");
            assert!(rings_codec::deserialize::<OnionServiceName>(bytes.as_slice()).is_err());
        }
    }

    /// `from_code ∘ code = Some` on `Σ`, the codes are `0..|Σ|` in table order (`relay`, then
    /// `Σ_I`, then `Σ_W`), and no code past the table names a symbol. Enumerated over all of `u8`.
    #[test]
    fn test_from_code_inverts_code() {
        let decoded = (0..=u8::MAX)
            .filter_map(|code| OnionSymbol::from_code(code).map(|symbol| (code, symbol)))
            .collect::<Vec<_>>();
        let table = std::iter::once(OnionSymbol::Relay)
            .chain(OnionShape::all().map(OnionSymbol::Shape))
            .collect::<Vec<_>>();

        assert_eq!(table.len(), OnionSymbol::COUNT);
        assert_eq!(decoded.len(), table.len());
        for ((position, (code, symbol)), expected) in decoded.iter().enumerate().zip(&table) {
            assert_eq!(usize::from(*code), position);
            assert_eq!(symbol.code(), *code);
            assert_eq!(symbol, expected);
        }
    }

    /// Service names order by their canonical string.
    #[test]
    fn test_service_names_order_by_name() {
        assert!(OnionServiceName::https() < OnionServiceName::tcp());
    }
}
