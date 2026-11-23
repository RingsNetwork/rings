//! A small, pure parser for the bits of SDP (RFC 4566 / RFC 8841) we read off a peer's session
//! description.
//!
//! It is structured as three composable passes — the shape of a Haskell parser: lex into line
//! tokens, parse those into an AST that preserves SDP's session/media structure, then fold the AST
//! to the value we want. None of it does I/O; it is a total function of the input text. The tokens
//! and attribute fields borrow from the input (no copying of values); only the per-section vectors
//! are allocated.
//!
//! ```text
//!   lex   : &str               ↦ [Line]               -- text   → tokens
//!   parse : &str               ↦ SessionDescription   -- tokens → AST (session + media sections)
//!   fold  : SessionDescription ↦ Option<u32>          -- AST    → value
//! ```
//!
//! SDP is line-oriented: every line is `<type>=<value>` where `<type>` is a single character
//! (RFC 4566 §5). It is also *sectioned*: attributes before the first `m=` line are session-level,
//! and each `m=` line opens a media section whose attributes are media-level. This matters here:
//! `a=max-message-size` is a **media-level** attribute of the SCTP association (RFC 8841 §6), so it
//! must be read from the `m=application … DTLS/SCTP … webrtc-datachannel` section we actually send
//! on — not from session level or some other media section. The AST keeps that structure so the
//! fold can ask the right section.

/// A lexed SDP line: a one-character type and its raw value, with surrounding whitespace and the
/// line ending stripped. The smallest meaningful token of an SDP document.
#[derive(Debug, PartialEq, Eq)]
pub struct Line<'a> {
    /// The line type — the single character left of `=` (`v`, `o`, `m`, `a`, …).
    pub kind: char,
    /// Everything right of the first `=`, trimmed.
    pub value: &'a str,
}

/// An `a=` line parsed into its attribute form (RFC 4566 §5.13): either a value-less property flag
/// (`a=sendrecv`) or a `key:value` pair (`a=max-message-size:262144`).
#[derive(Debug, PartialEq, Eq)]
pub enum Attribute<'a> {
    /// `a=<flag>`.
    Flag(&'a str),
    /// `a=<key>:<value>`.
    Pair {
        /// The attribute name, left of `:`, trimmed.
        key: &'a str,
        /// The attribute value, right of the first `:`, trimmed.
        value: &'a str,
    },
}

/// One media section: the `m=` line's value and the attributes that follow it (up to the next
/// `m=`).
#[derive(Debug, PartialEq, Eq)]
pub struct MediaDescription<'a> {
    /// The `m=` line value, e.g. `application 9 UDP/DTLS/SCTP webrtc-datachannel`.
    pub media: &'a str,
    /// Media-level attributes declared under this `m=` line.
    pub attrs: Vec<Attribute<'a>>,
}

/// The AST: session-level attributes followed by the ordered media sections.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SessionDescription<'a> {
    /// Attributes appearing before the first `m=` line.
    pub session_attrs: Vec<Attribute<'a>>,
    /// Media sections in document order.
    pub media: Vec<MediaDescription<'a>>,
}

/// Lex: split the document into [`Line`] tokens. Lines that are not `<single-char>=<value>` (blank
/// lines, malformed lines) are dropped. Tolerant of both CRLF and LF endings.
pub fn lex(sdp: &str) -> impl Iterator<Item = Line<'_>> {
    sdp.lines().filter_map(|raw| {
        let (kind, value) = raw.trim().split_once('=')?;
        let mut chars = kind.chars();
        let kind = chars.next()?;
        // RFC 4566 §5: the type is exactly one character.
        if chars.next().is_some() {
            return None;
        }
        Some(Line {
            kind,
            value: value.trim(),
        })
    })
}

/// Parse the value of an `a=` line into an [`Attribute`] — a `Pair` if it contains `:`, else a
/// `Flag`.
pub fn parse_attribute(value: &str) -> Attribute<'_> {
    match value.split_once(':') {
        Some((key, value)) => Attribute::Pair {
            key: key.trim(),
            value: value.trim(),
        },
        None => Attribute::Flag(value),
    }
}

