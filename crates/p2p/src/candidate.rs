use std::net::{IpAddr, SocketAddr};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NatKind {
    Unknown,
    Cone,
    Symmetric,
}

impl NatKind {
    pub fn is_symmetric(self) -> bool {
        matches!(self, Self::Symmetric)
    }

    pub fn is_cone(self) -> bool {
        matches!(self, Self::Cone)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CandidateKind {
    Host,
    ServerReflexive,
    PeerReflexive,
    PortMapped,
    Relay,
}

impl CandidateKind {
    pub fn type_preference(self) -> u32 {
        match self {
            Self::Host => 126,
            Self::PeerReflexive => 110,
            Self::PortMapped => 105,
            Self::ServerReflexive => 100,
            Self::Relay => 0,
        }
    }

    pub fn is_direct(self) -> bool {
        !matches!(self, Self::Relay)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NetworkFamily {
    Ipv4,
    Ipv6,
}

impl NetworkFamily {
    pub fn of(addr: SocketAddr) -> Self {
        if addr.is_ipv4() {
            Self::Ipv4
        } else {
            Self::Ipv6
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub id: u32,
    pub socket_id: u32,
    pub generation: u64,
    pub kind: CandidateKind,
    pub addr: SocketAddr,
    pub base: Option<SocketAddr>,
    pub priority: u32,
    pub foundation: String,
    pub verified: bool,
}

impl Candidate {
    pub fn new(id: u32, kind: CandidateKind, addr: SocketAddr) -> Self {
        Self::with_base(id, kind, addr, None)
    }

    pub fn with_base(
        id: u32,
        kind: CandidateKind,
        addr: SocketAddr,
        base: Option<SocketAddr>,
    ) -> Self {
        Self::with_metadata(
            id,
            0,
            0,
            kind,
            addr,
            base,
            matches!(kind, CandidateKind::Host),
        )
    }

    pub fn with_metadata(
        id: u32,
        socket_id: u32,
        generation: u64,
        kind: CandidateKind,
        addr: SocketAddr,
        base: Option<SocketAddr>,
        verified: bool,
    ) -> Self {
        let local_preference = match NetworkFamily::of(addr) {
            NetworkFamily::Ipv4 => 65_535,
            NetworkFamily::Ipv6 => 65_534,
        };
        let component = 1;
        let priority = (kind.type_preference() << 24) | (local_preference << 8) | (256 - component);
        let foundation = format!("{}-{}", kind_name(kind), NetworkFamily::of(addr).name());
        Self {
            id,
            socket_id,
            generation,
            kind,
            addr,
            base,
            priority,
            foundation,
            verified,
        }
    }

    pub fn relay(addr: SocketAddr) -> Self {
        Self::new(0, CandidateKind::Relay, addr)
    }

    pub fn family(&self) -> NetworkFamily {
        NetworkFamily::of(self.addr)
    }

    pub fn is_direct(&self) -> bool {
        self.kind.is_direct()
    }

    pub fn is_unspecified(&self) -> bool {
        self.addr.ip().is_unspecified()
    }

    pub fn can_pair_with(&self, remote: &Candidate) -> bool {
        self.family() == remote.family()
            && self.is_direct()
            && remote.is_direct()
            && !self.is_unspecified()
            && !remote.is_unspecified()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CandidatePairId {
    pub local_id: u32,
    pub remote_id: u32,
}

pub fn pair_priority(local: &Candidate, remote: &Candidate, controlling: bool) -> u64 {
    let (g, d) = if controlling {
        (local.priority, remote.priority)
    } else {
        (remote.priority, local.priority)
    };
    ((1_u64 << 32) - 1) * u64::from(g.min(d)) + 2 * u64::from(g.max(d)) + u64::from(g > d)
}

pub fn port_guess_candidates(
    next_id: &mut u32,
    remote: &Candidate,
    limit: usize,
    max_delta: u16,
) -> Vec<Candidate> {
    if !matches!(
        remote.kind,
        CandidateKind::ServerReflexive | CandidateKind::PeerReflexive | CandidateKind::PortMapped
    ) {
        return Vec::new();
    }
    let IpAddr::V4(ip) = remote.addr.ip() else {
        return Vec::new();
    };

    let mut out = Vec::with_capacity(limit);
    for delta in delta_sequence(max_delta) {
        let Some(port) = offset_port(remote.addr.port(), delta) else {
            continue;
        };
        if port == remote.addr.port() {
            continue;
        }
        let id = *next_id;
        *next_id = next_id.wrapping_add(1).max(1);
        let addr = SocketAddr::new(IpAddr::V4(ip), port);
        out.push(Candidate::with_base(
            id,
            CandidateKind::PeerReflexive,
            addr,
            Some(remote.addr),
        ));
        if out.len() == limit {
            break;
        }
    }
    out
}

fn delta_sequence(max_delta: u16) -> Vec<i32> {
    let mut values = Vec::with_capacity(max_delta as usize * 2);
    for value in 1..=i32::from(max_delta) {
        values.push(value);
        values.push(-value);
    }
    values
}

fn offset_port(port: u16, delta: i32) -> Option<u16> {
    let next = i32::from(port) + delta;
    (1..=65_535).contains(&next).then_some(next as u16)
}

fn kind_name(kind: CandidateKind) -> &'static str {
    match kind {
        CandidateKind::Host => "host",
        CandidateKind::ServerReflexive => "srflx",
        CandidateKind::PeerReflexive => "prflx",
        CandidateKind::PortMapped => "map",
        CandidateKind::Relay => "relay",
    }
}

impl NetworkFamily {
    fn name(self) -> &'static str {
        match self {
            Self::Ipv4 => "udp4",
            Self::Ipv6 => "udp6",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_candidates_pair_only_with_same_family() {
        let local_v4 = Candidate::new(1, CandidateKind::Host, "192.168.1.2:5000".parse().unwrap());
        let remote_v4 = Candidate::new(2, CandidateKind::Host, "192.168.1.3:5001".parse().unwrap());
        let remote_v6 = Candidate::new(
            3,
            CandidateKind::Host,
            "[2001:db8::1]:5001".parse().unwrap(),
        );
        let relay = Candidate::new(4, CandidateKind::Relay, "203.0.113.1:5002".parse().unwrap());

        assert!(local_v4.can_pair_with(&remote_v4));
        assert!(!local_v4.can_pair_with(&remote_v6));
        assert!(!local_v4.can_pair_with(&relay));
    }

    #[test]
    fn port_guesses_are_limited_and_sequential() {
        let mut next_id = 20;
        let remote = Candidate::new(
            9,
            CandidateKind::ServerReflexive,
            "198.51.100.10:55000".parse().unwrap(),
        );
        let guesses = port_guess_candidates(&mut next_id, &remote, 4, 4);
        let ports = guesses
            .iter()
            .map(|candidate| candidate.addr.port())
            .collect::<Vec<_>>();
        assert_eq!(ports, vec![55001, 54999, 55002, 54998]);
        assert_eq!(next_id, 24);
    }

    #[test]
    fn port_guesses_do_not_wrap_to_invalid_ports() {
        let mut next_id = 1;
        let remote = Candidate::new(
            9,
            CandidateKind::ServerReflexive,
            "198.51.100.10:1".parse().unwrap(),
        );
        let guesses = port_guess_candidates(&mut next_id, &remote, 4, 4);
        let ports = guesses
            .iter()
            .map(|candidate| candidate.addr.port())
            .collect::<Vec<_>>();
        assert_eq!(ports, vec![2, 3, 4, 5]);
    }
}
