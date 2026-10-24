use std::net::Ipv6Addr;

use hickory_proto::op::ResponseCode;
use hickory_proto::rr::rdata::AAAA;
use hickory_proto::rr::rdata::MX;
use hickory_proto::rr::rdata::NS;
use hickory_proto::rr::rdata::TXT;
use hickory_proto::rr::Name;
use hickory_proto::rr::RData;
use hickory_proto::rr::RecordType;

use super::*;
use crate::announcement::Announcement;
use crate::announcement::ProxyProtocol;
use crate::announcement::SubName;
use crate::announcement::TxtValue;
use crate::announcement::SMTP_PORT;
use crate::zone::Zone;

/// The TTLs of every case.
const TTL: AnswerTtl = AnswerTtl {
    positive: 60,
    negative: 30,
};

/// The address of the owner in every case.
const ADDRESS: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 7);

/// The absolute name `name`.
fn name(name: &str) -> Name {
    let mut name = Name::from_ascii(name).unwrap();
    name.set_fqdn(true);
    name
}

/// The zone `rings.rs.`, served by `ns.example.net.`.
fn zone() -> Zone {
    Zone::new(
        name("rings.rs"),
        name("ns.example.net"),
        vec![],
        name("hostmaster.example.net"),
    )
}

/// An owner serving SMTP and SSH, with a TXT record at `_acme-challenge`.
fn mail_and_ssh() -> Announcement {
    Announcement::new(
        [(SMTP_PORT, ProxyProtocol::Off), (22, ProxyProtocol::Off)],
        [(SubName::new("_acme-challenge").unwrap(), vec![
            TxtValue::new("token").unwrap(),
        ])],
    )
    .unwrap()
}

/// The record data of `answer`'s answer section.
fn rdata(answer: &Answer) -> Vec<RData> {
    answer
        .answers
        .iter()
        .map(|record| record.data.clone())
        .collect()
}

/// Whether `answer` is authoritative with `code`, no answer, and the zone's SOA as authority.
fn is_negative(answer: &Answer, code: ResponseCode) -> bool {
    answer.code == code
        && answer.authoritative
        && answer.answers.is_empty()
        && answer.authority.len() == 1
        && answer
            .authority
            .first()
            .is_some_and(|soa| soa.record_type() == RecordType::SOA && soa.ttl == TTL.negative)
}

#[test]
fn test_foreign_names_are_refused() {
    let answer = answer(View::Foreign, &name("example.org"), RecordType::A, TTL);
    assert_eq!(answer.code, ResponseCode::Refused);
    assert!(!answer.authoritative);
    assert!(answer.answers.is_empty() && answer.authority.is_empty());
}

#[test]
fn test_apex_answers_soa_and_ns_and_nodata_otherwise() {
    let zone = zone();
    let apex = name("rings.rs");
    let soa = answer(View::Apex(&zone), &apex, RecordType::SOA, TTL);
    assert_eq!(soa.code, ResponseCode::NoError);
    assert!(soa.authoritative);
    assert_eq!(
        soa.answers
            .iter()
            .map(|record| record.record_type())
            .collect::<Vec<_>>(),
        vec![RecordType::SOA]
    );
    let ns = answer(View::Apex(&zone), &apex, RecordType::NS, TTL);
    assert_eq!(rdata(&ns), vec![RData::NS(NS(name("ns.example.net")))]);
    let any = answer(View::Apex(&zone), &apex, RecordType::ANY, TTL);
    assert_eq!(
        any.answers
            .iter()
            .map(|record| record.record_type())
            .collect::<Vec<_>>(),
        vec![RecordType::SOA],
        "ANY draws one record set"
    );
    let aaaa = answer(View::Apex(&zone), &apex, RecordType::AAAA, TTL);
    assert!(is_negative(&aaaa, ResponseCode::NoError));
}

#[test]
fn test_absent_and_unannounced_names_do_not_exist() {
    let zone = zone();
    let owner = SubName::owner();
    for view in [View::Absent(&zone), View::Owned {
        zone: &zone,
        sub: &owner,
        announced: None,
    }] {
        let answer = answer(view, &name("o7.rings.rs"), RecordType::AAAA, TTL);
        assert!(is_negative(&answer, ResponseCode::NXDomain));
    }
}

#[test]
fn test_owner_name_answers_its_address_mail_exchanger_and_records() {
    let zone = zone();
    let announcement = mail_and_ssh();
    let owner = SubName::owner();
    let qname = name("o7.rings.rs");
    let view = View::Owned {
        zone: &zone,
        sub: &owner,
        announced: Some(Announced {
            announcement: &announcement,
            address: ADDRESS,
        }),
    };
    let aaaa = answer(view, &qname, RecordType::AAAA, TTL);
    assert_eq!(aaaa.code, ResponseCode::NoError);
    assert!(aaaa.authoritative && aaaa.authority.is_empty());
    assert_eq!(rdata(&aaaa), vec![RData::AAAA(AAAA(ADDRESS))]);
    assert!(aaaa.answers.iter().all(|record| record.ttl == TTL.positive));
    let mx = answer(view, &qname, RecordType::MX, TTL);
    assert_eq!(rdata(&mx), vec![RData::MX(MX::new(10, qname.clone()))]);
    let any = answer(view, &qname, RecordType::ANY, TTL);
    assert_eq!(
        rdata(&any),
        vec![RData::AAAA(AAAA(ADDRESS))],
        "ANY draws one record set: the address"
    );
    for nodata in [RecordType::A, RecordType::TXT, RecordType::NS] {
        let answer = answer(view, &qname, nodata, TTL);
        assert!(is_negative(&answer, ResponseCode::NoError), "{nodata}");
    }
}

#[test]
fn test_owner_without_port_25_has_no_mail_exchanger() {
    let zone = zone();
    let announcement = Announcement::new([(22, ProxyProtocol::Off)], []).unwrap();
    let owner = SubName::owner();
    let view = View::Owned {
        zone: &zone,
        sub: &owner,
        announced: Some(Announced {
            announcement: &announcement,
            address: ADDRESS,
        }),
    };
    let mx = answer(view, &name("o7.rings.rs"), RecordType::MX, TTL);
    assert!(is_negative(&mx, ResponseCode::NoError));
}

#[test]
fn test_names_under_the_owner_answer_their_txt_records_only() {
    let zone = zone();
    let announcement = mail_and_ssh();
    let announced = Some(Announced {
        announcement: &announcement,
        address: ADDRESS,
    });
    let challenge = SubName::new("_acme-challenge").unwrap();
    let qname = name("_acme-challenge.o7.rings.rs");
    let view = View::Owned {
        zone: &zone,
        sub: &challenge,
        announced,
    };
    let txt = answer(view, &qname, RecordType::TXT, TTL);
    assert_eq!(rdata(&txt), vec![RData::TXT(TXT::new(vec![
        "token".to_owned()
    ]))]);
    let aaaa = answer(view, &qname, RecordType::AAAA, TTL);
    assert!(
        is_negative(&aaaa, ResponseCode::NoError),
        "no address below the owner"
    );
    let missing = SubName::new("www").unwrap();
    let absent = answer(
        View::Owned {
            zone: &zone,
            sub: &missing,
            announced,
        },
        &name("www.o7.rings.rs"),
        RecordType::TXT,
        TTL,
    );
    assert!(is_negative(&absent, ResponseCode::NXDomain));
}
