use hickory_proto::rr::LowerName;
use hickory_proto::rr::Name;

use super::*;
use crate::owner::test_owner::TestOwner;

/// The absolute name `name`.
fn name(name: &str) -> Name {
    let mut name = Name::from_ascii(name).unwrap();
    name.set_fqdn(true);
    name
}

/// A zone with origin `origin`, served by `ns.example.net.`.
fn zone(origin: &str) -> Zone {
    Zone::new(
        name(origin),
        vec![name("ns.example.net")],
        name("hostmaster.example.net"),
    )
}

/// The placement of `query` among `zones`.
fn place<'z>(zones: &'z Zones, query: &str) -> Placement<'z, TestOwner> {
    zones.place(&LowerName::new(&name(query)))
}

#[test]
fn test_place_finds_apex_owner_sub_and_absence() {
    let zones = Zones::new(vec![zone("rings.rs"), zone("p2p.example.com")]).unwrap();
    let rings = zones.iter().next().unwrap();
    let p2p = zones.iter().nth(1).unwrap();
    let owned = |zone, owner, sub: &str| Placement::Owned {
        zone,
        owner: TestOwner(owner),
        sub: SubName::new(sub).unwrap(),
    };
    let cases = [
        ("example.org", Placement::Foreign),
        ("rs", Placement::Foreign),
        ("rings.rs", Placement::Apex(rings)),
        ("RINGS.rs", Placement::Apex(rings)),
        ("o7.rings.rs", owned(rings, 7, "")),
        ("O7.Rings.RS", owned(rings, 7, "")),
        (
            "_acme-challenge.o7.rings.rs",
            owned(rings, 7, "_acme-challenge"),
        ),
        ("a.b.o7.p2p.example.com", owned(p2p, 7, "a.b")),
        ("www.rings.rs", Placement::Absent(rings)),
        ("o07.rings.rs", Placement::Absent(rings)),
        ("a.b.c.d.e.o7.rings.rs", Placement::Absent(rings)),
    ];
    for (query, placement) in cases {
        assert_eq!(place(&zones, query), placement, "{query}");
    }
    // A label outside a sub-name's alphabet, which `Name::from_ascii` would not parse.
    let mut spaced = Name::from_labels([&b"x y"[..], b"o7", b"rings", b"rs"]).unwrap();
    spaced.set_fqdn(true);
    assert_eq!(
        zones.place::<TestOwner>(&LowerName::new(&spaced)),
        Placement::Absent(rings)
    );
}

#[test]
fn test_zones_refuse_what_cannot_be_served() {
    assert_eq!(Zones::new(vec![]), Err(ZoneError::Empty));
    assert_eq!(
        Zones::new(vec![zone("rings.rs"), zone("id.rings.rs")]),
        Err(ZoneError::Overlap {
            inner: "id.rings.rs.".to_owned(),
            outer: "rings.rs.".to_owned(),
        })
    );
    assert_eq!(
        Zones::new(vec![zone("id.rings.rs"), zone("rings.rs")]),
        Err(ZoneError::Overlap {
            inner: "id.rings.rs.".to_owned(),
            outer: "rings.rs.".to_owned(),
        })
    );
    assert_eq!(
        Zones::new(vec![Zone::new(
            name("rings.rs"),
            vec![],
            name("h.example.net")
        )]),
        Err(ZoneError::NoNameserver("rings.rs.".to_owned()))
    );
    assert_eq!(
        Zones::new(vec![Zone::new(
            name("rings.rs"),
            vec![name("ns.rings.rs")],
            name("h.example.net"),
        )]),
        Err(ZoneError::NameserverInZone {
            nameserver: "ns.rings.rs.".to_owned(),
            zone: "rings.rs.".to_owned(),
        })
    );
}
