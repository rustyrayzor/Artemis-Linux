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
    let mut hosts = BTreeMap::<(String, u16), DiscoveredHost>::new();

    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let name = info
                    .get_fullname()
                    .trim_end_matches(SERVICE_TYPE)
                    .trim_end_matches('.')
                    .to_owned();
                for address in info.get_addresses().iter().copied().filter(prefer_address) {
                    let host = address.to_string();
                    let port = info.get_port();
                    hosts.insert(
                        (host.clone(), port),
                        DiscoveredHost {
                            name: name.clone(),
                            address: HostAddress::new(host, port),
                        },
                    );
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

fn prefer_address(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(value) => !value.is_loopback() && !value.is_unspecified(),
        IpAddr::V6(value) => !value.is_loopback() && !value.is_unspecified(),
    }
}
