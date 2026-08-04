//! Streaming `srcset` tokenization shared with the browser runtime contract.

use crate::error::Result;

#[derive(Debug, Eq, PartialEq)]
pub(super) struct SrcsetCandidate {
    pub(super) url: String,
    pub(super) descriptor: String,
}

/// Visit candidates without retaining an attacker-controlled candidate table.
///
/// Law: a comma terminates a descriptor-less URL only when it is trailing in
/// its non-whitespace URL token. Commas inside `data:` payloads remain data.
pub(super) fn visit_srcset_candidates(
    input: &str,
    mut visit: impl FnMut(SrcsetCandidate) -> Result<()>,
) -> Result<()> {
    let mut cursor = 0_usize;
    while cursor < input.len() {
        cursor = skip_separators(input, cursor);
        if cursor == input.len() {
            break;
        }
        let url_start = cursor;
        cursor = scan_url_token(input, cursor);
        let url_end = cursor;
        if url_start == url_end {
            continue;
        }
        if input[..url_end].ends_with(',') {
            let url = normalize_url(input[url_start..url_end].trim_end_matches(','));
            if !url.is_empty() {
                visit(SrcsetCandidate {
                    url,
                    descriptor: String::new(),
                })?;
            }
            continue;
        }
        cursor = skip_whitespace(input, cursor);
        let descriptor_start = cursor;
        cursor = scan_descriptor(input, cursor);
        let url = normalize_url(&input[url_start..url_end]);
        if !url.is_empty() {
            visit(SrcsetCandidate {
                url,
                descriptor: input[descriptor_start..cursor].trim().to_string(),
            })?;
        }
        if input[cursor..].starts_with(',') {
            cursor += 1;
        }
    }
    Ok(())
}

fn skip_separators(input: &str, mut cursor: usize) -> usize {
    while let Some(character) = input[cursor..].chars().next() {
        if !matches!(character, '\t' | '\n' | '\x0c' | '\r' | ' ' | ',') {
            break;
        }
        cursor += character.len_utf8();
    }
    cursor
}

fn skip_whitespace(input: &str, mut cursor: usize) -> usize {
    while let Some(character) = input[cursor..].chars().next() {
        if !matches!(character, '\t' | '\n' | '\x0c' | '\r' | ' ') {
            break;
        }
        cursor += character.len_utf8();
    }
    cursor
}

fn scan_url_token(input: &str, mut cursor: usize) -> usize {
    let mut quote = None;
    let mut escaped = false;
    while let Some(character) = input[cursor..].chars().next() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if quote.is_some_and(|quoted| quoted == character) {
            quote = None;
        } else if quote.is_none() && matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if quote.is_none() && matches!(character, '\t' | '\n' | '\x0c' | '\r' | ' ') {
            break;
        }
        cursor += character.len_utf8();
    }
    cursor
}

fn scan_descriptor(input: &str, mut cursor: usize) -> usize {
    while let Some(character) = input[cursor..].chars().next() {
        if character == ',' {
            break;
        }
        cursor += character.len_utf8();
    }
    cursor
}

fn normalize_url(url: &str) -> String {
    let unquoted = match (url.chars().next(), url.chars().next_back()) {
        (Some('"'), Some('"')) | (Some('\''), Some('\'')) if url.len() >= 2 => {
            &url[1..url.len() - 1]
        }
        _ => url,
    };
    let mut normalized = String::new();
    let mut escaped = false;
    for character in unquoted.chars() {
        if escaped {
            normalized.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            normalized.push(character);
        }
    }
    if escaped {
        normalized.push('\\');
    }
    normalized
}
