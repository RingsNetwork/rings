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

#[cfg(test)]
mod tests {
    use super::lex;
    use super::parse;
    use super::parse_attribute;
    use super::parse_sdp_max_message_size;
    use super::Attribute;
    use super::Line;

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
    fn lex_splits_type_and_value_and_drops_junk() {
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
    fn lex_rejects_multi_char_type() {
        // `ab=...` is not a valid SDP line (type must be one char).
        assert_eq!(lex("ab=1\r\n").count(), 0);
    }

    #[test]
    fn parse_attribute_flag_vs_pair() {
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
    fn parse_routes_attributes_by_section() {
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
    fn data_channel_section_value_is_selected() {
        let sdp = data_channel_sdp("a=max-message-size:65536\r\n");
        assert_eq!(parse_sdp_max_message_size(&sdp), Some(65536));
    }

    #[test]
    fn session_level_value_is_ignored() {
        // `max-message-size` before the first `m=` is session-level and must not be used.
        let sdp = "a=max-message-size:1234\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=sctp-port:5000\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), None);
    }

    #[test]
    fn other_media_section_value_is_ignored() {
        // a value on an audio section must not leak into the data-channel limit.
        let sdp = "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
                   a=max-message-size:99999\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=max-message-size:65536\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), Some(65536));
    }

    #[test]
    fn no_data_channel_section_is_none() {
        let sdp = "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=max-message-size:99999\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), None);
    }

    #[test]
    fn absent_attribute_is_none() {
        let sdp = data_channel_sdp("a=sctp-port:5000\r\n");
        assert_eq!(parse_sdp_max_message_size(&sdp), None);
    }

    #[test]
    fn zero_is_preserved() {
        let sdp = data_channel_sdp("a=max-message-size:0\r\n");
        assert_eq!(parse_sdp_max_message_size(&sdp), Some(0));
    }

    #[test]
    fn tolerates_surrounding_whitespace() {
        let sdp = data_channel_sdp("   a=max-message-size: 1200 \r\n");
        assert_eq!(parse_sdp_max_message_size(&sdp), Some(1200));
    }

    #[test]
    fn malformed_value_is_none() {
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
    fn first_within_section_wins_when_repeated() {
        let sdp = data_channel_sdp("a=max-message-size:1024\r\na=max-message-size:2048\r\n");
        assert_eq!(parse_sdp_max_message_size(&sdp), Some(1024));
    }

    #[test]
    fn tcp_dtls_sctp_proto_is_accepted() {
        let sdp = "m=application 9 TCP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=max-message-size:65536\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), Some(65536));
    }

    #[test]
    fn substring_only_proto_is_rejected() {
        // `SCTPX` contains the substring "SCTP" but is not the `SCTP` proto token; a substring
        // match would wrongly accept this section.
        let sdp = "m=application 9 UDP/DTLS/SCTPX webrtc-datachannel\r\n\
                   a=max-message-size:65536\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), None);
    }

    #[test]
    fn substring_only_fmt_is_rejected() {
        // `webrtc-datachannel-x` contains the substring but is not the `webrtc-datachannel` fmt.
        let sdp = "m=application 9 UDP/DTLS/SCTP webrtc-datachannel-x\r\n\
                   a=max-message-size:65536\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), None);
    }

    #[test]
    fn non_application_media_with_datachannel_token_is_rejected() {
        // an `m=` line that merely names `webrtc-datachannel` as a format on a non-application media
        // must not be treated as the data channel.
        let sdp = "m=audio 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=max-message-size:65536\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), None);
    }

    #[test]
    fn legacy_numeric_fmt_with_sctpmap_is_detected() {
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
    fn rejected_data_channel_section_is_skipped() {
        // A rejected (`port 0`) data-channel section before the active one must not be selected.
        let sdp = "m=application 0 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=max-message-size:111\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=max-message-size:65536\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), Some(65536));
    }

    #[test]
    fn matching_is_case_insensitive() {
        // Lowercase proto token, mixed-case media, and mixed-case attribute key.
        let sdp = "m=Application 9 udp/dtls/sctp webrtc-datachannel\r\n\
                   a=Max-Message-Size:262144\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), Some(262144));
    }
}
