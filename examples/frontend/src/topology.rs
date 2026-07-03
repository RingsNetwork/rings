//! Chord topology inference and rendering.

use web_sys::MouseEvent;
use yew::prelude::*;

use crate::hex;
use crate::node::PeerView;

const CHORD_ID_BYTES: usize = 20;
const CHORD_ID_BITS: usize = CHORD_ID_BYTES * 8;
const CHORD_HEX_CHARS: usize = CHORD_ID_BYTES * 2;

pub(crate) fn view(did: &str, peers: &[PeerView]) -> Html {
    html! {
        <Topology did={did.to_string()} peers={peers.to_vec()} />
    }
}

#[derive(Properties, Clone, PartialEq)]
struct TopologyProps {
    did: String,
    peers: Vec<PeerView>,
}

#[function_component(Topology)]
fn topology_component(props: &TopologyProps) -> Html {
    let width = 420.0;
    let height = 420.0;
    let center_x = width / 2.0;
    let center_y = height / 2.0;
    let radius = 144.0;
    let nodes = chord_nodes(&props.did, &props.peers);
    let successor_edges = inferred_successor_edges(nodes.len());
    let finger_links = inferred_finger_links(&nodes);
    let local_context = local_chord_context(&nodes);
    let outer_orbit = open_orbit_path(center_x, center_y, radius + 42.0);
    let main_orbit = open_orbit_path(center_x, center_y, radius);
    let inner_orbit = open_orbit_path(center_x, center_y, radius - 50.0);
    let node_count = nodes.len();
    let show_remote_labels = node_count <= 6;
    let hovered_node = use_state(|| None::<String>);
    let pinned_node = use_state(|| None::<String>);
    let active_did = (*pinned_node).clone().or_else(|| (*hovered_node).clone());
    let clear_pinned = {
        let pinned_node = pinned_node.clone();
        Callback::from(move |_| pinned_node.set(None))
    };
    let clear_hovered = {
        let hovered_node = hovered_node.clone();
        Callback::from(move |_| hovered_node.set(None))
    };
    html! {
        <svg
            class="topology chord-topology"
            viewBox="0 0 420 420"
            role="img"
            aria-label="inferred Chord identifier ring"
            onclick={clear_pinned}
            onmouseleave={clear_hovered}
        >
            <path class="orbit outer" d={outer_orbit} />
            <path class="orbit" d={main_orbit.clone()} />
            <path class="orbit inner" d={inner_orbit} />
            <path class="scan" d={main_orbit} />
            <text class="topology-mode" x="20" y="30">{ "INFERRED CHORD RING" }</text>
            <text class="topology-count" x="20" y="48">{ format!("{node_count} visible IDs") }</text>
            <text class="ring-zero" x={center_x.to_string()} y="31" text-anchor="middle">{ "0 / 2^160" }</text>
            { for successor_edges.iter().filter_map(|edge| {
                let (source, target) = edge.endpoints(&nodes)?;
                let class = if edge.target == 0 { "ring-edge wrap" } else { "ring-edge" };
                let flow_class = if edge.target == 0 { "data-flow ring-flow wrap" } else { "data-flow ring-flow" };
                let path = ring_arc_path(center_x, center_y, radius, source.angle, target.angle);
                Some(html! {
                    <>
                        <path class={class} d={path.clone()}>
                            <title>{ format!("inferred successor: {} -> {}", source.did, target.did) }</title>
                        </path>
                        <path class={flow_class} d={path} aria-hidden="true" />
                    </>
                })
            })}
            { for finger_links.iter().filter_map(|edge| {
                let (source, target) = edge.endpoints(&nodes)?;
                let tone = if source.is_local {
                    if edge.exponent == 159 { "primary" } else { "local" }
                } else {
                    "remote"
                };
                let class = format!("finger-link {tone}");
                let flow_class = format!("data-flow finger-flow {tone}");
                let flow_delay = format!(
                    "animation-delay: -{}ms;",
                    (edge.source() * 311 + edge.target() * 43 + edge.exponent * 17) % 3600
                );
                let path = finger_curve_path(center_x, center_y, edge.exponent, source.angle, target.angle);
                Some(html! {
                    <>
                        <path class={class} d={path.clone()}>
                            <title>{ format!("inferred finger 2^{}: {} -> {}", edge.exponent, source.did, target.did) }</title>
                        </path>
                        <path class={flow_class} d={path} style={flow_delay} aria-hidden="true" />
                    </>
                })
            })}
            <circle class="id-space-core" cx={center_x.to_string()} cy={center_y.to_string()} r="50" />
            <text class="core-label" x={center_x.to_string()} y={(center_y + 4.0).to_string()} text-anchor="middle">{ "RINGS" }</text>
            {
                if let Some((predecessor, successor)) = local_context {
                    html! {
                        <>
                            { ring_peer_label("predecessor-label", format!("PRED {predecessor}"), center_x, center_y, radius - 112.0) }
                            { ring_peer_label("successor-label", format!("SUCC {successor}"), center_x, center_y, radius - 99.0) }
                        </>
                    }
                } else {
                    html! {}
                }
            }
            { for nodes.iter().enumerate().map(|(index, node)| {
                let (x, y) = polar_point(center_x, center_y, radius, node.angle);
                let (label_x, label_y) = polar_point(center_x, center_y, radius + 31.0, node.angle);
                let node_class = node_class(node);
                let node_radius = node_radius(node, node_count);
                let index_label = if node.is_local { "L".to_string() } else { (index + 1).to_string() };
                let show_index = node.is_local || node_count <= 16;
                let show_label = node.is_local || show_remote_labels;
                let index_size = if node_count > 10 { "8" } else { "10" };
                let is_active = active_did.as_ref().is_some_and(|active| active == &node.did);
                let group_class = if is_active { "topology-node active" } else { "topology-node" };
                let hover_did = node.did.clone();
                let pin_did = node.did.clone();
                let on_mouse_enter = {
                    let hovered_node = hovered_node.clone();
                    Callback::from(move |_| hovered_node.set(Some(hover_did.clone())))
                };
                let on_mouse_leave = {
                    let hovered_node = hovered_node.clone();
                    Callback::from(move |_| hovered_node.set(None))
                };
                let on_click = {
                    let pinned_node = pinned_node.clone();
                    Callback::from(move |event: MouseEvent| {
                        event.stop_propagation();
                        pinned_node.set(Some(pin_did.clone()));
                    })
                };
                html! {
                    <g
                        class={group_class}
                        onmouseenter={on_mouse_enter}
                        onmouseleave={on_mouse_leave}
                        onclick={on_click}
                    >
                        <title>{ format!("{} {}", node.state, node.did) }</title>
                        <circle class={node_class} cx={svg_num(x)} cy={svg_num(y)} r={svg_num(node_radius)} />
                        {
                            if show_index {
                                html! {
                                    <text class="peer-index" x={svg_num(x)} y={svg_num(y + 3.5)} text-anchor="middle" font-size={index_size}>{ index_label }</text>
                                }
                            } else {
                                html! {}
                            }
                        }
                        {
                            if show_label {
                                html! {
                                    <text class={if node.is_local { "node-id local-id" } else { "node-id" }} x={svg_num(label_x)} y={svg_num(label_y)} text-anchor="middle" font-size="9">
                                        { short_did(&node.did) }
                                    </text>
                                }
                            } else {
                                html! {}
                            }
                        }
                    </g>
                }
            })}
            {
                active_did
                    .as_ref()
                    .and_then(|did| nodes.iter().find(|node| &node.did == did))
                    .map(|node| active_node_label(node, center_x, center_y, radius))
                    .unwrap_or_else(|| html! {})
            }
            {
                if nodes.is_empty() {
                    html! {
                        <text class="empty-node-label" x={center_x.to_string()} y={(center_y + radius + 38.0).to_string()} text-anchor="middle" font-size="11">
                            { "waiting for peers" }
                        </text>
                    }
                } else {
                    html! {}
                }
            }
        </svg>
    }
}