/// Parse: tokens ↦ AST. Routes each `a=` line into the current section (session-level until the
/// first `m=`, media-level after), and opens a new media section on each `m=`. Other line kinds are
/// not needed by this policy and are ignored.
pub fn parse(sdp: &str) -> SessionDescription<'_> {
    let mut description = SessionDescription::default();
    for line in lex(sdp) {
        match line.kind {
            'm' => description.media.push(MediaDescription {
                media: line.value,
                attrs: Vec::new(),
            }),
            'a' => {
                let attr = parse_attribute(line.value);
                match description.media.last_mut() {
                    Some(section) => section.attrs.push(attr),
                    None => description.session_attrs.push(attr),
                }
            }
            _ => {}
        }
    }
    description
}

impl MediaDescription<'_> {
    /// Whether this section is a **live** (non-rejected) WebRTC SCTP data channel.
    ///
    /// An `m=` line is `<media> <port> <proto> <fmt> …` (RFC 4566 §5.14); we require it field by
    /// field (not by substring) so a value that merely *mentions* these tokens elsewhere cannot be
    /// mistaken for the data channel:
    ///
    /// - `<media>` is `application` (case-insensitive),
    /// - `<port>` is not `0` — a `0` port marks a **rejected** section (RFC 4566 / RFC 3264 §5.1),
    ///   which must be skipped so a rejected data-channel section before the active one is not
    ///   selected,
    /// - `<proto>` is a `/`-separated profile that includes `SCTP` (case-insensitive), and
    /// - it is recognisable as a data channel by **either** the modern `webrtc-datachannel` `<fmt>`
    ///   token (RFC 8841) **or** a legacy `a=sctp-port` / `a=sctpmap` attribute (the pre-standard
    ///   numeric-`<fmt>` form some older stacks still emit). Matching is case-insensitive.
    fn is_data_channel(&self) -> bool {
        let mut fields = self.media.split_whitespace();
        if !fields
            .next()
            .is_some_and(|m| m.eq_ignore_ascii_case("application"))
        {
            return false;
        }
        match fields.next() {
            Some("0") | None => return false, // rejected (port 0) or truncated
            _ => {}
        }
        let Some(proto) = fields.next() else {
            return false;
        };
        if !proto.split('/').any(|t| t.eq_ignore_ascii_case("SCTP")) {
            return false;
        }
        let has_datachannel_fmt = fields.any(|fmt| fmt.eq_ignore_ascii_case("webrtc-datachannel"));
        let has_sctp_attr = self.attrs.iter().any(|attr| {
            let key = match attr {
                Attribute::Flag(f) => *f,
                Attribute::Pair { key, .. } => *key,
            };
            key.eq_ignore_ascii_case("sctp-port") || key.eq_ignore_ascii_case("sctpmap")
        });
        has_datachannel_fmt || has_sctp_attr
    }

    /// `a=max-message-size` declared in this section, if any. Matched case-insensitively. The first
    /// occurrence wins (a well-formed SDP carries at most one; on a duplicate we take the first
    /// deterministically). A *present but unparsable* value is a malformed advertisement: it is
    /// logged and treated as absent — there is no smaller value we could safely assume instead.
    fn max_message_size(&self) -> Option<u32> {
        for attr in &self.attrs {
            if let Attribute::Pair { key, value } = attr {
                if key.eq_ignore_ascii_case("max-message-size") {
                    return value.parse::<u32>().map_or_else(
                        |_| {
                            tracing::warn!(
                                "SDP: malformed a=max-message-size {value:?}; ignoring (using default)"
                            );
                            None
                        },
                        Some,
                    );
                }
            }
        }
        None
    }
}

impl SessionDescription<'_> {
    /// The live WebRTC SCTP data-channel media section, if the description has one.
    pub fn data_channel(&self) -> Option<&MediaDescription<'_>> {
        self.media.iter().find(|section| section.is_data_channel())
    }

    /// The SCTP `a=max-message-size` (RFC 8841 §6) for the data channel — read from the
    /// data-channel media section only, never session level or an unrelated media section.
    pub fn data_channel_max_message_size(&self) -> Option<u32> {
        self.data_channel()?.max_message_size()
    }
}

/// Parse the data channel's `a=max-message-size` (RFC 8841) out of an SDP, composing the full
/// lex → parse → fold pipeline. Returns `None` when there is no data-channel section or it carries
/// no (valid) `max-message-size`. This is the same source webrtc reads internally; we parse it
/// ourselves so the negotiated limit is available identically on native and browser, without
/// relying on a backend to expose it.
pub fn parse_sdp_max_message_size(sdp: &str) -> Option<u32> {
    parse(sdp).data_channel_max_message_size()
}

