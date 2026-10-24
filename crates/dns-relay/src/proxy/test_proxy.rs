use std::net::SocketAddr;

use ppp::v2::Addresses;
use ppp::v2::Header;

use super::*;

#[test]
fn test_the_header_parses_back_to_its_addresses() {
    let client: SocketAddr = "[2001:db8::7]:40000".parse().unwrap();
    let relayed: SocketAddr = "[2001:db8:1::9]:25".parse().unwrap();
    let header = proxy_v2_header(client, relayed).unwrap();
    let parsed = Header::try_from(header.as_slice()).unwrap();
    assert_eq!(parsed.len(), header.len());
    let Addresses::IPv6(addresses) = parsed.addresses else {
        panic!("IPv6 addresses: {:?}", parsed.addresses);
    };
    assert_eq!(
        SocketAddr::from((addresses.source_address, addresses.source_port)),
        client
    );
    assert_eq!(
        SocketAddr::from((addresses.destination_address, addresses.destination_port)),
        relayed
    );
}