#[derive(Clone)]
struct ChordNode {
    did: String,
    state: String,
    id: [u8; CHORD_ID_BYTES],
    angle: f64,
    is_local: bool,
}

struct InferredEdge {
    source: usize,
    target: usize,
}

struct InferredFinger {
    edge: InferredEdge,
    exponent: usize,
}

impl InferredEdge {
    fn endpoints<'a>(&self, nodes: &'a [ChordNode]) -> Option<(&'a ChordNode, &'a ChordNode)> {
        Some((nodes.get(self.source)?, nodes.get(self.target)?))
    }
}

impl InferredFinger {
    fn source(&self) -> usize {
        self.edge.source
    }

    fn target(&self) -> usize {
        self.edge.target
    }

    fn endpoints<'a>(&self, nodes: &'a [ChordNode]) -> Option<(&'a ChordNode, &'a ChordNode)> {
        self.edge.endpoints(nodes)
    }
}

fn chord_nodes(did: &str, peers: &[PeerView]) -> Vec<ChordNode> {
    let mut nodes = Vec::new();
    if let Some(id) = did_identifier(did) {
        nodes.push(ChordNode {
            did: did.to_string(),
            state: "local".to_string(),
            angle: chord_angle(&id),
            id,
            is_local: true,
        });
    }
    for peer in peers {
        if nodes.iter().any(|node| node.did == peer.did()) {
            continue;
        }
        if let Some(id) = did_identifier(peer.did()) {
            nodes.push(ChordNode {
                did: peer.did().to_string(),
                state: peer.state().to_string(),
                angle: chord_angle(&id),
                id,
                is_local: false,
            });
        }
    }
    nodes.sort_by(|left, right| left.id.cmp(&right.id));
    nodes
}

