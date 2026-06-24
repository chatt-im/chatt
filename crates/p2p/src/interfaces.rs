use std::{
    ffi::CStr,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};

use crate::candidate::{Candidate, CandidateKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalInterface {
    pub index: u32,
    pub name: String,
    pub addr: IpAddr,
    pub is_up: bool,
    pub is_loopback: bool,
    pub is_virtual: bool,
}

impl LocalInterface {
    pub fn usable_for_host_candidate(&self, include_loopback: bool) -> bool {
        self.is_up
            && !self.is_virtual
            && (include_loopback || !self.is_loopback)
            && !self.addr.is_unspecified()
            && !is_multicast(self.addr)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterfaceSnapshot {
    interfaces: Vec<LocalInterface>,
}

impl InterfaceSnapshot {
    pub fn capture() -> io::Result<Self> {
        Self::from_interfaces(discover_interfaces()?)
    }

    pub fn from_interfaces(mut interfaces: Vec<LocalInterface>) -> io::Result<Self> {
        interfaces.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.addr.cmp(&b.addr))
                .then_with(|| a.index.cmp(&b.index))
        });
        interfaces.dedup();
        Ok(Self { interfaces })
    }

    pub fn changed_from(&self, previous: &Self) -> bool {
        self.interfaces != previous.interfaces
    }

    pub fn interfaces(&self) -> &[LocalInterface] {
        &self.interfaces
    }
}

pub fn host_candidates(
    port: u16,
    include_loopback: bool,
    next_id: &mut u32,
) -> io::Result<Vec<Candidate>> {
    host_candidates_with_metadata(0, 0, port, include_loopback, next_id, true)
}

pub fn host_candidates_with_metadata(
    socket_id: u32,
    generation: u64,
    port: u16,
    include_loopback: bool,
    next_id: &mut u32,
    prefer_ipv6: bool,
) -> io::Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    for interface in discover_interfaces()? {
        if !interface.usable_for_host_candidate(include_loopback) {
            continue;
        }
        let id = *next_id;
        *next_id = next_id.wrapping_add(1).max(1);
        candidates.push(Candidate::with_metadata(
            id,
            socket_id,
            generation,
            CandidateKind::Host,
            SocketAddr::new(interface.addr, port),
            None,
            true,
            prefer_ipv6,
        ));
    }
    Ok(candidates)
}

pub fn is_virtual_interface_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("tun")
        || lower.starts_with("tap")
        || lower.starts_with("utun")
        || lower.starts_with("wg")
        || lower.starts_with("ppp")
        || lower.starts_with("ipsec")
        || lower.starts_with("tailscale")
        || lower.starts_with("zerotier")
        || lower.starts_with("zt")
        || lower.starts_with("docker")
        || lower.starts_with("br-")
        || lower.starts_with("veth")
        || lower.starts_with("virbr")
        || lower.starts_with("vmnet")
        || lower.starts_with("ham")
}

#[cfg(unix)]
pub fn discover_interfaces() -> io::Result<Vec<LocalInterface>> {
    let mut addrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut addrs) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let mut out = Vec::new();
    let mut cursor = addrs;
    while !cursor.is_null() {
        let ifaddr = unsafe { &*cursor };
        if !ifaddr.ifa_addr.is_null() {
            let name = unsafe { CStr::from_ptr(ifaddr.ifa_name) }
                .to_string_lossy()
                .into_owned();
            if let Some(addr) = sockaddr_ip(ifaddr.ifa_addr) {
                let flags = ifaddr.ifa_flags;
                let index = unsafe { libc::if_nametoindex(ifaddr.ifa_name) };
                out.push(LocalInterface {
                    index,
                    is_up: flags & libc::IFF_UP as u32 != 0,
                    is_loopback: flags & libc::IFF_LOOPBACK as u32 != 0,
                    is_virtual: is_virtual_interface_name(&name),
                    name,
                    addr,
                });
            }
        }
        cursor = ifaddr.ifa_next;
    }
    unsafe { libc::freeifaddrs(addrs) };
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.addr.cmp(&b.addr)));
    out.dedup();
    Ok(out)
}

#[cfg(not(unix))]
pub fn discover_interfaces() -> io::Result<Vec<LocalInterface>> {
    Ok(Vec::new())
}

#[cfg(unix)]
fn sockaddr_ip(addr: *const libc::sockaddr) -> Option<IpAddr> {
    match unsafe { (*addr).sa_family as i32 } {
        libc::AF_INET => {
            let addr = unsafe { &*(addr as *const libc::sockaddr_in) };
            Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                addr.sin_addr.s_addr,
            ))))
        }
        libc::AF_INET6 => {
            let addr = unsafe { &*(addr as *const libc::sockaddr_in6) };
            Some(IpAddr::V6(Ipv6Addr::from(addr.sin6_addr.s6_addr)))
        }
        _ => None,
    }
}

fn is_multicast(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(addr) => addr.is_multicast(),
        IpAddr::V6(addr) => addr.is_multicast(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_interface_names_are_filtered() {
        for name in [
            "tun0",
            "utun4",
            "wg-home",
            "tailscale0",
            "docker0",
            "br-test",
            "veth123",
        ] {
            assert!(is_virtual_interface_name(name), "{name}");
        }
        assert!(!is_virtual_interface_name("eth0"));
        assert!(!is_virtual_interface_name("en0"));
        assert!(!is_virtual_interface_name("wlan0"));
    }

    #[test]
    fn usability_filters_virtual_and_loopback() {
        let physical = LocalInterface {
            index: 1,
            name: "eth0".to_string(),
            addr: "192.168.1.2".parse().unwrap(),
            is_up: true,
            is_loopback: false,
            is_virtual: false,
        };
        let loopback = LocalInterface {
            index: 2,
            name: "lo".to_string(),
            addr: "127.0.0.1".parse().unwrap(),
            is_up: true,
            is_loopback: true,
            is_virtual: false,
        };
        let vpn = LocalInterface {
            index: 3,
            name: "tun0".to_string(),
            addr: "10.8.0.2".parse().unwrap(),
            is_up: true,
            is_loopback: false,
            is_virtual: true,
        };

        assert!(physical.usable_for_host_candidate(false));
        assert!(!loopback.usable_for_host_candidate(false));
        assert!(loopback.usable_for_host_candidate(true));
        assert!(!vpn.usable_for_host_candidate(true));
    }

    #[test]
    fn snapshot_detects_interface_changes() {
        let before = InterfaceSnapshot::from_interfaces(vec![LocalInterface {
            index: 1,
            name: "eth0".to_string(),
            addr: "192.168.1.2".parse().unwrap(),
            is_up: true,
            is_loopback: false,
            is_virtual: false,
        }])
        .unwrap();
        let after = InterfaceSnapshot::from_interfaces(vec![LocalInterface {
            index: 1,
            name: "eth0".to_string(),
            addr: "192.168.1.3".parse().unwrap(),
            is_up: true,
            is_loopback: false,
            is_virtual: false,
        }])
        .unwrap();

        assert!(after.changed_from(&before));
    }
}
