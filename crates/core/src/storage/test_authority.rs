//! The authority lattice, table-driven: every variant names its point, every law follows from
//! the point's components, and every point is admissible (durable-records, Law 1).

use super::Crash;
use super::Meaning;
use super::RecordAuthority;

/// Every authority, with its point and the laws it must carry:
/// `(authority, crash, meaning, flushes, evicts, retires_undecodable, witnesses)`.
const TABLE: [(RecordAuthority, Crash, Meaning, bool, bool, bool, bool); 4] = [
    (
        RecordAuthority::Disposable,
        Crash::Cached,
        Meaning::Rebuild,
        false,
        true,
        true,
        false,
    ),
    (
        RecordAuthority::DurableCache,
        Crash::Durable,
        Meaning::Rebuild,
        true,
        true,
        true,
        false,
    ),
    (
        RecordAuthority::Retained,
        Crash::Durable,
        Meaning::Retained,
        true,
        false,
        true,
        false,
    ),
    (
        RecordAuthority::Authoritative,
        Crash::Durable,
        Meaning::Authoritative,
        true,
        false,
        false,
        true,
    ),
];

/// Each variant sits at its point, and carries exactly the laws of that point.
#[test]
fn test_every_authority_carries_the_laws_of_its_point() {
    for (authority, crash, meaning, flushes, evicts, retires, witnesses) in TABLE {
        assert_eq!(authority.components(), (crash, meaning), "{authority:?}");
        assert_eq!(authority.flushes(), flushes, "{authority:?}: flushes");
        assert_eq!(authority.evicts(), evicts, "{authority:?}: evicts");
        assert_eq!(
            authority.retires_undecodable(),
            retires,
            "{authority:?}: decode"
        );
        assert_eq!(authority.witnesses(), witnesses, "{authority:?}: presence");
    }
}

/// Admissibility (Law 1): a meaning above `Rebuild` asserts that an absent record is not a
/// loss, which only a durable store can uphold.
#[test]
fn test_every_authority_is_an_admissible_point() {
    for (authority, ..) in TABLE {
        let (crash, meaning) = authority.components();
        assert!(
            meaning == Meaning::Rebuild || crash == Crash::Durable,
            "{authority:?} is inadmissible"
        );
    }
}

/// The laws are monotone in the lattice: an authority above another in both components keeps
/// at least the obligations of the lower one (it flushes if the lower does, and evicts only if
/// the lower does).
#[test]
fn test_the_laws_are_monotone_in_the_lattice() {
    for (lower, ..) in TABLE {
        for (upper, ..) in TABLE {
            let (lower_crash, lower_meaning) = lower.components();
            let (upper_crash, upper_meaning) = upper.components();
            if lower_crash <= upper_crash && lower_meaning <= upper_meaning {
                assert!(!lower.flushes() || upper.flushes(), "{lower:?} ⊑ {upper:?}");
                assert!(!upper.evicts() || lower.evicts(), "{lower:?} ⊑ {upper:?}");
                assert!(
                    !lower.witnesses() || upper.witnesses(),
                    "{lower:?} ⊑ {upper:?}"
                );
            }
        }
    }
}