fn inferred_successor_edges(node_count: usize) -> Vec<InferredEdge> {
    if node_count < 2 {
        return Vec::new();
    }
    (0..node_count)
        .map(|source| InferredEdge {
            source,
            target: (source + 1) % node_count,
        })
        .collect()
}

fn inferred_finger_links(nodes: &[ChordNode]) -> Vec<InferredFinger> {
    if nodes.len() < 4 {
        return Vec::new();
    }
    let exponents = if nodes.len() >= 8 {
        vec![159, 158, 157, 156]
    } else {
        vec![159, 158, 157]
    };
    let mut links = Vec::new();
    for (source, source_node) in nodes.iter().enumerate() {
        let mut source_targets = Vec::new();
        for exponent in &exponents {
            let target_id = chord_add_power_of_two(&source_node.id, *exponent);
            let target = first_successor_index(nodes, &target_id);
            if target == source || source_targets.contains(&target) {
                continue;
            }
            links.push(InferredFinger {
                edge: InferredEdge { source, target },
                exponent: *exponent,
            });
            source_targets.push(target);
        }
    }
    links
}

fn local_chord_context(nodes: &[ChordNode]) -> Option<(String, String)> {
    if nodes.len() < 2 {
        return None;
    }
    let local = nodes.iter().position(|node| node.is_local)?;
    let predecessor_index = if local == 0 {
        nodes.len() - 1
    } else {
        local - 1
    };
    let successor_index = (local + 1) % nodes.len();
    let predecessor = nodes.get(predecessor_index)?;
    let successor = nodes.get(successor_index)?;
    Some((predecessor.did.clone(), successor.did.clone()))
}

fn node_class(node: &ChordNode) -> &'static str {
    if node.is_local {
        return "ring-node local-node";
    }
    if node.state.eq_ignore_ascii_case("connected") {
        "ring-node peer-node connected"
    } else {
        "ring-node peer-node"
    }
}

fn node_radius(node: &ChordNode, node_count: usize) -> f64 {
    if node.is_local {
        return if node_count > 16 { 24.0 } else { 28.0 };
    }
    match node_count {
        0..=8 => 21.0,
        9..=16 => 15.0,
        _ => 10.0,
    }
}

