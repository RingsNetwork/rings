//! The authoritative answer: one pure function of where a name lies, what its owner announces,
//! and the record type asked for.
//!
//! ```text
//!   answer(Foreign, _)                     = REFUSED
//!   answer(Apex z, SOA)                    = [soa z]
//!   answer(Apex z, NS)                     = ns z
//!   answer(Absent z, _)                    = NXDOMAIN, authority [soa z]
//!   answer(Owned z o s ⊥, _)               = NXDOMAIN, authority [soa z]      o announces nothing
//!   answer(Owned z o ε a, AAAA)            = [AAAA addr(o)]
//!   answer(Owned z o ε a, MX)              = [MX 10 n]                         if receives_mail(a)
//!   answer(Owned z o s a, TXT)             = txt(a)(s)
//!   answer(Owned z o s a, _)               = NXDOMAIN, authority [soa z]       if ¬exists(a, s)
//!   answer(_, _)                           = NOERROR, no answer, authority [soa z]   otherwise
//! ```
//!
//! Every answer inside a zone is authoritative. A `NOERROR` answer with no records (NODATA) and
//! an `NXDOMAIN` both carry the zone's SOA, whose minimum field is the negative-caching TTL
//! (RFC 2308).
//!
//! `ANY` is answered with one record set (RFC 8482): the SOA at an apex, the address at an
//! owner's name, the TXT records below it. A small question then never draws a large answer, so
//! the relay amplifies no reflected traffic.

use std::net::Ipv6Addr;

use hickory_proto::op::ResponseCode;
use hickory_proto::rr::rdata::AAAA;
use hickory_proto::rr::rdata::MX;
use hickory_proto::rr::rdata::NS;
use hickory_proto::rr::rdata::SOA;
use hickory_proto::rr::rdata::TXT;
use hickory_proto::rr::Name;
use hickory_proto::rr::RData;
use hickory_proto::rr::Record;
use hickory_proto::rr::RecordType;

use crate::announcement::Announcement;
use crate::announcement::SubName;
use crate::zone::Zone;

/// Preference of the MX record of an owner that receives mail: its only mail exchanger.
const MX_PREFERENCE: u16 = 10;
/// Serial of every zone's SOA. The zones change with the owners' descriptors, not by
/// transfer: there is no secondary to tell a new version, so the serial stays constant.
const SOA_SERIAL: u32 = 1;
/// SOA refresh interval, in seconds (unused without secondaries; RFC 1912 range).
const SOA_REFRESH_SECS: i32 = 3_600;
/// SOA retry interval, in seconds (unused without secondaries; RFC 1912 range).
const SOA_RETRY_SECS: i32 = 600;
/// SOA expire interval, in seconds (unused without secondaries; RFC 1912 range).
const SOA_EXPIRE_SECS: i32 = 1_209_600;

/// The TTLs the relay answers with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnswerTtl {
    /// TTL of every positive record, in seconds. It bounds how long a resolver keeps an owner's
    /// address or services after the owner changes them.
    pub positive: u32,
    /// TTL of a negative answer (the SOA minimum), in seconds: how long a resolver remembers
    /// that a name does not exist.
    pub negative: u32,
}

/// What a name's owner announces, as the answer needs it: its announcement and its address.
#[derive(Clone, Copy, Debug)]
pub struct Announced<'a> {
    /// The owner's validated announcement.
    pub announcement: &'a Announcement,
    /// The owner's address, `addr(o)`, bound in the relay's address book.
    pub address: Ipv6Addr,
}

/// Where a queried name lies, resolved against the owners' announcements: the input of
/// [`answer`].
#[derive(Clone, Copy, Debug)]
pub enum View<'a> {
    /// The name lies in no relayed zone.
    Foreign,
    /// The name is a zone's apex.
    Apex(&'a Zone),
    /// The name lies in a zone and names no owner.
    Absent(&'a Zone),
    /// The name is `sub` under an owner's name in `zone`.
    Owned {
        /// The zone the name lies in.
        zone: &'a Zone,
        /// The name under the owner's name.
        sub: &'a SubName,
        /// What the owner announces; `None` when it announces nothing valid.
        announced: Option<Announced<'a>>,
    },
}

/// A response: its code, whether it is authoritative, and its answer and authority sections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Answer {
    /// The response code.
    pub code: ResponseCode,
    /// Whether the relay answers as the zone's authority.
    pub authoritative: bool,
    /// The answer section.
    pub answers: Vec<Record>,
    /// The authority section.
    pub authority: Vec<Record>,
}

