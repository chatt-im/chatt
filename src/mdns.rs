//! Minimal multicast DNS responder and one-shot resolver for `.local` host
//! candidate privacy.
//!
//! The responder answers A/AAAA queries for the session's own randomly named
//! `{token}.local` host candidates. The resolver issues a one-shot query for a
//! peer's `.local` candidate and yields the resolved address. Both share one
//! IPv4 and one IPv6 socket bound to the standard mDNS port, joining the link
//! local multicast group. All resolution is best effort: a name that does not
//! resolve within the timeout is simply dropped, and the connection falls back
//! to reflexive and relay candidates.

use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket},
    time::{Duration, Instant},
};

use chatt_p2p::{
    interfaces::discover_interfaces,
    socket::{UdpSocketOptions, bind_udp_socket},
};
use mio::{Interest, Registry, Token, net::UdpSocket as MioUdpSocket};
use ring::rand::SecureRandom;
use rpc::evented::recv_datagram_with;

const MDNS_PORT: u16 = 5353;
const MDNS_GROUP_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_GROUP_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
const RESOLVE_TIMEOUT: Duration = Duration::from_millis(500);
const RECORD_TTL_SECS: u32 = 120;
const READ_BUF_LEN: usize = 9000;
const TOKEN_BYTES: usize = 16;

const QTYPE_A: u16 = 1;
const QTYPE_AAAA: u16 = 28;
const QTYPE_ANY: u16 = 255;
const QCLASS_IN: u16 = 1;

const FLAG_RESPONSE: u16 = 0x8000;
const CLASS_CACHE_FLUSH: u16 = 0x8000;
const QU_BIT: u16 = 0x8000;

/// Generates a random unguessable `{token}.local` name for a host candidate.
pub fn generate_mdns_name(rng: &dyn SecureRandom) -> Option<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    rng.fill(&mut bytes).ok()?;
    let mut name = String::with_capacity(TOKEN_BYTES * 2 + 6);
    for byte in bytes {
        push_hex(&mut name, byte);
    }
    name.push_str(".local");
    Some(name)
}

fn push_hex(out: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(HEX[(byte >> 4) as usize] as char);
    out.push(HEX[(byte & 0x0f) as usize] as char);
}

/// A shared mDNS socket pair that responds for local names and resolves remote
/// `.local` candidates.
pub struct MdnsSystem {
    v4: Option<MioUdpSocket>,
    v6: Option<MioUdpSocket>,
    v6_scope: u32,
    v4_token: Token,
    v6_token: Token,
    local_names: HashMap<String, IpAddr>,
    active_queries: HashMap<String, Instant>,
}

#[derive(Debug, Default)]
pub struct MdnsReadOutcome {
    pub resolved: Vec<(String, IpAddr)>,
    pub hit_limit: bool,
}

impl MdnsSystem {
    /// Binds the IPv4 and IPv6 mDNS sockets and joins the multicast group.
    /// Returns a system with whichever families bound successfully. A family
    /// that fails to bind is left disabled rather than fatal.
    pub fn bind() -> Self {
        let v4 = bind_group_v4();
        let (v6, v6_scope) = bind_group_v6();
        Self {
            v4: v4.map(MioUdpSocket::from_std),
            v6: v6.map(MioUdpSocket::from_std),
            v6_scope,
            v4_token: Token(usize::MAX),
            v6_token: Token(usize::MAX),
            local_names: HashMap::new(),
            active_queries: HashMap::new(),
        }
    }

    /// Returns an inert system with no sockets, for sessions with p2p
    /// disabled. [`Self::rebind`] arms it if p2p is enabled at runtime.
    pub fn unbound() -> Self {
        Self {
            v4: None,
            v6: None,
            v6_scope: 0,
            v4_token: Token(usize::MAX),
            v6_token: Token(usize::MAX),
            local_names: HashMap::new(),
            active_queries: HashMap::new(),
        }
    }

    /// Whether any mDNS socket is currently bound.
    pub fn is_bound(&self) -> bool {
        self.v4.is_some() || self.v6.is_some()
    }