fn first_successor_index(nodes: &[ChordNode], target_id: &[u8; CHORD_ID_BYTES]) -> usize {
    nodes
        .iter()
        .position(|node| node.id >= *target_id)
        .unwrap_or(0)
}

fn chord_add_power_of_two(id: &[u8; CHORD_ID_BYTES], exponent: usize) -> [u8; CHORD_ID_BYTES] {
    let mut out = *id;
    if exponent >= CHORD_ID_BITS {
        return out;
    }
    let mut byte_index = CHORD_ID_BYTES - 1 - exponent / 8;
    let mut carry = (1u8 << (exponent % 8)) as u16;
    while carry > 0 {
        let Some(byte) = out.get_mut(byte_index) else {
            break;
        };
        let value = *byte as u16 + carry;
        *byte = value as u8;
        carry = value >> 8;
        if byte_index == 0 {
            break;
        }
        byte_index -= 1;
    }
    out
}

fn did_identifier(did: &str) -> Option<[u8; CHORD_ID_BYTES]> {
    let hex_value = did.trim().strip_prefix("0x").unwrap_or(did.trim());
    if hex_value.is_empty()
        || !hex_value.len().is_multiple_of(2)
        || !hex_value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }

    let low_hex = hex_value
        .as_bytes()
        .rchunks_exact(2)
        .take(CHORD_HEX_CHARS / 2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    let byte_count = low_hex.len();
    let mut id = [0_u8; CHORD_ID_BYTES];
    let offset = CHORD_ID_BYTES - byte_count;
    for (index, pair) in low_hex.into_iter().enumerate() {
        let high = hex::hex_nibble(*pair.first()?)?;
        let low = hex::hex_nibble(*pair.get(1)?)?;
        let byte = id.get_mut(offset + index)?;
        *byte = (high << 4) | low;
    }
    Some(id)
}

fn chord_angle(id: &[u8; CHORD_ID_BYTES]) -> f64 {
    let mut fraction = 0.0;
    let mut scale = 1.0 / 256.0;
    for byte in id.iter().take(8) {
        fraction += *byte as f64 * scale;
        scale /= 256.0;
    }
    fraction * std::f64::consts::TAU - std::f64::consts::FRAC_PI_2
}

fn polar_point(center_x: f64, center_y: f64, radius: f64, angle: f64) -> (f64, f64) {
    (
        center_x + radius * angle.cos(),
        center_y + radius * angle.sin(),
    )
}

fn ring_arc_path(
    center_x: f64,
    center_y: f64,
    radius: f64,
    source_angle: f64,
    target_angle: f64,
) -> String {
    let (source_x, source_y) = polar_point(center_x, center_y, radius, source_angle);
    let (target_x, target_y) = polar_point(center_x, center_y, radius, target_angle);
    let large_arc = if clockwise_delta(source_angle, target_angle) > std::f64::consts::PI {
        1
    } else {
        0
    };
    format!(
        "M {} {} A {} {} 0 {} 1 {} {}",
        svg_num(source_x),
        svg_num(source_y),
        svg_num(radius),
        svg_num(radius),
        large_arc,
        svg_num(target_x),
        svg_num(target_y)
    )
}

fn open_orbit_path(center_x: f64, center_y: f64, radius: f64) -> String {
    let gap = 0.72;
    let top = -std::f64::consts::FRAC_PI_2;
    arc_path(
        center_x,
        center_y,
        radius,
        top + gap / 2.0,
        top - gap / 2.0 + std::f64::consts::TAU,
        true,
    )
}

