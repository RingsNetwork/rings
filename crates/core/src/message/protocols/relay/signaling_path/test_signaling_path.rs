//! `Law (Retrace)`, `Law (Bound)` and `Law (Distinct)` of the recorded path.

use super::OutboundPath;
use super::RelayPath;
use super::ReturnPath;
use super::SignalingPath;
use crate::consts::MAX_RELAY_HOPS;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;

/// The DID numbered `n`.
fn did(n: u32) -> Did {
    Did::from(n)
}

/// Walk the return `path` from its sender to `destination`: every DID it takes, in order, and
/// the path it arrives with.
fn retrace(path: &ReturnPath, destination: Did) -> Result<(Vec<Did>, SignalingPath)> {
    let mut at = path.return_hop(destination);
    let mut taken = vec![at];
    let mut current = SignalingPath::Return(path.departed());
    while at != destination {
        let next = current
            .return_hop(destination)
            .ok_or(Error::InvalidNextHop)?;
        current = current.forwarded(at)?;
        taken.push(next);
        at = next;
    }
    Ok((taken, current))
}

/// The outbound path of an offer that crossed `relays`, in order.
fn crossed(relays: &[Did]) -> Result<SignalingPath> {
    relays
        .iter()
        .try_fold(SignalingPath::outbound(), |path, relay| {
            path.forwarded(*relay)
        })
}

/// Law (Retrace): an offer from `o` that crossed `r₁ r₂ r₃` is answered along `r₃ r₂ r₁ o`,
/// arrives recording `r₃ r₂ r₁`, and the reply to that reply retraces `r₁ r₂ r₃ d`.
#[test]
fn test_reply_retraces_the_crossed_relays_both_ways() -> Result<()> {
    let (origin, destination) = (did(1), did(9));
    let relays = [did(2), did(3), did(4)];
    let offer = crossed(&relays)?;
    assert_eq!(offer.crossed().hops(), relays);

    let (answer_hops, answer) = retrace(&offer.reply(), origin)?;
    assert_eq!(answer_hops, vec![relays[2], relays[1], relays[0], origin]);
    assert_eq!(answer.crossed().hops(), [relays[2], relays[1], relays[0]]);

    let (counter_hops, _) = retrace(&answer.reply(), destination)?;
    assert_eq!(counter_hops, vec![
        relays[0],
        relays[1],
        relays[2],
        destination
    ]);
    Ok(())
}

/// A direct offer crossed no relay, so its reply goes straight to the origin; an outbound
/// path names no return hop, since the DHT routes it.
#[test]
fn test_reply_to_a_direct_offer_is_direct() {
    let origin = did(1);
    assert_eq!(SignalingPath::outbound().reply().return_hop(origin), origin);
    assert_eq!(SignalingPath::outbound().return_hop(origin), None);
}

/// Law (Bound): neither decoding nor recording admits a path past `MAX_RELAY_HOPS`.
#[test]
fn test_path_is_bounded_by_the_hop_budget() -> Result<()> {
    let full = (0..u32::from(MAX_RELAY_HOPS)).map(did).collect::<Vec<_>>();
    let path = SignalingPath::Outbound(OutboundPath {
        crossed: RelayPath::try_from(full.clone())?,
    });
    assert!(matches!(
        path.forwarded(did(1_000)),
        Err(Error::RelayPathAboveMax(length)) if length == usize::from(MAX_RELAY_HOPS) + 1
    ));
    let mut over = full;
    over.push(did(1_000));
    assert!(matches!(
        RelayPath::try_from(over),
        Err(Error::RelayPathAboveMax(_))
    ));
    Ok(())
}

/// The hops of `path` in their wire form, 20 canonical bytes each.
fn wire_hops(path: &[Did]) -> Vec<[u8; 20]> {
    path.iter()
        .map(|hop| ethereum_types::H160::from(*hop).to_fixed_bytes())
        .collect()
}

/// The encoding (#952): a path of `n` hops is the varint of `n` and each hop's 20 canonical
/// bytes, and it decodes back to the same hops.
#[test]
fn test_a_path_encodes_each_hop_as_its_20_bytes() -> Result<()> {
    for n in [0, 1, 2, u32::from(MAX_RELAY_HOPS)] {
        let hops = (0..n).map(|hop| did(hop + 7)).collect::<Vec<_>>();
        let path = RelayPath::try_from(hops.clone())?;
        let encoded = rings_codec::serialize(&path).map_err(Error::CodecSerialize)?;
        let mut expected = rings_codec::serialize(&n).map_err(Error::CodecSerialize)?;
        expected.extend(wire_hops(&hops).concat());
        assert_eq!(encoded, expected, "{n} hops");
        let decoded: RelayPath =
            rings_codec::deserialize(&encoded).map_err(Error::CodecDeserialize)?;
        assert_eq!(decoded, path);
    }
    Ok(())
}

/// Law (Bound) and Law (Distinct) on the wire: 65 hops, or a repeated hop, do not decode.
#[test]
fn test_wire_path_above_the_bound_does_not_decode() -> Result<()> {
    let at_bound = (0..u32::from(MAX_RELAY_HOPS)).map(did).collect::<Vec<_>>();
    let encoded = rings_codec::serialize(&wire_hops(&at_bound)).map_err(Error::CodecSerialize)?;
    let decoded: RelayPath = rings_codec::deserialize(&encoded).map_err(Error::CodecDeserialize)?;
    assert_eq!(decoded.hops(), at_bound);

    let mut over = at_bound;
    over.push(did(1_000));
    let encoded = rings_codec::serialize(&wire_hops(&over)).map_err(Error::CodecSerialize)?;
    assert!(rings_codec::deserialize::<RelayPath>(&encoded).is_err());

    let repeated = wire_hops(&[did(2), did(3), did(2)]);
    let encoded = rings_codec::serialize(&repeated).map_err(Error::CodecSerialize)?;
    assert!(rings_codec::deserialize::<RelayPath>(&encoded).is_err());
    Ok(())
}

/// Law (Distinct): a repeated hop is refused when decoded, when recorded, and when it names the
/// holding node or the destination.
#[test]
fn test_repeated_or_self_naming_path_is_refused() -> Result<()> {
    let (local, destination) = (did(5), did(9));
    assert!(matches!(
        RelayPath::try_from(vec![did(2), did(3), did(2)]),
        Err(Error::RelayPathRepeatedHop { hop }) if hop == did(2)
    ));
    assert!(matches!(
        crossed(&[did(2), did(3)])?.forwarded(did(2)),
        Err(Error::RelayPathRepeatedHop { .. })
    ));

    let honest = crossed(&[did(2), did(3)])?;
    honest.validate_at(local, destination)?;
    honest.validate_at(destination, destination)?;
    for forged in [crossed(&[did(2), local])?, crossed(&[did(2), destination])?] {
        assert!(matches!(
            forged.validate_at(local, destination),
            Err(Error::RelayPathRepeatedHop { .. })
        ));
    }

    let crossing = SignalingPath::Return(ReturnPath {
        ahead: RelayPath::try_from(vec![did(2), did(3)])?,
        crossed: RelayPath::try_from(vec![did(3)])?,
    });
    assert!(matches!(
        crossing.validate_at(local, destination),
        Err(Error::RelayPathRepeatedHop { hop }) if hop == did(3)
    ));
    Ok(())
}
