use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};

use crate::{Error, HostAddress, Result};

const SERVICE_TYPE: &str = "_nvstream._tcp.local.";

/// One local-network host reported over mDNS.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredHost {
    pub name: String,
    pub address: HostAddress,
}

/// Browses for GameStream-compatible hosts for a bounded period.
///
/// # Errors
///
/// Returns an error when the mDNS daemon cannot start or begin browsing.
pub fn discover(timeout: Duration) -> Result<Vec<DiscoveredHost>> {
    let daemon = ServiceDaemon::new().map_err(|error| Error::Discovery(error.to_string()))?;
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .map_err(|error| Error::Discovery(error.to_string()))?;
    let deadline = Instant::now() + timeout;
    let mut hosts = BTreeMap::<String, DiscoveredHost>::new();

    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let name = info
                    .get_fullname()
                    .trim_end_matches(SERVICE_TYPE)
                    .trim_end_matches('.')
                    .to_owned();
                if let Some(address) = info
                    .get_addresses()
                    .iter()
                    .copied()
                    .filter(prefer_address)
                    .min_by_key(address_preference)
                {
                    let host = address.to_string();
                    let port = info.get_port();
                    let candidate = DiscoveredHost {
                        name: name.clone(),
                        address: HostAddress::new(host, port),
                    };
                    hosts
                        .entry(name)
                        .and_modify(|current| {
                            if host_preference(&candidate.address)
                                < host_preference(&current.address)
                            {
                                current.clone_from(&candidate);
                            }
                        })
                        .or_insert(candidate);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
    Ok(hosts.into_values().collect())
}

fn address_preference(address: &IpAddr) -> u8 {
    match address {
        IpAddr::V4(value) if value.octets()[0..2] == [192, 168] => 0,
        IpAddr::V4(value) if value.is_private() && !value.is_link_local() => 1,
        IpAddr::V4(value) if !value.is_link_local() => 2,
        IpAddr::V6(value) if value.is_unique_local() => 3,
        IpAddr::V6(value) if !value.is_unicast_link_local() => 4,
        IpAddr::V4(_) | IpAddr::V6(_) => 5,
    }
}

fn host_preference(address: &HostAddress) -> u8 {
    address
        .host
        .parse()
        .map_or(u8::MAX, |address| address_preference(&address))
}

fn prefer_address(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(value) => !value.is_loopback() && !value.is_unspecified(),
        IpAddr::V6(value) => !value.is_loopback() && !value.is_unspecified(),
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::address_preference;

    #[test]
    fn local_ipv4_is_preferred_for_a_multi_interface_host() {
        let local_lan = IpAddr::V4(Ipv4Addr::new(192, 168, 100, 20));
        let virtual_adapter = IpAddr::V4(Ipv4Addr::new(172, 18, 0, 1));
        let link_local = IpAddr::V6("fe80::1".parse::<Ipv6Addr>().expect("IPv6 address"));

        assert!(address_preference(&local_lan) < address_preference(&virtual_adapter));
        assert!(address_preference(&virtual_adapter) < address_preference(&link_local));
    }
}
