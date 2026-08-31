//! Minimal synchronous Windows route binding for the gateway transaction.
//!
//! This deliberately wraps only list/add/delete from the IP Helper API. Route change
//! subscriptions would pull an unrelated channel runtime into the native gateway graph.

use std::io;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::ptr;

use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::Foundation::ERROR_NOT_FOUND;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Foundation::WIN32_ERROR;
use windows_sys::Win32::NetworkManagement::IpHelper::ConvertInterfaceAliasToLuid;
use windows_sys::Win32::NetworkManagement::IpHelper::ConvertInterfaceIndexToLuid;
use windows_sys::Win32::NetworkManagement::IpHelper::ConvertInterfaceLuidToAlias;
use windows_sys::Win32::NetworkManagement::IpHelper::ConvertInterfaceLuidToIndex;
use windows_sys::Win32::NetworkManagement::IpHelper::CreateIpForwardEntry2;
use windows_sys::Win32::NetworkManagement::IpHelper::DeleteIpForwardEntry2;
use windows_sys::Win32::NetworkManagement::IpHelper::FreeMibTable;
use windows_sys::Win32::NetworkManagement::IpHelper::GetBestRoute2;
use windows_sys::Win32::NetworkManagement::IpHelper::GetIpForwardTable2;
use windows_sys::Win32::NetworkManagement::IpHelper::InitializeIpForwardEntry;
use windows_sys::Win32::NetworkManagement::IpHelper::MIB_IPFORWARD_ROW2;
use windows_sys::Win32::NetworkManagement::IpHelper::MIB_IPFORWARD_TABLE2;
use windows_sys::Win32::NetworkManagement::Ndis::NET_LUID_LH;
use windows_sys::Win32::Networking::WinSock::AF_INET;
use windows_sys::Win32::Networking::WinSock::AF_INET6;
use windows_sys::Win32::Networking::WinSock::AF_UNSPEC;
use windows_sys::Win32::Networking::WinSock::IN6_ADDR;
use windows_sys::Win32::Networking::WinSock::IN_ADDR;
use windows_sys::Win32::Networking::WinSock::SOCKADDR_INET;

/// Windows route fields needed by the cross-platform route transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Route {
    destination: IpAddr,
    prefix: u8,
    gateway: Option<IpAddr>,
    if_name: Option<String>,
    if_index: Option<u32>,
    metric: Option<u32>,
    luid: Option<u64>,
}

impl Route {
    pub(crate) fn new(destination: IpAddr, prefix: u8) -> Self {
        Self {
            destination,
            prefix,
            gateway: None,
            if_name: None,
            if_index: None,
            metric: None,
            luid: None,
        }
    }

    pub(crate) fn destination(&self) -> IpAddr {
        self.destination
    }

    pub(crate) fn prefix(&self) -> u8 {
        self.prefix
    }

    pub(crate) fn gateway(&self) -> Option<IpAddr> {
        self.gateway
    }

    pub(crate) fn if_name(&self) -> Option<&String> {
        self.if_name.as_ref()
    }

    pub(crate) fn if_index(&self) -> Option<u32> {
        self.if_index
    }

    pub(crate) fn metric(&self) -> Option<u32> {
        self.metric
    }

    pub(crate) fn luid(&self) -> Option<u64> {
        self.luid
    }

    pub(crate) fn with_gateway(mut self, gateway: IpAddr) -> Self {
        self.gateway = Some(gateway);
        self
    }

    pub(crate) fn with_if_name(mut self, name: String) -> Self {
        self.if_name = Some(name);
        self
    }

    pub(crate) fn with_if_index(mut self, index: u32) -> Self {
        self.if_index = Some(index);
        self
    }

    pub(crate) fn with_metric(mut self, metric: u32) -> Self {
        self.metric = Some(metric);
        self
    }

    pub(crate) fn with_luid(mut self, luid: u64) -> Self {
        self.luid = Some(luid);
        self
    }