    /// Registers the bound sockets with the worker's mio poll. On an unbound
    /// system this only records the tokens for a later [`Self::rebind`].
    pub fn register(
        &mut self,
        registry: &Registry,
        v4_token: Token,
        v6_token: Token,
    ) -> io::Result<()> {
        self.v4_token = v4_token;
        self.v6_token = v6_token;
        if let Some(socket) = self.v4.as_mut() {
            registry.register(socket, v4_token, Interest::READABLE)?;
        }
        if let Some(socket) = self.v6.as_mut() {
            registry.register(socket, v6_token, Interest::READABLE)?;
        }
        Ok(())
    }

    /// Binds fresh sockets and registers them under the tokens recorded by
    /// [`Self::register`], keeping published names and active queries.
    ///
    /// # Errors
    ///
    /// Returns the first registration failure; a family that fails to bind is
    /// left disabled rather than fatal, matching [`Self::bind`].
    pub fn rebind(&mut self, registry: &Registry) -> io::Result<()> {
        let v4 = bind_group_v4();
        let (v6, v6_scope) = bind_group_v6();
        self.v4 = v4.map(MioUdpSocket::from_std);
        self.v6 = v6.map(MioUdpSocket::from_std);
        self.v6_scope = v6_scope;
        if let Some(socket) = self.v4.as_mut() {
            registry.register(socket, self.v4_token, Interest::READABLE)?;
        }
        if let Some(socket) = self.v6.as_mut() {
            registry.register(socket, self.v6_token, Interest::READABLE)?;
        }
        Ok(())
    }

    /// Deregisters and drops the sockets and clears the responder names and
    /// in-flight queries, so LAN multicast traffic no longer wakes the loop.
    pub fn shutdown(&mut self, registry: &Registry) {
        if let Some(mut socket) = self.v4.take() {
            let _ = registry.deregister(&mut socket);
        }
        if let Some(mut socket) = self.v6.take() {
            let _ = registry.deregister(&mut socket);
        }
        self.local_names.clear();
        self.active_queries.clear();
    }

    /// Replaces the responder's name table atomically. Called on each publish
    /// and on ICE restart so a name is never a long lived identifier.
    pub fn publish_names<I>(&mut self, names: I)
    where
        I: IntoIterator<Item = (String, IpAddr)>,
    {
        self.local_names = names
            .into_iter()
            .map(|(name, ip)| (name.to_ascii_lowercase(), ip))
            .collect();
    }