impl Answer {
    /// `REFUSED`: the name lies in no relayed zone.
    fn refused() -> Self {
        Self {
            code: ResponseCode::Refused,
            authoritative: false,
            answers: Vec::new(),
            authority: Vec::new(),
        }
    }

    /// An authoritative positive answer of `answers`, or NODATA when it is empty.
    fn records(zone: &Zone, ttl: AnswerTtl, answers: Vec<Record>) -> Self {
        let authority = if answers.is_empty() {
            vec![soa_record(zone, ttl)]
        } else {
            Vec::new()
        };
        Self {
            code: ResponseCode::NoError,
            authoritative: true,
            answers,
            authority,
        }
    }

    /// An authoritative `NXDOMAIN`.
    fn nonexistent(zone: &Zone, ttl: AnswerTtl) -> Self {
        Self {
            code: ResponseCode::NXDomain,
            authoritative: true,
            answers: Vec::new(),
            authority: vec![soa_record(zone, ttl)],
        }
    }
}

/// The answer to a query for `qname` of type `qtype`, which lies as `view` says (the module
/// doc's table).
pub fn answer(view: View<'_>, qname: &Name, qtype: RecordType, ttl: AnswerTtl) -> Answer {
    match view {
        View::Foreign => Answer::refused(),
        View::Absent(zone) => Answer::nonexistent(zone, ttl),
        View::Apex(zone) => Answer::records(zone, ttl, apex_records(zone, qtype, ttl)),
        View::Owned {
            zone,
            announced: None,
            ..
        } => Answer::nonexistent(zone, ttl),
        View::Owned {
            zone,
            sub,
            announced: Some(announced),
        } => {
            if !announced.announcement.names(sub) {
                return Answer::nonexistent(zone, ttl);
            }
            Answer::records(zone, ttl, owned_records(qname, sub, announced, qtype, ttl))
        }
    }
}

/// The record type a query for `qtype` asks for, at a name whose `ANY` answer is the record set
/// of type `for_any` (the module doc).
fn asked(qtype: RecordType, for_any: RecordType) -> RecordType {
    if qtype == RecordType::ANY {
        for_any
    } else {
        qtype
    }
}

/// The records of type `qtype` at `zone`'s apex.
fn apex_records(zone: &Zone, qtype: RecordType, ttl: AnswerTtl) -> Vec<Record> {
    match asked(qtype, RecordType::SOA) {
        RecordType::SOA => vec![soa_record(zone, ttl)],
        RecordType::NS => {
            let origin = Name::from(zone.origin());
            zone.nameservers()
                .map(|nameserver| {
                    Record::from_rdata(
                        origin.clone(),
                        ttl.positive,
                        RData::NS(NS(nameserver.clone())),
                    )
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// The records of type `qtype` at `qname`, the name `sub` under an owner that announces
/// `announced`.
///
/// Pre: `announced.announcement.names(sub)`.
fn owned_records(
    qname: &Name,
    sub: &SubName,
    announced: Announced<'_>,
    qtype: RecordType,
    ttl: AnswerTtl,
) -> Vec<Record> {
    let record = |rdata: RData| Record::from_rdata(qname.clone(), ttl.positive, rdata);
    let for_any = if sub.is_owner() {
        RecordType::AAAA
    } else {
        RecordType::TXT
    };
    match asked(qtype, for_any) {
        RecordType::AAAA if sub.is_owner() => vec![record(RData::AAAA(AAAA(announced.address)))],
        RecordType::MX if sub.is_owner() && announced.announcement.receives_mail() => {
            vec![record(RData::MX(MX::new(MX_PREFERENCE, qname.clone())))]
        }
        RecordType::TXT => announced
            .announcement
            .txt_at(sub)
            .iter()
            .map(|value| record(RData::TXT(TXT::new(vec![value.as_str().to_owned()]))))
            .collect(),
        _ => Vec::new(),
    }
}

/// `zone`'s SOA record, whose minimum is the negative TTL.
fn soa_record(zone: &Zone, ttl: AnswerTtl) -> Record {
    Record::from_rdata(
        Name::from(zone.origin()),
        ttl.negative,
        RData::SOA(SOA::new(
            zone.primary().clone(),
            zone.contact().clone(),
            SOA_SERIAL,
            SOA_REFRESH_SECS,
            SOA_RETRY_SECS,
            SOA_EXPIRE_SECS,
            ttl.negative,
        )),
    )
}

#[cfg(test)]
mod test_authority;