    pub(crate) fn contains(&self, target: &IpAddr) -> bool {
        match (self.destination, *target) {
            (IpAddr::V4(network), IpAddr::V4(address)) => {
                masked_v4(network, self.prefix) == masked_v4(address, self.prefix)
            }
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                masked_v6(network, self.prefix) == masked_v6(address, self.prefix)
            }
            _ => false,
        }
    }

    fn checked_index(&self) -> io::Result<Option<u32>> {
        let name_index = self.if_name.as_deref().map(if_name_to_index).transpose()?;
        if let (Some(index), Some(resolved)) = (self.if_index, name_index) {
            if index != resolved {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "route interface name and index disagree",
                ));
            }
        }
        if let Some(index) = self.if_index {
            let _ = index_to_luid(index)?;
        }
        Ok(self.if_index.or(name_index))
    }

    fn validate(&self) -> io::Result<()> {
        let max_prefix = if self.destination.is_ipv4() { 32 } else { 128 };
        if self.prefix > max_prefix {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "route prefix exceeds address width",
            ));
        }
        if let Some(gateway) = self.gateway {
            if gateway.is_ipv4() != self.destination.is_ipv4() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "route gateway and destination families differ",
                ));
            }
        }
        let _ = self.checked_index()?;
        Ok(())
    }
}

/// Synchronous IP Helper route manager used only by the privileged gateway path.
pub(crate) struct RouteManager;

impl RouteManager {
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self)
    }

    pub(crate) fn list(&mut self) -> io::Result<Vec<Route>> {
        let mut table = ptr::null_mut::<MIB_IPFORWARD_TABLE2>();
        // SAFETY: IP Helper initializes `table` on success; the returned allocation is wrapped
        // immediately in `OwnedRouteTable`, whose Drop calls `FreeMibTable` exactly once.
        let status = unsafe { GetIpForwardTable2(AF_UNSPEC, &mut table) };
        status_to_result(status)?;
        if table.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "GetIpForwardTable2 returned a null table",
            ));
        }
        let owned = OwnedRouteTable(table);
        // SAFETY: `owned.0` is a successful IP Helper allocation. Its flexible array contains
        // exactly `NumEntries` initialized `MIB_IPFORWARD_ROW2` values for this allocation.
        let rows = unsafe {
            let count = usize::try_from((*owned.0).NumEntries).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "route table is too large")
            })?;
            let first = ptr::addr_of!((*owned.0).Table).cast::<MIB_IPFORWARD_ROW2>();
            std::slice::from_raw_parts(first, count)
        };
        Ok(rows.iter().filter_map(row_to_route).collect())
    }

    pub(crate) fn find_route(&mut self, destination: &IpAddr) -> io::Result<Option<Route>> {
        // SAFETY: all structures are valid writable values for the duration of the call. The
        // interface and source pointers are null by contract so Windows chooses the effective
        // route and source exactly as it would for an unconstrained connection.
        unsafe {
            let mut row: MIB_IPFORWARD_ROW2 = std::mem::zeroed();
            let mut destination_address: SOCKADDR_INET = std::mem::zeroed();
            let mut best_source_address: SOCKADDR_INET = std::mem::zeroed();
            write_sockaddr(&mut destination_address, *destination);
            let status = GetBestRoute2(
                ptr::null(),
                0,
                ptr::null(),
                &destination_address,
                0,
                &mut row,
                &mut best_source_address,
            );
            status_to_result(status)?;
            Ok(row_to_route(&row))
        }
    }

    pub(crate) fn add(&mut self, route: &Route) -> io::Result<()> {
        let row = route_to_row(route)?;
        // SAFETY: `row` is fully initialized by `route_to_row` for CreateIpForwardEntry2.
        status_to_result(unsafe { CreateIpForwardEntry2(&row) })
    }

    pub(crate) fn delete(&mut self, route: &Route) -> io::Result<()> {
        let row = route_to_row(route)?;
        // SAFETY: `row` is fully initialized by `route_to_row` for DeleteIpForwardEntry2.
        status_to_result(unsafe { DeleteIpForwardEntry2(&row) })
    }
}

struct OwnedRouteTable(*mut MIB_IPFORWARD_TABLE2);

impl Drop for OwnedRouteTable {
    fn drop(&mut self) {
        // SAFETY: the pointer was returned by `GetIpForwardTable2`, remains owned here, and this
        // Drop implementation is its sole release path.
        unsafe { FreeMibTable(self.0.cast()) };
    }
}

fn status_to_result(status: WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else if status == ERROR_NOT_FOUND || status == ERROR_FILE_NOT_FOUND {
        Err(io::Error::from(io::ErrorKind::NotFound))
    } else {
        Err(io::Error::from_raw_os_error(status.cast_signed()))
    }
}

