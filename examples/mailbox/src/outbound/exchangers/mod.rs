//! Where a domain's mail goes: its mail exchangers (RFC 5321 §5.1, RFC 7505).
//!
//! ```text
//!   MX lookup of d                     exchangers(d)
//!   ─────────────────────────────────  ─────────────────────────────────────────────
//!   NOERROR, MX records                the hosts by preference, each resolved
//!   NOERROR, one MX of "."             NullMx: d takes no mail             permanent
//!   NOERROR, no MX record              d itself, resolved (the implicit MX)
//!   NXDOMAIN                           NoDomain: d does not exist          permanent
//!   SERVFAIL, timeout, any other       Lookup: DNS cannot tell now         temporary
//!   hosts resolve to no address        NoAddress                           temporary
//! ```
//!
//! A permanent failure bounces the message at once; a temporary one leaves it queued for the
//! next attempt.

use std::net::SocketAddr;

use async_trait::async_trait;
use hickory_resolver::net::DnsError;
use hickory_resolver::net::NetError;
use hickory_resolver::net::NoRecords;
use hickory_resolver::proto::op::ResponseCode;
use hickory_resolver::proto::rr::RData;
use hickory_resolver::TokioResolver;
use thiserror::Error;

/// The SMTP port mail exchangers listen on.
const SMTP_PORT: u16 = 25;

/// Why a domain's exchangers are not known.
#[derive(Debug, Error)]
pub enum ExchangersError {
    /// The domain does not exist.
    #[error("{0} does not exist")]
    NoDomain(String),
    /// The domain says it takes no mail (a null MX, RFC 7505).
    #[error("{0} accepts no mail")]
    NullMx(String),
    /// DNS cannot tell now.
    #[error("cannot look up the mail exchangers of {domain}: {source}")]
    Lookup {
        /// The domain.
        domain: String,
        /// Why.
        source: NetError,
    },
    /// No exchanger of the domain resolves to an address.
    #[error("no mail exchanger of {0} has an address")]
    NoAddress(String),
}

impl ExchangersError {
    /// Whether asking again later cannot change the answer.
    pub fn is_permanent(&self) -> bool {
        matches!(self, Self::NoDomain(_) | Self::NullMx(_))
    }
}

/// Where a domain's mail exchangers are.
#[async_trait]
pub trait Exchangers: Send + Sync + 'static {
    /// The addresses of `domain`'s mail exchangers, most preferred first.
    async fn exchangers(&self, domain: &str) -> Result<Vec<SocketAddr>, ExchangersError>;
}

/// The hosts `domain`'s MX lookup names, most preferred first (the module doc's table, its
/// address column aside).
pub fn exchange_hosts(
    domain: &str,
    looked_up: Result<Vec<(u16, String)>, NetError>,
) -> Result<Vec<String>, ExchangersError> {
    let mut exchangers = match looked_up {
        Ok(exchangers) => exchangers,
        Err(NetError::Dns(DnsError::NoRecordsFound(NoRecords {
            response_code: ResponseCode::NoError,
            ..
        }))) => return Ok(vec![format!("{domain}.")]),
        Err(NetError::Dns(DnsError::NoRecordsFound(NoRecords {
            response_code: ResponseCode::NXDomain,
            ..
        }))) => return Err(ExchangersError::NoDomain(domain.to_owned())),
        Err(source) => {
            return Err(ExchangersError::Lookup {
                domain: domain.to_owned(),
                source,
            });
        }
    };
    if let [(_preference, host)] = exchangers.as_slice() {
        if host == "." {
            return Err(ExchangersError::NullMx(domain.to_owned()));
        }
    }
    exchangers.sort();
    Ok(exchangers
        .into_iter()
        .filter(|(_preference, host)| host != ".")
        .map(|(_preference, host)| host)
        .collect())
}

/// The exchangers DNS names, found with a resolver.
pub struct DnsExchangers {
    resolver: TokioResolver,
}

impl DnsExchangers {
    /// Exchangers found with the system's resolver configuration.
    pub fn system() -> Result<Self, NetError> {
        let resolver = TokioResolver::builder_tokio()?.build()?;
        Ok(Self { resolver })
    }

    /// The addresses of `host`, IPv6 first.
    async fn addresses(&self, host: &str) -> Vec<SocketAddr> {
        let Ok(found) = self.resolver.lookup_ip(host).await else {
            return Vec::new();
        };
        let mut addresses = found
            .iter()
            .map(|ip| SocketAddr::new(ip, SMTP_PORT))
            .collect::<Vec<_>>();
        addresses.sort_by_key(|address| !address.is_ipv6());
        addresses
    }
}

#[async_trait]
impl Exchangers for DnsExchangers {
    async fn exchangers(&self, domain: &str) -> Result<Vec<SocketAddr>, ExchangersError> {
        let looked_up = self
            .resolver
            .mx_lookup(format!("{domain}."))
            .await
            .map(|lookup| {
                lookup
                    .answers()
                    .iter()
                    .filter_map(|record| match &record.data {
                        RData::MX(mx) => Some((mx.preference, mx.exchange.to_ascii())),
                        _ => None,
                    })
                    .collect()
            });
        let mut addresses = Vec::new();
        for host in exchange_hosts(domain, looked_up)? {
            addresses.extend(self.addresses(&host).await);
        }
        if addresses.is_empty() {
            return Err(ExchangersError::NoAddress(domain.to_owned()));
        }
        Ok(addresses)
    }
}

#[cfg(test)]
mod test_exchangers;