/// The effective per-message send limit for a peer whose SDP handshake description is
/// `remote_handshake`.
///
/// An absent attribute defaults to [`MAX_CHANNEL_MESSAGE_SIZE`](crate::core::transport::MAX_CHANNEL_MESSAGE_SIZE)
/// (`65536`) and a value of `0` means "no limit" (still bounded by our own send cap); any explicit
/// value at or above [`MIN_MAX_MESSAGE_SIZE`](crate::core::transport::MIN_MAX_MESSAGE_SIZE) is
/// honoured and capped at [`MAX_CHANNEL_MESSAGE_SIZE`](crate::core::transport::MAX_CHANNEL_MESSAGE_SIZE)
/// for interop, while any positive value below [`MIN_MAX_MESSAGE_SIZE`](crate::core::transport::MIN_MAX_MESSAGE_SIZE)
/// is refused with [`Error::RemoteMaxMessageSizeTooSmall`](crate::error::Error::RemoteMaxMessageSizeTooSmall)
/// so DHT control frames are never chunked (#1130).
pub fn effective_max_message_size(remote_handshake: &str) -> crate::error::Result<usize> {
    use crate::core::transport::MAX_CHANNEL_MESSAGE_SIZE;
    use crate::core::transport::MIN_MAX_MESSAGE_SIZE;
    use crate::error::Error;

    match parse_sdp_max_message_size(remote_handshake) {
        None | Some(0) => Ok(MAX_CHANNEL_MESSAGE_SIZE),
        Some(n) if (n as usize) < MIN_MAX_MESSAGE_SIZE => {
            Err(Error::RemoteMaxMessageSizeTooSmall {
                advertised: n as usize,
                minimum: MIN_MAX_MESSAGE_SIZE,
            })
        }
        Some(n) => Ok((n as usize).min(MAX_CHANNEL_MESSAGE_SIZE)),
    }
}

/// The connectivity tier of an ICE candidate or of the ICE server set configured on a peer
/// connection, ordered as a join-semilattice (`Host < Reflexive < Relay`).
///
/// Law (Settle threshold): given a peer connection's target tier `t` (the join of its configured
/// ICE servers, or `Host` when none are configured) and the highest gathered candidate tier `c`,
/// gathering has reached its target tier iff `c >= t`.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum IceCandidateTier {
    /// Local interface candidate (`typ host`), or an ICE configuration with no STUN/TURN servers.
    #[default]
    Host,
    /// Server-reflexive (`typ srflx`) or peer-reflexive (`typ prflx`) candidate, or an ICE
    /// configuration with at least one `stun:` server and no `turn:`/`turns:` servers.
    Reflexive,
    /// Relayed TURN candidate (`typ relay`), or an ICE configuration with at least one
    /// `turn:`/`turns:` server.
    Relay,
}

/// Settle timer window selected by the pure ICE gathering transition algebra.
///
/// Ordered by duration (`Fast < Fallback`), so joining two unlocked windows via `min` always
/// selects the shorter window (`Fast` dominates `Fallback`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum IceSettleWindow {
    /// Fast quiet window (`300 ms`) once a candidate reaches the configured target tier.
    Fast,
    /// Fallback quiet window (`3 s`) when only `Host` candidates have arrived for a `Reflexive`
    /// (STUN-only) target.
    Fallback,
}

/// Pure state of one ICE gathering session (`State` in `State × Event -> State × Effect`).
///
/// Invariants:
/// - `No premature target-tier omission`: for every `target_tier ∈ {Host, Reflexive, Relay}`,
///   `armed_window.is_some() ⟺ highest_observed.is_some_and(|c| c >= target_tier)`.
/// - `Fast window on target tier`:
///   `highest_observed.is_some_and(|c| c >= target_tier) ⟹ armed_window == Some(IceSettleWindow::Fast)`.
/// - `Quiet-window reset`: whenever `armed_window.is_some()`, observing any candidate line
///   (`Host`, `Reflexive`, or `Relay`) strictly increments `epoch`.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct IceGatherState {
    /// Target candidate tier derived from configured ICE servers (`Host < Reflexive < Relay`).
    pub target_tier: IceCandidateTier,
    /// Highest candidate tier observed so far, if any.
    pub highest_observed: Option<IceCandidateTier>,
    /// Currently armed settle window, if any.
    pub armed_window: Option<IceSettleWindow>,
    /// Monotonic quiet-window epoch; incremented whenever a candidate arrives while a window is
    /// (or becomes) armed.
    pub epoch: u64,
}