    /// Starts a one-shot resolution for a `.local` host name, sending an A query
    /// over IPv4 and an AAAA query over IPv6.
    pub fn start_resolve(&mut self, name: &str, now: Instant) {
        let lower = name.to_ascii_lowercase();
        if !rpc::control::is_valid_mdns_candidate_name(&lower) {
            return;
        }
        if self.active_queries.contains_key(&lower) {
            return;
        }
        if let Some(socket) = self.v4.as_ref() {
            let query = serialize_query(&lower, QTYPE_A);
            let dest = SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP_V4, MDNS_PORT));
            let _ = socket.send_to(&query, dest);
        }
        if let Some(socket) = self.v6.as_ref() {
            let query = serialize_query(&lower, QTYPE_AAAA);
            let dest = SocketAddr::V6(SocketAddrV6::new(
                MDNS_GROUP_V6,
                MDNS_PORT,
                0,
                self.v6_scope,
            ));
            let _ = socket.send_to(&query, dest);
        }
        self.active_queries.insert(lower, now);
    }

    /// Drains all pending datagrams on the given socket. Answers queries for
    /// names in the local table and returns any completed resolutions.
    pub fn handle_readable(
        &mut self,
        token: Token,
        _now: Instant,
        budget: usize,
    ) -> MdnsReadOutcome {
        let mut outcome = MdnsReadOutcome::default();
        if budget == 0 {
            outcome.hit_limit = true;
            return outcome;
        }
        let is_v4 = if token == self.v4_token {
            true
        } else if token == self.v6_token {
            false
        } else {
            return outcome;
        };
        let mut buf = [0u8; READ_BUF_LEN];
        let mut datagrams = 0usize;
        loop {
            let (len, src) = match self.recv_one(is_v4, &mut buf) {
                Ok(Some(value)) => value,
                Ok(None) => break,
                Err(error) => {
                    kvlog::warn!("mdns receive failed", error = %error);
                    break;
                }
            };
            datagrams += 1;
            let packet = &buf[..len];
            if let Some(queries) = parse_queries(packet) {
                for query in &queries {
                    if let Some((reply, dest)) = self.build_response(query, is_v4, src) {
                        self.send_to(is_v4, &reply, dest);
                    }
                }
            }
            if let Some(answers) = parse_responses(packet) {
                self.collect_resolutions(answers, &mut outcome.resolved);
            }
            if datagrams >= budget {
                outcome.hit_limit = true;
                break;
            }
        }
        outcome
    }

    /// Time until the oldest in-flight query times out, if any.
    pub fn next_timeout(&self, now: Instant) -> Option<Duration> {
        let oldest = self.active_queries.values().min()?;
        Some((*oldest + RESOLVE_TIMEOUT).saturating_duration_since(now))
    }

    /// Drops queries that exceeded the resolution timeout, returning their names.
    pub fn handle_timeout(&mut self, now: Instant) -> Vec<String> {
        let mut expired = Vec::new();
        self.active_queries.retain(|name, sent_at| {
            if now.duration_since(*sent_at) >= RESOLVE_TIMEOUT {
                expired.push(name.clone());
                false
            } else {
                true
            }
        });
        expired
    }

    fn recv_one(&self, is_v4: bool, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        let socket = if is_v4 {
            self.v4.as_ref()
        } else {
            self.v6.as_ref()
        };
        let Some(socket) = socket else {
            return Ok(None);
        };
        recv_datagram_with(buf, |buf| socket.recv_from(buf))
    }

    fn send_to(&self, is_v4: bool, bytes: &[u8], dest: SocketAddr) {
        let socket = if is_v4 {
            self.v4.as_ref()
        } else {
            self.v6.as_ref()
        };
        if let Some(socket) = socket {
            let _ = socket.send_to(bytes, dest);
        }
    }

    fn build_response(
        &self,
        query: &ParsedQuery,
        is_v4: bool,
        src: SocketAddr,
    ) -> Option<(Vec<u8>, SocketAddr)> {
        let ip = *self.local_names.get(&query.name)?;
        let family_matches = (ip.is_ipv4() && is_v4) || (ip.is_ipv6() && !is_v4);
        if !family_matches {
            return None;
        }
        let type_matches = match query.qtype {
            QTYPE_ANY => true,
            QTYPE_A => ip.is_ipv4(),
            QTYPE_AAAA => ip.is_ipv6(),
            _ => false,
        };
        if !type_matches {
            return None;
        }
        let unicast = query.unicast_response || src.port() != MDNS_PORT;
        let id = if unicast { query.id } else { 0 };
        let bytes = serialize_response(id, &query.name, ip);
        let dest = if unicast {
            src
        } else if is_v4 {
            SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP_V4, MDNS_PORT))
        } else {
            SocketAddr::V6(SocketAddrV6::new(
                MDNS_GROUP_V6,
                MDNS_PORT,
                0,
                self.v6_scope,
            ))
        };
        Some((bytes, dest))
    }

    fn collect_resolutions(
        &mut self,
        answers: Vec<(String, IpAddr)>,
        out: &mut Vec<(String, IpAddr)>,
    ) {
        let mut grouped: HashMap<String, Vec<IpAddr>> = HashMap::new();
        for (name, ip) in answers {
            grouped.entry(name).or_default().push(ip);
        }
        for (name, ips) in grouped {
            if !self.active_queries.contains_key(&name) {
                continue;
            }
            // mDNS-ICE single-address constraint: a name resolving to more than
            // one address is a scanning or routing hazard, so ignore it.
            if ips.len() == 1 {
                out.push((name.clone(), ips[0]));
            }
            self.active_queries.remove(&name);
        }
    }
}

fn bind_group_v4() -> Option<UdpSocket> {
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, MDNS_PORT));
    let options = UdpSocketOptions {
        ignore_icmp_errors: false,
        ..UdpSocketOptions::default()
    };
    let socket = match bind_udp_socket(addr, options) {
        Ok(socket) => socket,
        Err(error) => {
            kvlog::warn!("mdns ipv4 bind failed", error = %error);
            return None;
        }
    };
    let _ = socket.set_multicast_loop_v4(true);
    let mut joined = socket
        .join_multicast_v4(&MDNS_GROUP_V4, &Ipv4Addr::UNSPECIFIED)
        .is_ok();
    for addr in interface_addrs_v4() {
        if socket.join_multicast_v4(&MDNS_GROUP_V4, &addr).is_ok() {
            joined = true;
        }
    }
    if !joined {
        kvlog::warn!("mdns ipv4 multicast join failed");
    }
    Some(socket)
}