fn row_to_route(row: &MIB_IPFORWARD_ROW2) -> Option<Route> {
    let destination = sockaddr_to_ip(&row.DestinationPrefix.Prefix)?;
    let gateway = sockaddr_to_ip(&row.NextHop);
    let if_name = if_index_to_name(row.InterfaceIndex).ok();
    Some(Route {
        destination,
        prefix: row.DestinationPrefix.PrefixLength,
        gateway,
        if_name,
        if_index: Some(row.InterfaceIndex),
        metric: Some(row.Metric),
        luid: Some(luid_to_u64(row.InterfaceLuid)),
    })
}

fn route_to_row(route: &Route) -> io::Result<MIB_IPFORWARD_ROW2> {
    route.validate()?;
    // SAFETY: `InitializeIpForwardEntry` requires a writable row and supplies Windows defaults;
    // zero initialization is the documented precursor for this C structure.
    let mut row: MIB_IPFORWARD_ROW2 = unsafe { std::mem::zeroed() };
    // SAFETY: `row` is a valid writable `MIB_IPFORWARD_ROW2` for the duration of this call.
    unsafe { InitializeIpForwardEntry(&mut row) };

    if let Some(index) = route.checked_index()? {
        row.InterfaceIndex = index;
    }
    if let Some(luid) = route.luid {
        row.InterfaceLuid = u64_to_luid(luid);
    }
    write_sockaddr(&mut row.DestinationPrefix.Prefix, route.destination);
    row.DestinationPrefix.PrefixLength = route.prefix;
    match route.gateway {
        Some(gateway) => write_sockaddr(&mut row.NextHop, gateway),
        None => set_sockaddr_family(&mut row.NextHop, route.destination),
    }
    if let Some(metric) = route.metric {
        row.Metric = metric;
    }
    Ok(row)
}

fn sockaddr_to_ip(address: &SOCKADDR_INET) -> Option<IpAddr> {
    // SAFETY: `si_family` is the shared discriminant of SOCKADDR_INET. The matching union member
    // is read only after checking that discriminant, and IN_ADDR/IN6_ADDR are plain byte storage.
    unsafe {
        match address.si_family {
            AF_INET => Some(IpAddr::V4(Ipv4Addr::from(std::mem::transmute::<
                IN_ADDR,
                [u8; 4],
            >(
                address.Ipv4.sin_addr
            )))),
            AF_INET6 => Some(IpAddr::V6(Ipv6Addr::from(std::mem::transmute::<
                IN6_ADDR,
                [u8; 16],
            >(
                address.Ipv6.sin6_addr
            )))),
            _ => None,
        }
    }
}

fn write_sockaddr(address: &mut SOCKADDR_INET, ip: IpAddr) {
    // SAFETY: the family discriminant and corresponding union member are written together. Both
    // Windows address structures are byte-for-byte representations of the supplied octets.
    unsafe {
        match ip {
            IpAddr::V4(ip) => {
                address.si_family = AF_INET;
                address.Ipv4.sin_family = AF_INET;
                address.Ipv4.sin_addr = std::mem::transmute::<[u8; 4], IN_ADDR>(ip.octets());
            }
            IpAddr::V6(ip) => {
                address.si_family = AF_INET6;
                address.Ipv6.sin6_family = AF_INET6;
                address.Ipv6.sin6_addr = std::mem::transmute::<[u8; 16], IN6_ADDR>(ip.octets());
            }
        }
    }
}

fn set_sockaddr_family(address: &mut SOCKADDR_INET, family_source: IpAddr) {
    address.si_family = if family_source.is_ipv4() {
        AF_INET
    } else {
        AF_INET6
    };
}

fn if_name_to_index(name: &str) -> io::Result<u32> {
    let encoded = name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: the buffer is NUL-terminated and lives through the API call; `luid` is writable.
    let mut luid: NET_LUID_LH = unsafe { std::mem::zeroed() };
    // SAFETY: pointers reference the valid buffers described above.
    status_to_result(unsafe { ConvertInterfaceAliasToLuid(encoded.as_ptr(), &mut luid) })?;
    luid_to_index(&luid)
}