impl IceGatherState {
    /// Create a fresh gathering state for `target_tier`.
    pub const fn new(target_tier: IceCandidateTier) -> Self {
        Self {
            target_tier,
            highest_observed: None,
            armed_window: None,
            epoch: 0,
        }
    }

    /// Classify the settle window unlocked by a single `candidate_tier` for `target_tier`.
    ///
    /// Law (Target-tier threshold): the early quiet-window settle timer (`300 ms`) is unlocked iff
    /// `candidate_tier >= target_tier`. Lower-tier candidates never arm an early timer while the
    /// configured tier (`Reflexive` for STUN or `Relay` for TURN) is still pending, so non-trickle
    /// SDP never finalizes a host-only description while STUN or TURN gathering is still in flight.
    pub const fn window_for_tier(
        candidate_tier: IceCandidateTier,
        target_tier: IceCandidateTier,
    ) -> Option<IceSettleWindow> {
        if (candidate_tier as u8) >= (target_tier as u8) {
            Some(IceSettleWindow::Fast)
        } else {
            None
        }
    }

    /// Pure transition `State × IceCandidateTier -> (State, bool)`: observe a candidate of
    /// `candidate_tier` and return `(next_state, timer_armed_or_reset)`.
    pub fn observe_tier(self, candidate_tier: IceCandidateTier) -> (Self, bool) {
        let highest_observed = Some(
            self.highest_observed
                .map_or(candidate_tier, |prev| prev.max(candidate_tier)),
        );
        let unlocked = Self::window_for_tier(candidate_tier, self.target_tier);
        let armed_window = match (self.armed_window, unlocked) {
            (Some(existing), Some(new)) => Some(existing.min(new)),
            (Some(existing), None) => Some(existing),
            (None, unlocked) => unlocked,
        };
        if armed_window.is_some() {
            (
                Self {
                    target_tier: self.target_tier,
                    highest_observed,
                    armed_window,
                    epoch: self.epoch.wrapping_add(1),
                },
                true,
            )
        } else {
            (
                Self {
                    target_tier: self.target_tier,
                    highest_observed,
                    armed_window: None,
                    epoch: self.epoch,
                },
                false,
            )
        }
    }

    /// Observe raw candidate/SDP `text`: if it contains at least one candidate line, apply
    /// [`Self::observe_tier`] with the highest tier in `text`; otherwise preserve state and return
    /// `(self, false)`.
    pub fn observe_text(self, text: &str) -> (Self, bool) {
        match parse_sdp_candidate_tier(text) {
            Some(tier) => self.observe_tier(tier),
            None => (self, false),
        }
    }
}

/// Classify a single ICE candidate line (`candidate:...` or `a=candidate:...`) into its
/// [`IceCandidateTier`], or `None` if `line` is not a candidate line.
pub fn parse_candidate_line_tier(line: &str) -> Option<IceCandidateTier> {
    let trimmed = line.trim();
    let candidate = trimmed.strip_prefix("a=").unwrap_or(trimmed);
    if !candidate.starts_with("candidate:") {
        return None;
    }
    let mut tokens = candidate.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "typ" {
            return Some(match tokens.next() {
                Some(kind) if kind.eq_ignore_ascii_case("relay") => IceCandidateTier::Relay,
                Some(kind)
                    if kind.eq_ignore_ascii_case("srflx") || kind.eq_ignore_ascii_case("prflx") =>
                {
                    IceCandidateTier::Reflexive
                }
                _ => IceCandidateTier::Host,
            });
        }
    }
    Some(IceCandidateTier::Host)
}

/// Fold all ICE candidate lines in `text` (either a raw `onicecandidate` string or a full SDP
/// document) into the highest [`IceCandidateTier`] observed, or `None` if `text` contains no
/// candidate lines.
pub fn parse_sdp_candidate_tier(text: &str) -> Option<IceCandidateTier> {
    text.lines().filter_map(parse_candidate_line_tier).max()
}

