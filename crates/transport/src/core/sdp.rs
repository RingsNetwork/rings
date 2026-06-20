//! A small, pure parser for the bits of SDP (RFC 4566) we read off a peer's session description.
//!
//! It is deliberately structured as three composable, allocation-free (borrowing) passes — the
//! shape of a Haskell parser: lex into tokens, parse the tokens into an AST (a sum type over line
//! kinds), then fold the AST to the value we want. None of it does I/O; it is a total function of
//! the input text.
//!
//! ```text
//!   lex   : &str        ↦ [Line]                  -- text  → tokens
//!   parse : [Line]      ↦ SessionDescription      -- tokens → AST
//!   fold  : SessionDescription ↦ Option<u32>      -- AST    → value
//! ```
//!
//! SDP is line-oriented: every line is `<type>=<value>` where `<type>` is a single character
//! (RFC 4566 §5). We model attribute lines (`a=`) precisely, since they carry the value we need
//! (`a=max-message-size`, RFC 8841 §6), and keep every other line kind verbatim so the AST stays a
//! faithful, total representation of the input rather than a lossy filter.

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

/// One node of the AST: a sum type over SDP line kinds. Attribute lines are parsed into
/// [`Attribute`]; every other kind is preserved as a raw [`SdpItem::Field`].
#[derive(Debug, PartialEq, Eq)]
pub enum SdpItem<'a> {
    /// An `a=` line.
    Attribute(Attribute<'a>),
    /// Any other `<type>=<value>` line, kept verbatim.
    Field {
        /// The line type character.
        kind: char,
        /// The raw, trimmed value.
        value: &'a str,
    },
}

/// The AST: an SDP session description as the ordered sequence of its lines.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SessionDescription<'a> {
    /// Lines in document order.
    pub items: Vec<SdpItem<'a>>,
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

/// Parse one token into an AST node. An `a=` line becomes an [`Attribute`] (a `Pair` if it contains
/// `:`, else a `Flag`); anything else is kept as a [`SdpItem::Field`].
pub fn parse_item<'a>(line: Line<'a>) -> SdpItem<'a> {
    if line.kind != 'a' {
        return SdpItem::Field {
            kind: line.kind,
            value: line.value,
        };
    }
    match line.value.split_once(':') {
        Some((key, value)) => SdpItem::Attribute(Attribute::Pair {
            key: key.trim(),
            value: value.trim(),
        }),
        None => SdpItem::Attribute(Attribute::Flag(line.value)),
    }
}

/// Parse: tokens ↦ AST.
pub fn parse(sdp: &str) -> SessionDescription<'_> {
    SessionDescription {
        items: lex(sdp).map(parse_item).collect(),
    }
}

impl SessionDescription<'_> {
    /// Fold the AST to the first `max-message-size` attribute value (RFC 8841 §6), if any. A linear
    /// fold over the items selecting the one typed value we care about.
    pub fn max_message_size(&self) -> Option<u32> {
        self.items.iter().find_map(|item| match item {
            SdpItem::Attribute(Attribute::Pair { key, value }) if *key == "max-message-size" => {
                value.parse().ok()
            }
            _ => None,
        })
    }
}

/// Parse the SCTP `a=max-message-size` attribute (RFC 8841) out of an SDP, composing the full
/// lex → parse → fold pipeline. Returns `None` when the attribute is absent or malformed. This is
/// the same source webrtc reads internally; we parse it ourselves so the negotiated limit is
/// available identically on native and browser, without relying on a backend to expose it.
pub fn parse_sdp_max_message_size(sdp: &str) -> Option<u32> {
    parse(sdp).max_message_size()
}

#[cfg(test)]
mod tests {
    use super::lex;
    use super::parse;
    use super::parse_item;
    use super::parse_sdp_max_message_size;
    use super::Attribute;
    use super::Line;
    use super::SdpItem;

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
    fn parse_item_flag_vs_pair() {
        assert_eq!(
            parse_item(Line {
                kind: 'a',
                value: "sendrecv"
            }),
            SdpItem::Attribute(Attribute::Flag("sendrecv")),
        );
        assert_eq!(
            parse_item(Line {
                kind: 'a',
                value: "max-message-size:262144"
            }),
            SdpItem::Attribute(Attribute::Pair {
                key: "max-message-size",
                value: "262144"
            }),
        );
    }

    #[test]
    fn parse_item_non_attribute_is_field() {
        assert_eq!(
            parse_item(Line {
                kind: 'm',
                value: "application 9 UDP/DTLS/SCTP webrtc-datachannel"
            }),
            SdpItem::Field {
                kind: 'm',
                value: "application 9 UDP/DTLS/SCTP webrtc-datachannel"
            },
        );
    }

    #[test]
    fn parse_builds_ordered_ast() {
        let ast = parse("v=0\r\na=max-message-size:65536\r\n");
        assert_eq!(ast.items, vec![
            SdpItem::Field {
                kind: 'v',
                value: "0"
            },
            SdpItem::Attribute(Attribute::Pair {
                key: "max-message-size",
                value: "65536"
            }),
        ]);
    }

    #[test]
    fn fold_absent_attribute_is_none() {
        let sdp = "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=sctp-port:5000\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), None);
    }

    #[test]
    fn fold_explicit_value() {
        let sdp = "a=sctp-port:5000\r\na=max-message-size:262144\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), Some(262144));
    }

    #[test]
    fn fold_zero_is_preserved() {
        assert_eq!(
            parse_sdp_max_message_size("a=max-message-size:0\r\n"),
            Some(0)
        );
    }

    #[test]
    fn fold_tolerates_surrounding_whitespace() {
        assert_eq!(
            parse_sdp_max_message_size("   a=max-message-size: 1200 \r\n"),
            Some(1200)
        );
    }

    #[test]
    fn fold_malformed_value_is_none() {
        assert_eq!(
            parse_sdp_max_message_size("a=max-message-size:notanumber\r\n"),
            None
        );
        assert_eq!(parse_sdp_max_message_size("a=max-message-size:\r\n"), None);
    }

    #[test]
    fn fold_finds_attribute_among_many_lines() {
        let sdp = concat!(
            "v=0\r\n",
            "o=- 0 0 IN IP4 0.0.0.0\r\n",
            "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n",
            "a=max-message-size:65536\r\n",
            "a=setup:actpass\r\n",
        );
        assert_eq!(parse_sdp_max_message_size(sdp), Some(65536));
    }

    #[test]
    fn fold_takes_first_when_repeated() {
        let sdp = "a=max-message-size:1024\r\na=max-message-size:2048\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), Some(1024));
    }
}