fn if_index_to_name(index: u32) -> io::Result<String> {
    let luid = index_to_luid(index)?;
    let mut alias = vec![0_u16; 257];
    // SAFETY: `luid` is initialized and `alias` exposes its valid writable capacity to Windows.
    status_to_result(unsafe {
        ConvertInterfaceLuidToAlias(&luid, alias.as_mut_ptr(), alias.len())
    })?;
    let end = alias
        .iter()
        .position(|code_unit| *code_unit == 0)
        .unwrap_or(alias.len());
    let prefix = match alias.get(..end) {
        Some(prefix) => prefix,
        None => &[],
    };
    Ok(String::from_utf16_lossy(prefix))
}

fn index_to_luid(index: u32) -> io::Result<NET_LUID_LH> {
    // SAFETY: `luid` is writable and initialized by Windows on successful return.
    let mut luid: NET_LUID_LH = unsafe { std::mem::zeroed() };
    // SAFETY: `luid` is a valid output pointer for this call.
    status_to_result(unsafe { ConvertInterfaceIndexToLuid(index, &mut luid) })?;
    Ok(luid)
}

fn luid_to_index(luid: &NET_LUID_LH) -> io::Result<u32> {
    let mut index = 0;
    // SAFETY: `luid` is initialized and `index` is a valid writable output pointer.
    status_to_result(unsafe { ConvertInterfaceLuidToIndex(luid, &mut index) })?;
    Ok(index)
}

fn luid_to_u64(luid: NET_LUID_LH) -> u64 {
    // SAFETY: NET_LUID_LH is the Windows SDK's transparent 64-bit LUID union.
    unsafe { std::mem::transmute::<NET_LUID_LH, u64>(luid) }
}

fn u64_to_luid(luid: u64) -> NET_LUID_LH {
    // SAFETY: NET_LUID_LH is the Windows SDK's transparent 64-bit LUID union.
    unsafe { std::mem::transmute::<u64, NET_LUID_LH>(luid) }
}

fn masked_v4(address: Ipv4Addr, prefix: u8) -> u32 {
    let shift = 32_u32.saturating_sub(u32::from(prefix));
    let mask = u32::MAX.checked_shl(shift).unwrap_or(0);
    u32::from(address) & mask
}

fn masked_v6(address: Ipv6Addr, prefix: u8) -> u128 {
    let shift = 128_u32.saturating_sub(u32::from(prefix));
    let mask = u128::MAX.checked_shl(shift).unwrap_or(0);
    u128::from(address) & mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sockaddr_round_trip_preserves_both_address_families() {
        for address in [
            "203.0.113.7".parse().expect("test IPv4 address"),
            "2001:db8::7".parse().expect("test IPv6 address"),
        ] {
            // SAFETY: SOCKADDR_INET is plain address storage and is initialized before reading.
            let mut sockaddr: SOCKADDR_INET = unsafe { std::mem::zeroed() };
            write_sockaddr(&mut sockaddr, address);
            assert_eq!(sockaddr_to_ip(&sockaddr), Some(address));
        }
    }

    #[test]
    fn route_contains_masks_v4_and_v6_prefixes() {
        let ipv4 = Route::new("192.0.2.0".parse().expect("test route"), 24);
        assert!(ipv4.contains(&"192.0.2.99".parse().expect("test address")));
        assert!(!ipv4.contains(&"192.0.3.1".parse().expect("test address")));

        let ipv6 = Route::new("2001:db8::".parse().expect("test route"), 32);
        assert!(ipv6.contains(&"2001:db8:1::1".parse().expect("test address")));
        assert!(!ipv6.contains(&"2001:db9::1".parse().expect("test address")));
    }

    #[test]
    fn route_validation_rejects_invalid_prefix_and_mixed_gateway_family() {
        assert!(Route::new("192.0.2.0".parse().expect("test route"), 33)
            .validate()
            .is_err());
        assert!(Route::new("2001:db8::".parse().expect("test route"), 129)
            .validate()
            .is_err());
        assert!(Route::new("192.0.2.0".parse().expect("test route"), 24)
            .with_gateway("2001:db8::1".parse().expect("test gateway"))
            .validate()
            .is_err());
    }

    #[test]
    fn luid_storage_round_trip_preserves_every_bit() {
        for luid in [0, 1, u64::MAX, 0x0123_4567_89ab_cdef] {
            assert_eq!(luid_to_u64(u64_to_luid(luid)), luid);
        }
    }
}