fn bind_group_v6() -> (Option<UdpSocket>, u32) {
    let addr = SocketAddr::from((Ipv6Addr::UNSPECIFIED, MDNS_PORT));
    let options = UdpSocketOptions {
        ignore_icmp_errors: false,
        ..UdpSocketOptions::default()
    };
    let socket = match bind_udp_socket(addr, options) {
        Ok(socket) => socket,
        Err(error) => {
            kvlog::warn!("mdns ipv6 bind failed", error = %error);
            return (None, 0);
        }
    };
    let _ = socket.set_multicast_loop_v6(true);
    let indices = interface_indices_v6();
    let mut joined = false;
    for index in &indices {
        if socket.join_multicast_v6(&MDNS_GROUP_V6, *index).is_ok() {
            joined = true;
        }
    }
    if !joined {
        kvlog::warn!("mdns ipv6 multicast join failed");
    }
    let scope = indices.first().copied().unwrap_or(0);
    (Some(socket), scope)
}

fn interface_addrs_v4() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    let Ok(interfaces) = discover_interfaces() else {
        return out;
    };
    for interface in interfaces {
        if !interface.is_up || interface.is_virtual {
            continue;
        }
        if let IpAddr::V4(addr) = interface.addr {
            out.push(addr);
        }
    }
    out
}

fn interface_indices_v6() -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(interfaces) = discover_interfaces() else {
        return out;
    };
    for interface in interfaces {
        if !interface.is_up || interface.is_virtual {
            continue;
        }
        if interface.addr.is_ipv6() && !out.contains(&interface.index) {
            out.push(interface.index);
        }
    }
    out
}

struct ParsedQuery {
    id: u16,
    name: String,
    qtype: u16,
    unicast_response: bool,
}

struct DnsReader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> DnsReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn read_u16(&mut self) -> Option<u16> {
        let end = self.offset.checked_add(2)?;
        if end > self.data.len() {
            return None;
        }
        let value = u16::from_be_bytes([self.data[self.offset], self.data[self.offset + 1]]);
        self.offset = end;
        Some(value)
    }

    fn read_u32(&mut self) -> Option<u32> {
        let end = self.offset.checked_add(4)?;
        if end > self.data.len() {
            return None;
        }
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(&self.data[self.offset..end]);
        self.offset = end;
        Some(u32::from_be_bytes(bytes))
    }

    fn skip(&mut self, len: usize) -> Option<()> {
        let end = self.offset.checked_add(len)?;
        if end > self.data.len() {
            return None;
        }
        self.offset = end;
        Some(())
    }

    fn read_name(&mut self) -> Option<String> {
        let mut name = String::new();
        let mut cursor = self.offset;
        let mut pointers = 0;
        let mut advance = true;
        loop {
            if cursor >= self.data.len() {
                return None;
            }
            let len = self.data[cursor];
            if len == 0 {
                if advance {
                    self.offset = cursor + 1;
                }
                break;
            }
            match len & 0xc0 {
                0x00 => {
                    let start = cursor + 1;
                    let end = start.checked_add(len as usize)?;
                    if end > self.data.len() {
                        return None;
                    }
                    let label = std::str::from_utf8(&self.data[start..end]).ok()?;
                    if !name.is_empty() {
                        name.push('.');
                    }
                    name.push_str(&label.to_ascii_lowercase());
                    cursor = end;
                }
                0xc0 => {
                    if cursor + 2 > self.data.len() {
                        return None;
                    }
                    let pointer = (u16::from_be_bytes([self.data[cursor], self.data[cursor + 1]])
                        & 0x3fff) as usize;
                    if advance {
                        self.offset = cursor + 2;
                        advance = false;
                    }
                    pointers += 1;
                    if pointers > 10 {
                        return None;
                    }
                    cursor = pointer;
                }
                _ => return None,
            }
        }
        Some(name)
    }
}