fn arc_path(
    center_x: f64,
    center_y: f64,
    radius: f64,
    source_angle: f64,
    target_angle: f64,
    sweep: bool,
) -> String {
    let (source_x, source_y) = polar_point(center_x, center_y, radius, source_angle);
    let (target_x, target_y) = polar_point(center_x, center_y, radius, target_angle);
    let delta = if sweep {
        clockwise_delta(source_angle, target_angle)
    } else {
        clockwise_delta(target_angle, source_angle)
    };
    let large_arc = if delta > std::f64::consts::PI { 1 } else { 0 };
    let sweep_flag = if sweep { 1 } else { 0 };
    format!(
        "M {} {} A {} {} 0 {} {} {} {}",
        svg_num(source_x),
        svg_num(source_y),
        svg_num(radius),
        svg_num(radius),
        large_arc,
        sweep_flag,
        svg_num(target_x),
        svg_num(target_y)
    )
}

fn ring_peer_label(
    class_name: &'static str,
    label: String,
    center_x: f64,
    center_y: f64,
    radius: f64,
) -> Html {
    let chars: Vec<char> = label.chars().collect();
    let count = chars.len();
    if count == 0 {
        return html! {};
    }
    let start_angle = 2.78;
    let end_angle = 0.36;
    let denominator = if count > 1 { (count - 1) as f64 } else { 1.0 };
    let group_class = format!("ring-peer-label {class_name}");
    html! {
        <g class={group_class}>
            { for chars.into_iter().enumerate().map(|(index, ch)| {
                let t = index as f64 / denominator;
                let angle = start_angle + (end_angle - start_angle) * t;
                let (x, y) = polar_point(center_x, center_y, radius, angle);
                let rotation = (-angle.cos()).atan2(angle.sin()).to_degrees();
                let transform = format!(
                    "rotate({} {} {})",
                    svg_num(rotation),
                    svg_num(x),
                    svg_num(y)
                );
                html! {
                    <text x={svg_num(x)} y={svg_num(y)} text-anchor="middle" transform={transform}>
                        { ch }
                    </text>
                }
            }) }
        </g>
    }
}

fn active_node_label(node: &ChordNode, center_x: f64, center_y: f64, radius: f64) -> Html {
    let (node_x, node_y) = polar_point(center_x, center_y, radius, node.angle);
    let (raw_x, raw_y) = polar_point(center_x, center_y, radius + 64.0, node.angle);
    let readout_width = (node.did.chars().count() as f64 * 5.2 + 20.0).clamp(180.0, 340.0);
    let readout_x =
        (raw_x - readout_width / 2.0).clamp(18.0, center_x * 2.0 - readout_width - 18.0);
    let label_x = readout_x + readout_width / 2.0;
    let label_y = raw_y.clamp(66.0, center_y * 2.0 - 62.0);

    html! {
        <g class="active-node-readout" pointer-events="none">
            <line
                class="active-node-pointer"
                x1={svg_num(node_x)}
                y1={svg_num(node_y)}
                x2={svg_num(label_x)}
                y2={svg_num(label_y)}
            />
            <rect
                class="active-node-frame"
                x={svg_num(readout_x)}
                y={svg_num(label_y - 13.0)}
                width={svg_num(readout_width)}
                height="22"
                rx="4"
            />
            <text
                class="active-node-id"
                x={svg_num(label_x)}
                y={svg_num(label_y)}
                text-anchor="middle"
            >
                { node.did.clone() }
            </text>
        </g>
    }
}

fn finger_curve_path(
    center_x: f64,
    center_y: f64,
    exponent: usize,
    source_angle: f64,
    target_angle: f64,
) -> String {
    let radius = 144.0;
    let control_radius = match exponent {
        159 => 24.0,
        158 => 52.0,
        _ => 76.0,
    };
    let (source_x, source_y) = polar_point(center_x, center_y, radius - 10.0, source_angle);
    let (target_x, target_y) = polar_point(center_x, center_y, radius - 10.0, target_angle);
    let (control_1_x, control_1_y) = polar_point(center_x, center_y, control_radius, source_angle);
    let (control_2_x, control_2_y) = polar_point(center_x, center_y, control_radius, target_angle);
    format!(
        "M {} {} C {} {}, {} {}, {} {}",
        svg_num(source_x),
        svg_num(source_y),
        svg_num(control_1_x),
        svg_num(control_1_y),
        svg_num(control_2_x),
        svg_num(control_2_y),
        svg_num(target_x),
        svg_num(target_y)
    )
}

