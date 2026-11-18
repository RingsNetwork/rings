//! The host candidates of a native peer connection beyond the addresses it binds: configured
//! external addresses, set as the ICE agent's 1:1 NAT addresses or, for loopback addresses,
//! appended to the local SDP as copies of its host candidates.

use std::net::IpAddr;

/// The number of ICE candidate lines in `sdp`.
pub(super) fn sdp_candidate_count(sdp: &str) -> usize {
    sdp.lines()
        .filter(|line| line.starts_with("a=candidate:"))
        .count()
}

/// The configured external addresses: the comma-separated `external_address`, trimmed,
/// without blanks, each kept once in first-seen order.
pub(super) fn external_address_candidates(external_address: Option<&str>) -> Vec<String> {
    let Some(external_address) = external_address else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    for candidate in external_address.split(',') {
        let candidate = candidate.trim();
        if candidate.is_empty() || candidates.iter().any(|seen| seen == candidate) {
            continue;
        }
        candidates.push(candidate.to_string());
    }
    candidates
}

/// Whether `candidate` is a loopback IP address.
fn is_loopback_address(candidate: &str) -> bool {
    candidate
        .parse::<IpAddr>()
        .map(|addr| addr.is_loopback())
        .unwrap_or(false)
}

/// The candidates set as the ICE agent's 1:1 NAT addresses: the non-loopback ones.
pub(super) fn nat_1to1_host_candidates(candidates: &[String]) -> Vec<String> {
    candidates
        .iter()
        .filter(|candidate| !is_loopback_address(candidate))
        .cloned()
        .collect()
}

/// The candidates appended to the SDP only, not set as 1:1 NAT addresses: the loopback ones.
pub(super) fn sdp_extra_host_candidates(candidates: &[String]) -> Vec<String> {
    candidates
        .iter()
        .filter(|candidate| is_loopback_address(candidate))
        .cloned()
        .collect()
}

/// Copies of the host candidate `line`, one per address of `extra_addresses` other than its
/// own, each with that address in place of its own; nothing for any other line.
fn duplicate_host_candidate_line(line: &str, extra_addresses: &[String]) -> Vec<String> {
    if !line.starts_with("a=candidate:") {
        return Vec::new();
    }

    let fields = line.split_whitespace().collect::<Vec<_>>();
    let Some(address) = fields.get(4) else {
        return Vec::new();
    };
    let Some(candidate_type_marker) = fields.get(6) else {
        return Vec::new();
    };
    let Some(candidate_type) = fields.get(7) else {
        return Vec::new();
    };
    if *candidate_type_marker != "typ" || *candidate_type != "host" {
        return Vec::new();
    }

    extra_addresses
        .iter()
        .filter(|candidate| candidate.as_str() != *address)
        .filter_map(|candidate| {
            let mut duplicate = fields
                .iter()
                .map(|field| field.to_string())
                .collect::<Vec<_>>();
            let address_slot = duplicate.get_mut(4)?;
            *address_slot = candidate.clone();
            Some(duplicate.join(" "))
        })
        .collect()
}

/// Whether a local network interface named `name` is a virtual or container bridge interface
/// (`docker*`, `br-*`, `veth*`, `virbr*`, `cni*`, `flannel*`, `cali*`, `tunl*`, `wg*`) whose UDP
/// sockets cannot receive direct inbound Internet traffic and therefore must not be rewritten into
/// public `typ host` candidates by `SettingEngine::set_nat_1to1_ips` (#1290).
pub(super) fn is_excluded_virtual_interface(name: &str) -> bool {
    let trimmed = name.trim();
    const EXCLUDED_PREFIXES: [&str; 9] = [
        "docker", "br-", "veth", "virbr", "cni", "flannel", "cali", "tunl", "wg",
    ];
    EXCLUDED_PREFIXES.iter().any(|prefix| {
        trimmed
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    })
}

/// Strip all `a=candidate:` lines from `sdp` while keeping the rest of the session description
/// (including `a=ice-ufrag:`, `a=ice-pwd:`, `a=fingerprint:`, and media sections) intact, and
/// return the stripped SDP together with the extracted candidate attribute values (`candidate:...`
/// without the leading `a=`).
///
/// Passing the candidate-free offer to `RTCPeerConnection::set_remote_description` starts local
/// answer gathering immediately without letting `webrtc-rs` fire outbound UDP STUN connectivity
/// checks at the remote peer's `srflx` NAT port before the HTTP `answerOffer` response has
/// returned to the browser (#1290).
pub(super) fn split_remote_offer_candidates(sdp: &str) -> (String, Vec<String>) {
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

/// `sdp` with every host candidate line followed by its copies for `extra_addresses`, each
/// copy ending as its line does.
pub(super) fn append_sdp_extra_host_candidates(sdp: String, extra_addresses: &[String]) -> String {
    if extra_addresses.is_empty() {
        return sdp;
    }

    let mut output = String::with_capacity(sdp.len());
    for segment in sdp.split_inclusive('\n') {
        output.push_str(segment);

        let line = segment.trim_end_matches(['\r', '\n']);
        let line_ending = if segment.ends_with("\r\n") {
            "\r\n"
        } else if segment.ends_with('\n') {
            "\n"
        } else {
            ""
        };
        for duplicate in duplicate_host_candidate_line(line, extra_addresses) {
            output.push_str(&duplicate);
            output.push_str(line_ending);
        }
    }

    output
}
