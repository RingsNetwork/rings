use hickory_resolver::proto::op::Query;
use hickory_resolver::proto::rr::Name;
use hickory_resolver::proto::rr::RecordType;

use super::*;

/// The lookup failure "no records", answered with `code`.
fn no_records(code: ResponseCode) -> NetError {
    let query = Query::query(Name::from_ascii("example.com.").unwrap(), RecordType::MX);
    NetError::Dns(DnsError::NoRecordsFound(NoRecords::new(query, code)))
}

/// The MX records `records`, as a lookup returns them.
fn mx(records: &[(u16, &str)]) -> Result<Vec<(u16, String)>, NetError> {
    Ok(records
        .iter()
        .map(|(preference, host)| (*preference, (*host).to_owned()))
        .collect())
}

#[test]
fn test_exchangers_go_by_preference() {
    let hosts = exchange_hosts("example.com", mx(&[(20, "b."), (10, "a."), (30, "c.")]));
    assert_eq!(hosts.unwrap(), vec!["a.", "b.", "c."]);
}

#[test]
fn test_a_domain_without_mx_is_its_own_exchanger() {
    let hosts = exchange_hosts("example.com", Err(no_records(ResponseCode::NoError)));
    assert_eq!(hosts.unwrap(), vec!["example.com."]);
}

#[test]
fn test_a_missing_domain_and_a_null_mx_fail_for_good() {
    for looked_up in [Err(no_records(ResponseCode::NXDomain)), mx(&[(0, ".")])] {
        let failed = exchange_hosts("example.com", looked_up).unwrap_err();
        assert!(failed.is_permanent(), "{failed}");
    }
}

#[test]
fn test_a_server_failure_is_tried_again_not_resolved_as_the_domain() {
    let failed = exchange_hosts("example.com", Err(no_records(ResponseCode::ServFail)));
    let failed = failed.unwrap_err();
    assert!(matches!(failed, ExchangersError::Lookup { .. }), "{failed}");
    assert!(!failed.is_permanent());
}