fn clockwise_delta(source_angle: f64, target_angle: f64) -> f64 {
    (target_angle - source_angle).rem_euclid(std::f64::consts::TAU)
}

fn svg_num(value: f64) -> String {
    format!("{value:.2}")
}

pub(crate) fn short_did(did: &str) -> String {
    if did.len() <= 14 {
        return did.to_string();
    }
    let prefix: String = did.chars().take(8).collect();
    let suffix: String = did
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}...{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn did_with_high_byte(value: u8) -> String {
        format!("0x{value:02x}{}", "00".repeat(CHORD_ID_BYTES - 1))
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn did_identifier_left_pads_short_hex() {
        let id = did_identifier("0x000aff");
        assert!(id.is_some());
        let id = id.unwrap_or([0; CHORD_ID_BYTES]);

        assert!(id.iter().take(CHORD_ID_BYTES - 3).all(|byte| *byte == 0));
        assert_eq!(
            id.iter()
                .skip(CHORD_ID_BYTES - 3)
                .copied()
                .collect::<Vec<_>>(),
            vec![0, 10, 255]
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn did_identifier_uses_low_160_bits() {
        let did = format!("0xabcd{}", "11".repeat(CHORD_ID_BYTES));
        let id = did_identifier(&did);
        assert!(id.is_some());
        let id = id.unwrap_or([0; CHORD_ID_BYTES]);

        assert!(id.iter().all(|byte| *byte == 0x11));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn did_identifier_rejects_odd_or_non_hex_input() {
        assert!(did_identifier("0x123").is_none());
        assert!(did_identifier("0x00zz").is_none());
        assert!(did_identifier("").is_none());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn successor_edges_form_wrapped_cycle() {
        let edges = inferred_successor_edges(4)
            .into_iter()
            .map(|edge| (edge.source, edge.target))
            .collect::<Vec<_>>();

        assert_eq!(edges, vec![(0, 1), (1, 2), (2, 3), (3, 0)]);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn local_context_uses_sorted_ring_neighbors() {
        let peers = ["0x10", "0x30"]
            .into_iter()
            .filter_map(|did| PeerView::connected(did.to_string()))
            .collect::<Vec<_>>();
        let nodes = chord_nodes("0x20", &peers);

        assert_eq!(
            local_chord_context(&nodes),
            Some(("0x10".to_string(), "0x30".to_string()))
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn local_context_wraps_for_first_ring_node() {
        let peers = ["0x20", "0x30"]
            .into_iter()
            .filter_map(|did| PeerView::connected(did.to_string()))
            .collect::<Vec<_>>();
        let nodes = chord_nodes("0x01", &peers);

        assert_eq!(
            local_chord_context(&nodes),
            Some(("0x30".to_string(), "0x20".to_string()))
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn chord_add_power_of_two_wraps_identifier_space() {
        assert_eq!(
            chord_add_power_of_two(&[0xff; CHORD_ID_BYTES], 0),
            [0; CHORD_ID_BYTES]
        );

        let high_bit = chord_add_power_of_two(&[0; CHORD_ID_BYTES], CHORD_ID_BITS - 1);
        assert_eq!(
            high_bit.to_vec(),
            [vec![0x80], vec![0; CHORD_ID_BYTES - 1]].concat()
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn inferred_finger_links_have_bounded_non_self_targets() {
        let local = did_with_high_byte(0);
        let peers = (1u8..8)
            .filter_map(|index| PeerView::connected(did_with_high_byte(index * 32)))
            .collect::<Vec<_>>();
        let nodes = chord_nodes(&local, &peers);
        let links = inferred_finger_links(&nodes);

        assert!(!links.is_empty());
        assert!(links
            .iter()
            .all(|link| link.source() < nodes.len() && link.target() < nodes.len()));
        assert!(links.iter().all(|link| link.source() != link.target()));
    }
}