fn parse_queries(packet: &[u8]) -> Option<Vec<ParsedQuery>> {
    let mut reader = DnsReader::new(packet);
    let id = reader.read_u16()?;
    let flags = reader.read_u16()?;
    let qdcount = reader.read_u16()?;
    let _ancount = reader.read_u16()?;
    let _nscount = reader.read_u16()?;
    let _arcount = reader.read_u16()?;
    if flags & FLAG_RESPONSE != 0 {
        return None;
    }
    let mut queries = Vec::new();
    for _ in 0..qdcount {
        let name = reader.read_name()?;
        let qtype = reader.read_u16()?;
        let qclass = reader.read_u16()?;
        if qclass & 0x7fff != QCLASS_IN {
            continue;
        }
        queries.push(ParsedQuery {
            id,
            name,
            qtype,
            unicast_response: qclass & QU_BIT != 0,
        });
    }
    Some(queries)
}

fn parse_responses(packet: &[u8]) -> Option<Vec<(String, IpAddr)>> {
    let mut reader = DnsReader::new(packet);
    let _id = reader.read_u16()?;
    let flags = reader.read_u16()?;
    let qdcount = reader.read_u16()?;
    let ancount = reader.read_u16()?;
    let _nscount = reader.read_u16()?;
    let _arcount = reader.read_u16()?;
    if flags & FLAG_RESPONSE == 0 {
        return None;
    }
    for _ in 0..qdcount {
        let _ = reader.read_name()?;
        let _ = reader.read_u16()?;
        let _ = reader.read_u16()?;
    }
    let mut answers = Vec::new();
    for _ in 0..ancount {
        let name = reader.read_name()?;
        let rtype = reader.read_u16()?;
        let _rclass = reader.read_u16()?;
        let _ttl = reader.read_u32()?;
        let rdlen = reader.read_u16()? as usize;
        if rtype == QTYPE_A && rdlen == 4 {
            let end = reader.offset + 4;
            if end <= reader.data.len() {
                let mut bytes = [0u8; 4];
                bytes.copy_from_slice(&reader.data[reader.offset..end]);
                answers.push((name, IpAddr::V4(Ipv4Addr::from(bytes))));
            }
        } else if rtype == QTYPE_AAAA && rdlen == 16 {
            let end = reader.offset + 16;
            if end <= reader.data.len() {
                let mut bytes = [0u8; 16];
                bytes.copy_from_slice(&reader.data[reader.offset..end]);
                answers.push((name, IpAddr::V6(Ipv6Addr::from(bytes))));
            }
        }
        reader.skip(rdlen)?;
    }
    Some(answers)
}

fn encode_name(buf: &mut Vec<u8>, name: &str) {
    for label in name.split('.') {
        if label.is_empty() {
            continue;
        }
        buf.push(label.len() as u8);
        buf.extend_from_slice(label.as_bytes());
    }
    buf.push(0);
}

fn serialize_query(name: &str, qtype: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&1u16.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    encode_name(&mut buf, name);
    buf.extend_from_slice(&qtype.to_be_bytes());
    buf.extend_from_slice(&(QCLASS_IN | QU_BIT).to_be_bytes());
    buf
}