/// Partition an incoming SDP document into a candidate-free SDP description and the extracted
/// `candidate:...` attribute strings (without the leading `a=`), dropping `a=end-of-candidates`.
///
/// Passing the candidate-free offer to `RTCPeerConnection::set_remote_description` starts local
/// answer gathering immediately without letting `webrtc-rs` fire outbound UDP STUN connectivity
/// checks at the remote peer's `srflx` NAT port before the HTTP `answerOffer` response has
/// returned to the browser (#1290).
pub fn split_sdp_remote_candidates(sdp: &str) -> (String, Vec<String>) {
    let mut stripped = String::with_capacity(sdp.len());
    let mut candidates = Vec::new();
    for segment in sdp.split_inclusive('\n') {
        let trimmed = segment.trim_end_matches(['\r', '\n']).trim();
        if let Some(cand) = trimmed.strip_prefix("a=candidate:") {
            candidates.push(format!("candidate:{cand}"));
        } else if trimmed == "a=end-of-candidates" {
            continue;
        } else {
            stripped.push_str(segment);
        }
    }
    (stripped, candidates)
}

#[cfg(all(test, not(target_family = "wasm")))]
mod test_ice_gather_model;

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use super::effective_max_message_size;
    use super::lex;
    use super::parse;
    use super::parse_attribute;
    use super::parse_sdp_max_message_size;
    use super::Attribute;
    use super::Line;

    const SEED_ENV: &str = "RINGS_DECODE_BOUNDARY_SEED";
    const CASES_ENV: &str = "RINGS_DECODE_BOUNDARY_CASES";
    const DEFAULT_CASES: usize = 128;

    /// A minimal but realistic data-channel SDP, with a `body` of media-section attribute lines.
    fn data_channel_sdp(body: &str) -> String {
        format!(
            "v=0\r\n\
             o=- 0 0 IN IP4 0.0.0.0\r\n\
             m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
             a=setup:actpass\r\n\
            {body}"
        )
    }

    #[test]
    fn generated_sdp_decode_boundary_inputs_are_total() {
        let mut generator = DecodeBoundaryGenerator::named("transport-sdp");
        let cases = generated_case_count();
        eprintln!(
            "{SEED_ENV}={} {CASES_ENV}={cases} target=transport-sdp",
            generator.seed()
        );

        for _ in 0..cases {
            let bytes = generator.bytes(4096);
            let lossy = String::from_utf8_lossy(&bytes);
            let _raw = parse_sdp_max_message_size(&lossy);
            let sdp = generator.sdp();
            let _structured = parse_sdp_max_message_size(&sdp);
        }
    }

    fn generated_case_count() -> usize {
        std::env::var(CASES_ENV)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .map(|cases| cases.clamp(1, 50_000))
            .unwrap_or(DEFAULT_CASES)
    }

    struct DecodeBoundaryGenerator {
        seed: u64,
        state: u64,
    }

    impl DecodeBoundaryGenerator {
        fn named(label: &str) -> Self {
            let seed = std::env::var(SEED_ENV)
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or_else(time_seed);
            let state = fold_label(seed, label);
            Self { seed, state }
        }

        fn seed(&self) -> u64 {
            self.seed
        }

        fn next_u64(&mut self) -> u64 {
            self.state = splitmix64(self.state);
            self.state
        }

        fn bytes(&mut self, max_len: usize) -> Vec<u8> {
            let len = self.usize(max_len.saturating_add(1));
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                bytes.push(self.byte());
            }
            bytes
        }

        fn byte(&mut self) -> u8 {
            u8::try_from(self.next_u64() & 0xff).unwrap_or(0)
        }

        fn usize(&mut self, upper: usize) -> usize {
            let upper = u64::try_from(upper).unwrap_or(u64::MAX).max(1);
            usize::try_from(self.next_u64() % upper).unwrap_or(0)
        }

        fn sdp(&mut self) -> String {
            let mut text = String::new();
            for _ in 0..self.usize(24).saturating_add(1) {
                text.push_str(&self.sdp_line());
            }
            text
        }

        fn sdp_line(&mut self) -> String {
            match self.usize(6) {
                0 => format!("{}={}\r\n", self.line_kind(), self.text(96)),
                1 => format!(
                    "m={} {} {} {}\r\n",
                    self.media(),
                    self.port(),
                    self.proto(),
                    self.format_token()
                ),
                2 => format!("a=max-message-size:{}\r\n", self.next_u64()),
                3 => format!("a={}:{}\r\n", self.text(32), self.text(64)),
                4 => "a=sctp-port:5000\r\n".to_string(),
                _ => self.text(128),
            }
        }

        fn text(&mut self, max_len: usize) -> String {
            let len = self.usize(max_len.saturating_add(1));
            (0..len)
                .map(|_| char::from(32_u8.saturating_add(self.byte() % 95)))
                .collect()
        }

        fn line_kind(&mut self) -> char {
            match self.usize(6) {
                0 => 'v',
                1 => 'o',
                2 => 'm',
                3 => 'a',
                4 => '=',
                _ => 'x',
            }
        }

        fn media(&mut self) -> &'static str {
            match self.usize(4) {
                0 => "application",
                1 => "Application",
                2 => "audio",
                _ => "video",
            }
        }

        fn port(&mut self) -> &'static str {
            match self.usize(4) {
                0 => "0",
                1 => "9",
                2 => "65535",
                _ => "port",
            }
        }

        fn proto(&mut self) -> &'static str {
            match self.usize(5) {
                0 => "UDP/DTLS/SCTP",
                1 => "TCP/DTLS/SCTP",
                2 => "udp/dtls/sctp",
                3 => "UDP/DTLS/SCTPX",
                _ => "RTP/AVP",
            }
        }

        fn format_token(&mut self) -> &'static str {
            match self.usize(4) {
                0 => "webrtc-datachannel",
                1 => "WEBRTC-DATACHANNEL",
                2 => "5000",
                _ => "webrtc-datachannel-x",
            }
        }
    }

    fn time_seed() -> u64 {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(duration.as_secs());
        nanos ^ u64::from(std::process::id())
    }

    fn fold_label(seed: u64, label: &str) -> u64 {
        label
            .bytes()
            .fold(seed, |state, byte| splitmix64(state ^ u64::from(byte)))
    }

    fn splitmix64(mut state: u64) -> u64 {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed = state;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^ (mixed >> 31)
    }

    #[test]
    fn test_lex_splits_type_and_value_and_drops_junk() {
        let lines: Vec<Line> = lex("v=0\r\na=sendrecv\r\n\r\nnonsense\r\n").collect();
        assert_eq!(lines, vec![
            Line {
                kind: 'v',
                value: "0"
            },
            Line {
                kind: 'a',
                value: "sendrecv"
            },
        ]);
    }

    #[test]
    fn test_lex_rejects_multi_char_type() {
        // `ab=...` is not a valid SDP line (type must be one char).
        assert_eq!(lex("ab=1\r\n").count(), 0);
    }

    #[test]
    fn test_parse_attribute_flag_vs_pair() {
        assert_eq!(parse_attribute("sendrecv"), Attribute::Flag("sendrecv"));
        assert_eq!(
            parse_attribute("max-message-size:262144"),
            Attribute::Pair {
                key: "max-message-size",
                value: "262144"
            }
        );
    }

    #[test]
    fn test_parse_routes_attributes_by_section() {
        let sdp = "a=group:BUNDLE 0\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=max-message-size:65536\r\n";
        let ast = parse(sdp);
        // the pre-`m=` attribute is session-level (and `group:…` parses as a key:value pair)
        assert_eq!(ast.session_attrs, vec![Attribute::Pair {
            key: "group",
            value: "BUNDLE 0"
        }]);
        // the post-`m=` attribute is media-level under the data-channel section
        assert_eq!(ast.media.len(), 1);
        assert_eq!(
            ast.media[0].media,
            "application 9 UDP/DTLS/SCTP webrtc-datachannel"
        );
        assert_eq!(ast.media[0].attrs, vec![Attribute::Pair {
            key: "max-message-size",
            value: "65536"
        }]);
    }

    #[test]
    fn test_data_channel_section_value_is_selected() {
        let sdp = data_channel_sdp("a=max-message-size:65536\r\n");
        assert_eq!(parse_sdp_max_message_size(&sdp), Some(65536));
    }

    #[test]
    fn test_session_level_value_is_ignored() {
        // `max-message-size` before the first `m=` is session-level and must not be used.
        let sdp = "a=max-message-size:1234\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=sctp-port:5000\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), None);
    }

    #[test]
    fn test_other_media_section_value_is_ignored() {
        // a value on an audio section must not leak into the data-channel limit.
        let sdp = "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
                   a=max-message-size:99999\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=max-message-size:65536\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), Some(65536));
    }

    #[test]
    fn test_no_data_channel_section_is_none() {
        let sdp = "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=max-message-size:99999\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), None);
    }

    #[test]
    fn test_absent_attribute_is_none() {
        let sdp = data_channel_sdp("a=sctp-port:5000\r\n");
        assert_eq!(parse_sdp_max_message_size(&sdp), None);
    }

    #[test]
    fn test_zero_is_preserved() {
        let sdp = data_channel_sdp("a=max-message-size:0\r\n");
        assert_eq!(parse_sdp_max_message_size(&sdp), Some(0));
    }

    #[test]
    fn test_tolerates_surrounding_whitespace() {
        let sdp = data_channel_sdp("   a=max-message-size: 1200 \r\n");
        assert_eq!(parse_sdp_max_message_size(&sdp), Some(1200));
    }

    #[test]
    fn test_malformed_value_is_none() {
        assert_eq!(
            parse_sdp_max_message_size(&data_channel_sdp("a=max-message-size:notanumber\r\n")),
            None
        );
        assert_eq!(
            parse_sdp_max_message_size(&data_channel_sdp("a=max-message-size:\r\n")),
            None
        );
    }

    #[test]
    fn test_first_within_section_wins_when_repeated() {
        let sdp = data_channel_sdp("a=max-message-size:1024\r\na=max-message-size:2048\r\n");
        assert_eq!(parse_sdp_max_message_size(&sdp), Some(1024));
    }

    #[test]
    fn test_tcp_dtls_sctp_proto_is_accepted() {
        let sdp = "m=application 9 TCP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=max-message-size:65536\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), Some(65536));
    }

    #[test]
    fn test_substring_only_proto_is_rejected() {
        // `SCTPX` contains the substring "SCTP" but is not the `SCTP` proto token; a substring
        // match would wrongly accept this section.
        let sdp = "m=application 9 UDP/DTLS/SCTPX webrtc-datachannel\r\n\
                   a=max-message-size:65536\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), None);
    }

    #[test]
    fn test_substring_only_fmt_is_rejected() {
        // `webrtc-datachannel-x` contains the substring but is not the `webrtc-datachannel` fmt.
        let sdp = "m=application 9 UDP/DTLS/SCTP webrtc-datachannel-x\r\n\
                   a=max-message-size:65536\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), None);
    }

    #[test]
    fn test_non_application_media_with_datachannel_token_is_rejected() {
        // an `m=` line that merely names `webrtc-datachannel` as a format on a non-application media
        // must not be treated as the data channel.
        let sdp = "m=audio 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=max-message-size:65536\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), None);
    }

    #[test]
    fn test_legacy_numeric_fmt_with_sctpmap_is_detected() {
        // Pre-RFC-8841 form: numeric `<fmt>` (5000) and an `a=sctpmap`/`a=sctp-port` instead of the
        // `webrtc-datachannel` token. We must still recognise it and read its limit.
        let sdp = "m=application 9 DTLS/SCTP 5000\r\n\
                   a=sctpmap:5000 webrtc-datachannel 1024\r\n\
                   a=max-message-size:16384\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), Some(16384));

        let sdp2 = "m=application 9 UDP/DTLS/SCTP 5000\r\n\
                    a=sctp-port:5000\r\n\
                    a=max-message-size:16384\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp2), Some(16384));
    }

    #[test]
    fn test_rejected_data_channel_section_is_skipped() {
        // A rejected (`port 0`) data-channel section before the active one must not be selected.
        let sdp = "m=application 0 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=max-message-size:111\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=max-message-size:65536\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), Some(65536));
    }

    #[test]
    fn test_matching_is_case_insensitive() {
        // Lowercase proto token, mixed-case media, and mixed-case attribute key.
        let sdp = "m=Application 9 udp/dtls/sctp webrtc-datachannel\r\n\
                   a=Max-Message-Size:262144\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), Some(262144));
    }

    #[test]
    fn test_ice_candidate_tier_semilattice_and_sdp_partition_laws() {
        use super::parse_candidate_line_tier;
        use super::parse_sdp_candidate_tier;
        use super::split_sdp_remote_candidates;
        use super::IceCandidateTier;

        const TIERS: [IceCandidateTier; 3] = [
            IceCandidateTier::Host,
            IceCandidateTier::Reflexive,
            IceCandidateTier::Relay,
        ];
        // Semilattice laws: idempotence, commutativity, associativity, and bottom element `Host`.
        for a in TIERS {
            assert_eq!(a.max(a), a);
            assert_eq!(a.max(IceCandidateTier::Host), a);
            for b in TIERS {
                assert_eq!(a.max(b), b.max(a));
                for c in TIERS {
                    assert_eq!(a.max(b).max(c), a.max(b.max(c)));
                }
            }
        }

        let host = "a=candidate:1 1 udp 2130706431 10.0.0.2 51345 typ host";
        let srflx =
            "a=candidate:2 1 udp 1677729535 14.155.204.30 51345 typ srflx raddr 0.0.0.0 rport 0";
        let prflx =
            "candidate:3 1 udp 1853824767 198.51.100.7 61000 typ prflx raddr 10.0.0.2 rport 51345";
        let relay =
            "a=candidate:4 1 udp 16777215 203.0.113.5 54000 typ relay raddr 0.0.0.0 rport 0";

        assert_eq!(
            parse_candidate_line_tier(host),
            Some(IceCandidateTier::Host)
        );
        assert_eq!(
            parse_candidate_line_tier(srflx),
            Some(IceCandidateTier::Reflexive)
        );
        assert_eq!(
            parse_candidate_line_tier(prflx),
            Some(IceCandidateTier::Reflexive)
        );
        assert_eq!(
            parse_candidate_line_tier(relay),
            Some(IceCandidateTier::Relay)
        );

        let full_sdp = format!(
            "v=0\r\n\
             o=- 1 2 IN IP4 127.0.0.1\r\n\
             m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
             a=max-message-size:65536\r\n\
             {host}\r\n\
             {srflx}\r\n\
             a=end-of-candidates\r\n"
        );
        assert_eq!(
            parse_sdp_candidate_tier(&full_sdp),
            Some(IceCandidateTier::Reflexive)
        );
        let (stripped, extracted) = split_sdp_remote_candidates(&full_sdp);
        assert_eq!(parse_sdp_candidate_tier(&stripped), None);
        assert_eq!(parse_sdp_max_message_size(&stripped), Some(65536));
        assert_eq!(extracted.len(), 2);
        assert_eq!(
            extracted
                .iter()
                .filter_map(|c| parse_candidate_line_tier(c))
                .max(),
            Some(IceCandidateTier::Reflexive)
        );
    }

    #[test]
    fn test_effective_max_message_size_policy_layers() {
        use crate::core::transport::MAX_CHANNEL_MESSAGE_SIZE;
        use crate::core::transport::MIN_MAX_MESSAGE_SIZE;
        use crate::error::Error;

        let sdp_with = |value: &str| -> String {
            format!(
                "v=0\r\n\
                 m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                 a=max-message-size:{value}\r\n"
            )
        };

        assert_eq!(
            effective_max_message_size(
                "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n"
            )
            .unwrap(),
            MAX_CHANNEL_MESSAGE_SIZE
        );
        assert_eq!(
            effective_max_message_size(&sdp_with("0")).unwrap(),
            MAX_CHANNEL_MESSAGE_SIZE
        );
        for advertised in [1_usize, 2048, MIN_MAX_MESSAGE_SIZE - 1] {
            let result = effective_max_message_size(&sdp_with(&advertised.to_string()));
            assert!(
                matches!(
                    result,
                    Err(Error::RemoteMaxMessageSizeTooSmall {
                        advertised: got,
                        minimum: MIN_MAX_MESSAGE_SIZE,
                    }) if got == advertised
                ),
                "expected RemoteMaxMessageSizeTooSmall for {advertised}, got {result:?}"
            );
        }
        assert_eq!(
            effective_max_message_size(&sdp_with(&MIN_MAX_MESSAGE_SIZE.to_string())).unwrap(),
            MIN_MAX_MESSAGE_SIZE
        );
        assert_eq!(
            effective_max_message_size(&sdp_with("16384")).unwrap(),
            16384
        );
        assert_eq!(
            effective_max_message_size(&sdp_with("1048576")).unwrap(),
            MAX_CHANNEL_MESSAGE_SIZE
        );
        assert_eq!(
            effective_max_message_size(&sdp_with("65536")).unwrap(),
            MAX_CHANNEL_MESSAGE_SIZE
        );
    }
}