fn serialize_response(id: u16, name: &str, ip: IpAddr) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(&id.to_be_bytes());
    buf.extend_from_slice(&(FLAG_RESPONSE | 0x0400).to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&1u16.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    encode_name(&mut buf, name);
    match ip {
        IpAddr::V4(ipv4) => {
            buf.extend_from_slice(&QTYPE_A.to_be_bytes());
            buf.extend_from_slice(&(QCLASS_IN | CLASS_CACHE_FLUSH).to_be_bytes());
            buf.extend_from_slice(&RECORD_TTL_SECS.to_be_bytes());
            buf.extend_from_slice(&4u16.to_be_bytes());
            buf.extend_from_slice(&ipv4.octets());
        }
        IpAddr::V6(ipv6) => {
            buf.extend_from_slice(&QTYPE_AAAA.to_be_bytes());
            buf.extend_from_slice(&(QCLASS_IN | CLASS_CACHE_FLUSH).to_be_bytes());
            buf.extend_from_slice(&RECORD_TTL_SECS.to_be_bytes());
            buf.extend_from_slice(&16u16.to_be_bytes());
            buf.extend_from_slice(&ipv6.octets());
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_name_is_valid_local() {
        let rng = ring::rand::SystemRandom::new();
        let name = generate_mdns_name(&rng).unwrap();
        assert!(name.ends_with(".local"));
        assert!(rpc::control::is_valid_mdns_candidate_name(&name));
        assert_eq!(name.len(), TOKEN_BYTES * 2 + ".local".len());
    }

    #[test]
    fn interrupted_io_errors_are_retryable() {
        assert!(rpc::evented::is_interrupted_io_error(&io::Error::from(
            io::ErrorKind::Interrupted
        )));
        assert!(rpc::evented::is_interrupted_io_error(
            &io::Error::from_raw_os_error(libc::EINTR)
        ));
        assert!(!rpc::evented::is_interrupted_io_error(&io::Error::from(
            io::ErrorKind::WouldBlock
        )));
    }

    #[test]
    fn query_round_trips_through_parser() {
        let query = serialize_query("abc123.local", QTYPE_A);
        let parsed = parse_queries(&query).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "abc123.local");
        assert_eq!(parsed[0].qtype, QTYPE_A);
        assert!(parsed[0].unicast_response);
    }

    #[test]
    fn parses_a_and_aaaa_answers() {
        let v4 = "203.0.113.7".parse::<Ipv4Addr>().unwrap();
        let response = serialize_response(0, "host.local", IpAddr::V4(v4));
        let answers = parse_responses(&response).unwrap();
        assert_eq!(answers, vec![("host.local".to_string(), IpAddr::V4(v4))]);

        let v6 = "fe80::1".parse::<Ipv6Addr>().unwrap();
        let response = serialize_response(0, "host.local", IpAddr::V6(v6));
        let answers = parse_responses(&response).unwrap();
        assert_eq!(answers, vec![("host.local".to_string(), IpAddr::V6(v6))]);
    }

    #[test]
    fn responder_answers_only_for_table_entries() {
        let ip = IpAddr::V4("192.168.1.5".parse().unwrap());
        let mut system = MdnsSystem {
            v4: None,
            v6: None,
            v6_scope: 0,
            v4_token: Token(0),
            v6_token: Token(1),
            local_names: HashMap::new(),
            active_queries: HashMap::new(),
        };
        system.publish_names(HashMap::from([("known.local".to_string(), ip)]));
        let src = SocketAddr::from((Ipv4Addr::LOCALHOST, 40000));

        let known = ParsedQuery {
            id: 0,
            name: "known.local".to_string(),
            qtype: QTYPE_A,
            unicast_response: true,
        };
        assert!(system.build_response(&known, true, src).is_some());

        let unknown = ParsedQuery {
            id: 0,
            name: "other.local".to_string(),
            qtype: QTYPE_A,
            unicast_response: true,
        };
        assert!(system.build_response(&unknown, true, src).is_none());
    }

    #[test]
    fn single_address_constraint_drops_multi_answer() {
        let mut system = MdnsSystem {
            v4: None,
            v6: None,
            v6_scope: 0,
            v4_token: Token(0),
            v6_token: Token(1),
            local_names: HashMap::new(),
            active_queries: HashMap::from([("host.local".to_string(), Instant::now())]),
        };
        let mut out = Vec::new();
        system.collect_resolutions(
            vec![
                (
                    "host.local".to_string(),
                    IpAddr::V4("10.0.0.1".parse().unwrap()),
                ),
                (
                    "host.local".to_string(),
                    IpAddr::V4("10.0.0.2".parse().unwrap()),
                ),
            ],
            &mut out,
        );
        assert!(out.is_empty());
        assert!(!system.active_queries.contains_key("host.local"));
    }

    #[test]
    fn single_answer_resolves() {
        let mut system = MdnsSystem {
            v4: None,
            v6: None,
            v6_scope: 0,
            v4_token: Token(0),
            v6_token: Token(1),
            local_names: HashMap::new(),
            active_queries: HashMap::from([("host.local".to_string(), Instant::now())]),
        };
        let mut out = Vec::new();
        let ip = IpAddr::V4("10.0.0.9".parse().unwrap());
        system.collect_resolutions(vec![("host.local".to_string(), ip)], &mut out);
        assert_eq!(out, vec![("host.local".to_string(), ip)]);
    }

    #[test]
    fn rejects_compression_pointer_loop() {
        // A name whose pointer points back at itself must not loop forever.
        let packet = [0xc0u8, 0x00];
        let mut reader = DnsReader::new(&packet);
        assert!(reader.read_name().is_none());
    }
}
