use std::{
    collections::HashMap,
    net::SocketAddr,
    time::{Duration, Instant},
};

use crate::{
    candidate::{
        Candidate, CandidateKind, CandidatePairId, NatKind, pair_priority, port_guess_candidates,
    },
    stun::{MessageClass, RoleAttribute, StunError, StunMessage, TransactionId},
};

const DEFAULT_MIN_CHECK_INTERVAL: Duration = Duration::from_millis(25);
const DEFAULT_HANDSHAKE_MIN_DURATION: Duration = Duration::from_secs(3);
const DEFAULT_CHECK_DEADLINE: Duration = Duration::from_secs(5);
const DEFAULT_INITIAL_RTO: Duration = Duration::from_millis(100);
const DEFAULT_MAX_RTO: Duration = Duration::from_millis(800);
const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const DEFAULT_RESTART_AFTER_IDLE: Duration = Duration::from_secs(5);
const DEFAULT_DISCONNECT_AFTER_IDLE: Duration = Duration::from_secs(15);
const DEFAULT_PORT_GUESS_LIMIT: usize = 8;
const DEFAULT_PORT_GUESS_MAX_DELTA: u16 = 8;
const DEFAULT_MAX_CHECK_ATTEMPTS: u8 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IceRole {
    Controlling,
    Controlled,
}

impl IceRole {
    pub fn is_controlling(self) -> bool {
        matches!(self, Self::Controlling)
    }
}

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub username: Option<String>,
    pub min_check_interval: Duration,
    pub handshake_min_duration: Duration,
    pub check_deadline: Duration,
    pub initial_rto: Duration,
    pub max_rto: Duration,
    pub keepalive_interval: Duration,
    pub restart_after_idle: Duration,
    pub disconnect_after_idle: Duration,
    pub port_guess_limit: usize,
    pub port_guess_max_delta: u16,
    pub max_check_attempts: u8,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            username: None,
            min_check_interval: DEFAULT_MIN_CHECK_INTERVAL,
            handshake_min_duration: DEFAULT_HANDSHAKE_MIN_DURATION,
            check_deadline: DEFAULT_CHECK_DEADLINE,
            initial_rto: DEFAULT_INITIAL_RTO,
            max_rto: DEFAULT_MAX_RTO,
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
            restart_after_idle: DEFAULT_RESTART_AFTER_IDLE,
            disconnect_after_idle: DEFAULT_DISCONNECT_AFTER_IDLE,
            port_guess_limit: DEFAULT_PORT_GUESS_LIMIT,
            port_guess_max_delta: DEFAULT_PORT_GUESS_MAX_DELTA,
            max_check_attempts: DEFAULT_MAX_CHECK_ATTEMPTS,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackReason {
    RelayCandidateAvailable,
    SymmetricSymmetric,
    NoCommonAddressFamily,
    DirectChecksFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartReason {
    Idle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectedPair {
    pub local_candidate_id: Option<u32>,
    pub remote_candidate_id: Option<u32>,
    pub remote_addr: SocketAddr,
    pub peer_reflexive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    UseRelay {
        relay: Option<SocketAddr>,
        reason: FallbackReason,
    },
    SendStun {
        to: SocketAddr,
        bytes: Vec<u8>,
        transaction_id: TransactionId,
        pair: CandidatePairId,
        retransmit: bool,
    },
    SendStunResponse {
        to: SocketAddr,
        bytes: Vec<u8>,
        transaction_id: TransactionId,
    },
    SendKeepalive {
        to: SocketAddr,
        bytes: Vec<u8>,
        transaction_id: TransactionId,
    },
    DirectReady {
        selected: SelectedPair,
    },
    Migrated {
        selected: SelectedPair,
    },
    IceRestart {
        reason: RestartReason,
    },
    Disconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PairState {
    Waiting,
    InProgress,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug)]
struct CandidatePair {
    id: CandidatePairId,
    local_index: usize,
    remote_index: usize,
    priority: u64,
    state: PairState,
    transaction_id: Option<TransactionId>,
    first_sent_at: Option<Instant>,
    next_due_at: Option<Instant>,
    rto: Duration,
    attempts: u8,
}

#[derive(Clone, Copy, Debug)]
struct Transaction {
    pair_index: usize,
}

pub struct TraversalAgent {
    config: AgentConfig,
    role: IceRole,
    tie_breaker: u64,
    local_nat: NatKind,
    remote_nat: NatKind,
    local_candidates: Vec<Candidate>,
    remote_candidates: Vec<Candidate>,
    pairs: Vec<CandidatePair>,
    transactions: HashMap<TransactionId, Transaction>,
    start: Instant,
    next_check_at: Instant,
    transaction_counter: u64,
    next_candidate_id: u32,
    relay_announced: bool,
    direct_failed_announced: bool,
    guesses_added: bool,
    peer_reflexive_seen: bool,
    selected: Option<SelectedPair>,
    last_rx_at: Option<Instant>,
    next_keepalive_at: Option<Instant>,
    restart_announced: bool,
    disconnected_announced: bool,
}

impl TraversalAgent {
    pub fn new(
        now: Instant,
        config: AgentConfig,
        role: IceRole,
        tie_breaker: u64,
        local_nat: NatKind,
        remote_nat: NatKind,
        local_candidates: Vec<Candidate>,
        remote_candidates: Vec<Candidate>,
    ) -> Self {
        let next_candidate_id = local_candidates
            .iter()
            .chain(remote_candidates.iter())
            .map(|candidate| candidate.id)
            .max()
            .unwrap_or(0)
            .wrapping_add(1)
            .max(1);
        let mut agent = Self {
            config,
            role,
            tie_breaker,
            local_nat,
            remote_nat,
            local_candidates,
            remote_candidates,
            pairs: Vec::new(),
            transactions: HashMap::new(),
            start: now,
            next_check_at: now,
            transaction_counter: 1,
            next_candidate_id,
            relay_announced: false,
            direct_failed_announced: false,
            guesses_added: false,
            peer_reflexive_seen: false,
            selected: None,
            last_rx_at: None,
            next_keepalive_at: None,
            restart_announced: false,
            disconnected_announced: false,
        };
        agent.rebuild_pairs();
        agent
    }

    pub fn role(&self) -> IceRole {
        self.role
    }

    pub fn selected(&self) -> Option<SelectedPair> {
        self.selected
    }

    pub fn direct_pair_count(&self) -> usize {
        self.pairs.len()
    }

    pub fn poll(&mut self, now: Instant) -> Vec<Action> {
        self.mark_expired_pairs(now);
        let mut actions = Vec::new();

        if let Some(selected) = self.selected {
            actions.extend(self.poll_selected(now, selected));
            return actions;
        }

        if !self.relay_announced {
            if self.local_nat.is_symmetric() && self.remote_nat.is_symmetric() {
                self.relay_announced = true;
                actions.push(Action::UseRelay {
                    relay: self.relay_addr(),
                    reason: FallbackReason::SymmetricSymmetric,
                });
                return actions;
            }
            if self.pairs.is_empty() {
                self.relay_announced = true;
                actions.push(Action::UseRelay {
                    relay: self.relay_addr(),
                    reason: FallbackReason::NoCommonAddressFamily,
                });
                return actions;
            }
            if self.relay_addr().is_some() {
                self.relay_announced = true;
                actions.push(Action::UseRelay {
                    relay: self.relay_addr(),
                    reason: FallbackReason::RelayCandidateAvailable,
                });
            }
        }

        if self.all_pairs_failed() && !self.guesses_added {
            self.add_port_guesses();
        }

        if self.all_pairs_failed()
            && now.duration_since(self.start) >= self.config.handshake_min_duration
            && !self.direct_failed_announced
        {
            self.direct_failed_announced = true;
            actions.push(Action::UseRelay {
                relay: self.relay_addr(),
                reason: FallbackReason::DirectChecksFailed,
            });
            return actions;
        }

        if now < self.next_check_at {
            return actions;
        }

        if let Some(action) = self.next_check_action(now) {
            self.next_check_at = now + self.config.min_check_interval;
            actions.push(action);
        }
        actions
    }

    pub fn handle_inbound(
        &mut self,
        now: Instant,
        src: SocketAddr,
        bytes: &[u8],
    ) -> Result<Vec<Action>, StunError> {
        let message = StunMessage::decode(bytes)?;
        if message.class == MessageClass::Request
            && !self.accepts_username(message.username.as_deref())
        {
            return Ok(Vec::new());
        }
        self.last_rx_at = Some(now);
        self.restart_announced = false;
        self.disconnected_announced = false;
        self.resolve_role_conflict(&message);

        match message.class {
            MessageClass::Request => {
                self.peer_reflexive_seen = true;
                let selected = self.select_peer_reflexive(src);
                let response = StunMessage::binding_success(message.transaction_id, src).encode();
                let mut actions = vec![Action::SendStunResponse {
                    to: src,
                    bytes: response,
                    transaction_id: message.transaction_id,
                }];
                actions.push(Action::DirectReady { selected });
                Ok(actions)
            }
            MessageClass::SuccessResponse => {
                let Some(transaction) = self.transactions.remove(&message.transaction_id) else {
                    return Ok(Vec::new());
                };
                let pair = &mut self.pairs[transaction.pair_index];
                pair.state = PairState::Succeeded;
                pair.next_due_at = None;
                let selected = SelectedPair {
                    local_candidate_id: Some(pair.id.local_id),
                    remote_candidate_id: Some(pair.id.remote_id),
                    remote_addr: src,
                    peer_reflexive: self.remote_candidates[pair.remote_index].addr != src,
                };
                self.install_selected(now, selected);
                Ok(vec![Action::DirectReady { selected }])
            }
        }
    }

    pub fn observe_authenticated_packet(
        &mut self,
        now: Instant,
        src: SocketAddr,
    ) -> Option<Action> {
        self.last_rx_at = Some(now);
        self.restart_announced = false;
        self.disconnected_announced = false;

        match self.selected {
            Some(selected) if selected.remote_addr == src => None,
            Some(mut selected) => {
                selected.remote_addr = src;
                selected.peer_reflexive = true;
                self.install_selected(now, selected);
                Some(Action::Migrated { selected })
            }
            None => {
                let selected = self.select_peer_reflexive(src);
                Some(Action::DirectReady { selected })
            }
        }
    }

    fn poll_selected(&mut self, now: Instant, selected: SelectedPair) -> Vec<Action> {
        let mut actions = Vec::new();
        if let Some(last_rx_at) = self.last_rx_at {
            let idle = now.duration_since(last_rx_at);
            if idle >= self.config.disconnect_after_idle {
                if !self.disconnected_announced {
                    self.disconnected_announced = true;
                    actions.push(Action::Disconnected);
                }
                return actions;
            }
            if idle >= self.config.restart_after_idle && !self.restart_announced {
                self.restart_announced = true;
                actions.push(Action::IceRestart {
                    reason: RestartReason::Idle,
                });
            }
        }
        if self
            .next_keepalive_at
            .is_some_and(|next_keepalive_at| now >= next_keepalive_at)
        {
            let transaction_id = self.next_transaction_id();
            self.next_keepalive_at = Some(now + self.config.keepalive_interval);
            let bytes = StunMessage::binding_request(
                transaction_id,
                self.config.username.clone(),
                0,
                self.role_attribute(),
                false,
            )
            .encode();
            actions.push(Action::SendKeepalive {
                to: selected.remote_addr,
                bytes,
                transaction_id,
            });
        }
        actions
    }

    fn next_check_action(&mut self, now: Instant) -> Option<Action> {
        if let Some(pair_index) = self.next_due_retransmission(now) {
            return Some(self.send_check(now, pair_index, true));
        }
        let mut best_pair = None;
        let mut best_priority = 0;
        for (index, pair) in self.pairs.iter().enumerate() {
            if pair.state != PairState::Waiting || !self.pair_is_allowed(pair) {
                continue;
            }
            if best_pair.is_none() || pair.priority > best_priority {
                best_pair = Some(index);
                best_priority = pair.priority;
            }
        }
        let pair_index = best_pair?;
        Some(self.send_check(now, pair_index, false))
    }

    fn next_due_retransmission(&self, now: Instant) -> Option<usize> {
        self.pairs
            .iter()
            .enumerate()
            .filter(|(_, pair)| pair.state == PairState::InProgress)
            .filter(|(_, pair)| pair.next_due_at.is_some_and(|due_at| now >= due_at))
            .filter(|(_, pair)| pair.attempts < self.config.max_check_attempts)
            .max_by_key(|(_, pair)| pair.priority)
            .map(|(index, _)| index)
    }

    fn send_check(&mut self, now: Instant, pair_index: usize, retransmit: bool) -> Action {
        let transaction_id = if retransmit {
            self.pairs[pair_index]
                .transaction_id
                .expect("in-progress pair has transaction id")
        } else {
            self.next_transaction_id()
        };
        let local_index = self.pairs[pair_index].local_index;
        let remote_index = self.pairs[pair_index].remote_index;
        let local = &self.local_candidates[local_index];
        let remote = &self.remote_candidates[remote_index];
        let bytes = StunMessage::binding_request(
            transaction_id,
            self.config.username.clone(),
            local.priority,
            self.role_attribute(),
            self.role.is_controlling(),
        )
        .encode();

        let pair = &mut self.pairs[pair_index];
        pair.state = PairState::InProgress;
        pair.transaction_id = Some(transaction_id);
        pair.first_sent_at.get_or_insert(now);
        pair.attempts = pair.attempts.saturating_add(1);
        pair.next_due_at = Some(now + pair.rto);
        pair.rto = (pair.rto * 2).min(self.config.max_rto);
        self.transactions
            .insert(transaction_id, Transaction { pair_index });

        Action::SendStun {
            to: remote.addr,
            bytes,
            transaction_id,
            pair: pair.id,
            retransmit,
        }
    }

    fn pair_is_allowed(&self, pair: &CandidatePair) -> bool {
        if self.local_nat.is_cone() && self.remote_nat.is_symmetric() && !self.peer_reflexive_seen {
            let remote = &self.remote_candidates[pair.remote_index];
            !matches!(
                remote.kind,
                CandidateKind::ServerReflexive | CandidateKind::PeerReflexive
            )
        } else {
            true
        }
    }

    fn mark_expired_pairs(&mut self, now: Instant) {
        if now.duration_since(self.start) < self.config.handshake_min_duration {
            return;
        }
        for pair in &mut self.pairs {
            if pair.state != PairState::InProgress {
                continue;
            }
            let Some(first_sent_at) = pair.first_sent_at else {
                continue;
            };
            if pair.attempts >= self.config.max_check_attempts
                && now.duration_since(first_sent_at) >= self.config.check_deadline
            {
                pair.state = PairState::Failed;
                pair.next_due_at = None;
                if let Some(transaction_id) = pair.transaction_id.take() {
                    self.transactions.remove(&transaction_id);
                }
            }
        }
    }

    fn add_port_guesses(&mut self) {
        let guess_sources = self
            .remote_candidates
            .iter()
            .filter(|candidate| matches!(candidate.kind, CandidateKind::ServerReflexive))
            .cloned()
            .collect::<Vec<_>>();
        for remote in guess_sources {
            let guesses = port_guess_candidates(
                &mut self.next_candidate_id,
                &remote,
                self.config.port_guess_limit,
                self.config.port_guess_max_delta,
            );
            self.remote_candidates.extend(guesses);
        }
        self.guesses_added = true;
        self.rebuild_pairs();
    }

    fn rebuild_pairs(&mut self) {
        let mut existing = self
            .pairs
            .iter()
            .map(|pair| pair.id)
            .collect::<std::collections::HashSet<_>>();
        for (local_index, local) in self.local_candidates.iter().enumerate() {
            for (remote_index, remote) in self.remote_candidates.iter().enumerate() {
                if !local.can_pair_with(remote) {
                    continue;
                }
                let id = CandidatePairId {
                    local_id: local.id,
                    remote_id: remote.id,
                };
                if !existing.insert(id) {
                    continue;
                }
                self.pairs.push(CandidatePair {
                    id,
                    local_index,
                    remote_index,
                    priority: pair_priority(local, remote, self.role.is_controlling()),
                    state: PairState::Waiting,
                    transaction_id: None,
                    first_sent_at: None,
                    next_due_at: None,
                    rto: self.config.initial_rto,
                    attempts: 0,
                });
            }
        }
    }

    fn all_pairs_failed(&self) -> bool {
        !self.pairs.is_empty()
            && self
                .pairs
                .iter()
                .all(|pair| matches!(pair.state, PairState::Failed))
    }

    fn relay_addr(&self) -> Option<SocketAddr> {
        self.remote_candidates
            .iter()
            .chain(self.local_candidates.iter())
            .find(|candidate| candidate.kind == CandidateKind::Relay)
            .map(|candidate| candidate.addr)
    }

    fn resolve_role_conflict(&mut self, message: &StunMessage) {
        match (self.role, message.role) {
            (IceRole::Controlling, Some(RoleAttribute::Controlling(remote)))
                if remote > self.tie_breaker =>
            {
                self.role = IceRole::Controlled;
                self.recompute_pair_priorities();
            }
            (IceRole::Controlled, Some(RoleAttribute::Controlled(remote)))
                if self.tie_breaker > remote =>
            {
                self.role = IceRole::Controlling;
                self.recompute_pair_priorities();
            }
            _ => {}
        }
    }

    fn recompute_pair_priorities(&mut self) {
        for pair in &mut self.pairs {
            let local = &self.local_candidates[pair.local_index];
            let remote = &self.remote_candidates[pair.remote_index];
            pair.priority = pair_priority(local, remote, self.role.is_controlling());
        }
    }

    fn role_attribute(&self) -> RoleAttribute {
        match self.role {
            IceRole::Controlling => RoleAttribute::Controlling(self.tie_breaker),
            IceRole::Controlled => RoleAttribute::Controlled(self.tie_breaker),
        }
    }

    fn accepts_username(&self, username: Option<&str>) -> bool {
        match (&self.config.username, username) {
            (Some(expected), Some(actual)) => expected == actual,
            (Some(_), None) => false,
            _ => true,
        }
    }

    fn select_peer_reflexive(&mut self, src: SocketAddr) -> SelectedPair {
        if let Some(selected) = self.selected.filter(|selected| selected.remote_addr == src) {
            return selected;
        }
        let remote_candidate_id = self
            .remote_candidates
            .iter()
            .find(|candidate| candidate.addr == src)
            .map(|candidate| candidate.id)
            .or_else(|| {
                let id = self.next_candidate_id;
                self.next_candidate_id = self.next_candidate_id.wrapping_add(1).max(1);
                self.remote_candidates
                    .push(Candidate::new(id, CandidateKind::PeerReflexive, src));
                Some(id)
            });
        let local_candidate_id = self
            .local_candidates
            .iter()
            .find(|candidate| candidate.family() == crate::candidate::NetworkFamily::of(src))
            .map(|candidate| candidate.id);
        let selected = SelectedPair {
            local_candidate_id,
            remote_candidate_id,
            remote_addr: src,
            peer_reflexive: true,
        };
        self.install_selected(self.last_rx_at.unwrap_or(self.start), selected);
        selected
    }

    fn install_selected(&mut self, now: Instant, selected: SelectedPair) {
        self.selected = Some(selected);
        self.last_rx_at = Some(now);
        self.next_keepalive_at = Some(now + self.config.keepalive_interval);
        self.restart_announced = false;
        self.disconnected_announced = false;
    }

    fn next_transaction_id(&mut self) -> TransactionId {
        let id = TransactionId::from_counter(self.transaction_counter);
        self.transaction_counter = self.transaction_counter.wrapping_add(1).max(1);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{candidate::CandidateKind, stun::StunMessage};

    fn at(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    fn fast_config() -> AgentConfig {
        AgentConfig {
            username: None,
            min_check_interval: Duration::from_millis(25),
            handshake_min_duration: Duration::from_millis(1),
            check_deadline: Duration::from_millis(1),
            initial_rto: Duration::from_millis(100),
            max_rto: Duration::from_millis(800),
            keepalive_interval: Duration::from_secs(10),
            restart_after_idle: Duration::from_secs(5),
            disconnect_after_idle: Duration::from_secs(15),
            port_guess_limit: 4,
            port_guess_max_delta: 4,
            max_check_attempts: 1,
        }
    }

    fn candidate(id: u32, kind: CandidateKind, addr: &str) -> Candidate {
        Candidate::new(id, kind, addr.parse().unwrap())
    }

    fn relay() -> Candidate {
        candidate(99, CandidateKind::Relay, "203.0.113.10:41001")
    }

    fn send_stun(actions: &[Action]) -> Vec<SocketAddr> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::SendStun { to, .. } => Some(*to),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn symmetric_symmetric_uses_relay_without_waiting() {
        let now = at(0);
        let mut agent = TraversalAgent::new(
            now,
            AgentConfig::default(),
            IceRole::Controlling,
            10,
            NatKind::Symmetric,
            NatKind::Symmetric,
            vec![candidate(
                1,
                CandidateKind::ServerReflexive,
                "198.51.100.1:5000",
            )],
            vec![
                candidate(2, CandidateKind::ServerReflexive, "198.51.100.2:6000"),
                relay(),
            ],
        );

        assert_eq!(
            agent.poll(now),
            vec![Action::UseRelay {
                relay: Some("203.0.113.10:41001".parse().unwrap()),
                reason: FallbackReason::SymmetricSymmetric,
            }]
        );
    }

    #[test]
    fn symmetric_to_cone_is_inbound_driven_on_cone_side() {
        let now = at(0);
        let mut cone = TraversalAgent::new(
            now,
            AgentConfig::default(),
            IceRole::Controlled,
            5,
            NatKind::Cone,
            NatKind::Symmetric,
            vec![candidate(
                1,
                CandidateKind::ServerReflexive,
                "198.51.100.1:5000",
            )],
            vec![candidate(
                2,
                CandidateKind::ServerReflexive,
                "198.51.100.2:6000",
            )],
        );
        assert!(send_stun(&cone.poll(now)).is_empty());

        let request = StunMessage::binding_request(
            TransactionId::from_counter(1),
            None,
            1,
            RoleAttribute::Controlling(9),
            true,
        )
        .encode();
        let actions = cone
            .handle_inbound(
                now + Duration::from_millis(10),
                "198.51.100.2:6001".parse().unwrap(),
                &request,
            )
            .unwrap();

        assert!(matches!(actions[0], Action::SendStunResponse { .. }));
        assert!(matches!(
            actions[1],
            Action::DirectReady {
                selected: SelectedPair {
                    remote_addr,
                    peer_reflexive: true,
                    ..
                }
            } if remote_addr == "198.51.100.2:6001".parse().unwrap()
        ));
    }

    #[test]
    fn host_checks_cover_no_hairpin_lan_case_first() {
        let now = at(0);
        let mut agent = TraversalAgent::new(
            now,
            AgentConfig::default(),
            IceRole::Controlling,
            10,
            NatKind::Cone,
            NatKind::Cone,
            vec![
                candidate(1, CandidateKind::Host, "192.168.1.2:5000"),
                candidate(2, CandidateKind::ServerReflexive, "198.51.100.1:5000"),
            ],
            vec![
                candidate(3, CandidateKind::Host, "192.168.1.3:5001"),
                candidate(4, CandidateKind::ServerReflexive, "198.51.100.1:5001"),
            ],
        );

        let actions = agent.poll(now);
        assert_eq!(
            send_stun(&actions),
            vec!["192.168.1.3:5001".parse().unwrap()]
        );
    }

    #[test]
    fn ipv4_ipv6_mismatch_uses_relay() {
        let now = at(0);
        let mut agent = TraversalAgent::new(
            now,
            AgentConfig::default(),
            IceRole::Controlling,
            10,
            NatKind::Cone,
            NatKind::Cone,
            vec![candidate(1, CandidateKind::Host, "192.168.1.2:5000")],
            vec![
                candidate(2, CandidateKind::Host, "[2001:db8::2]:5000"),
                relay(),
            ],
        );

        assert_eq!(
            agent.poll(now),
            vec![Action::UseRelay {
                relay: Some("203.0.113.10:41001".parse().unwrap()),
                reason: FallbackReason::NoCommonAddressFamily,
            }]
        );
    }

    #[test]
    fn connectivity_checks_are_paced() {
        let now = at(0);
        let mut agent = TraversalAgent::new(
            now,
            AgentConfig::default(),
            IceRole::Controlling,
            10,
            NatKind::Cone,
            NatKind::Cone,
            vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
            vec![
                candidate(2, CandidateKind::Host, "10.0.0.3:5001"),
                candidate(3, CandidateKind::ServerReflexive, "198.51.100.3:5001"),
            ],
        );

        assert_eq!(send_stun(&agent.poll(now)).len(), 1);
        assert!(send_stun(&agent.poll(now + Duration::from_millis(10))).is_empty());
        assert_eq!(
            send_stun(&agent.poll(now + Duration::from_millis(25))).len(),
            1
        );
    }

    #[test]
    fn checks_retransmit_with_exponential_backoff() {
        let now = at(0);
        let mut agent = TraversalAgent::new(
            now,
            AgentConfig::default(),
            IceRole::Controlling,
            10,
            NatKind::Cone,
            NatKind::Cone,
            vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
            vec![candidate(2, CandidateKind::Host, "10.0.0.3:5001")],
        );

        assert!(matches!(
            agent.poll(now).pop(),
            Some(Action::SendStun {
                retransmit: false,
                ..
            })
        ));
        assert!(agent.poll(now + Duration::from_millis(99)).is_empty());
        assert!(matches!(
            agent.poll(now + Duration::from_millis(100)).pop(),
            Some(Action::SendStun {
                retransmit: true,
                ..
            })
        ));
        assert!(agent.poll(now + Duration::from_millis(299)).is_empty());
        assert!(matches!(
            agent.poll(now + Duration::from_millis(300)).pop(),
            Some(Action::SendStun {
                retransmit: true,
                ..
            })
        ));
    }

    #[test]
    fn failed_checks_add_limited_port_guesses_before_relay_failure() {
        let now = at(0);
        let mut agent = TraversalAgent::new(
            now,
            fast_config(),
            IceRole::Controlling,
            10,
            NatKind::Cone,
            NatKind::Cone,
            vec![candidate(
                1,
                CandidateKind::ServerReflexive,
                "198.51.100.1:5000",
            )],
            vec![candidate(
                2,
                CandidateKind::ServerReflexive,
                "198.51.100.2:55000",
            )],
        );

        assert_eq!(
            send_stun(&agent.poll(now)),
            vec!["198.51.100.2:55000".parse().unwrap()]
        );
        let actions = agent.poll(now + Duration::from_millis(25));
        assert_eq!(
            send_stun(&actions),
            vec!["198.51.100.2:55001".parse().unwrap()]
        );
    }

    #[test]
    fn response_selects_actual_source_address() {
        let now = at(0);
        let mut agent = TraversalAgent::new(
            now,
            AgentConfig::default(),
            IceRole::Controlling,
            10,
            NatKind::Cone,
            NatKind::Cone,
            vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
            vec![candidate(
                2,
                CandidateKind::ServerReflexive,
                "198.51.100.2:55000",
            )],
        );
        let action = agent.poll(now).pop().unwrap();
        let transaction_id = match action {
            Action::SendStun { transaction_id, .. } => transaction_id,
            _ => panic!("expected STUN check"),
        };
        let response =
            StunMessage::binding_success(transaction_id, "198.51.100.1:40000".parse().unwrap())
                .encode();
        let actions = agent
            .handle_inbound(
                now + Duration::from_millis(50),
                "198.51.100.2:55003".parse().unwrap(),
                &response,
            )
            .unwrap();

        assert!(matches!(
            actions.as_slice(),
            [Action::DirectReady {
                selected: SelectedPair {
                    remote_addr,
                    peer_reflexive: true,
                    ..
                }
            }] if *remote_addr == "198.51.100.2:55003".parse().unwrap()
        ));
    }

    #[test]
    fn username_filters_requests_but_not_transaction_responses() {
        let now = at(0);
        let config = AgentConfig {
            username: Some("chatt-p2p:77".to_string()),
            ..AgentConfig::default()
        };
        let mut agent = TraversalAgent::new(
            now,
            config,
            IceRole::Controlling,
            10,
            NatKind::Cone,
            NatKind::Cone,
            vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
            vec![candidate(2, CandidateKind::Host, "10.0.0.3:5001")],
        );

        let wrong_request = StunMessage::binding_request(
            TransactionId::from_counter(100),
            Some("chatt-p2p:78".to_string()),
            1,
            RoleAttribute::Controlled(9),
            true,
        )
        .encode();
        assert!(
            agent
                .handle_inbound(now, "10.0.0.3:5001".parse().unwrap(), &wrong_request)
                .unwrap()
                .is_empty()
        );

        let action = agent.poll(now).pop().unwrap();
        let transaction_id = match action {
            Action::SendStun { transaction_id, .. } => transaction_id,
            _ => panic!("expected STUN check"),
        };
        let response =
            StunMessage::binding_success(transaction_id, "10.0.0.2:5000".parse().unwrap()).encode();
        let actions = agent
            .handle_inbound(
                now + Duration::from_millis(50),
                "10.0.0.3:5001".parse().unwrap(),
                &response,
            )
            .unwrap();
        assert!(matches!(actions.as_slice(), [Action::DirectReady { .. }]));
    }

    #[test]
    fn authenticated_packet_migrates_selected_address() {
        let now = at(0);
        let mut agent = TraversalAgent::new(
            now,
            AgentConfig::default(),
            IceRole::Controlling,
            10,
            NatKind::Cone,
            NatKind::Cone,
            vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
            vec![candidate(2, CandidateKind::Host, "10.0.0.3:5001")],
        );
        assert!(matches!(
            agent.observe_authenticated_packet(now, "10.0.0.3:5001".parse().unwrap()),
            Some(Action::DirectReady { .. })
        ));
        assert!(matches!(
            agent.observe_authenticated_packet(
                now + Duration::from_secs(1),
                "198.51.100.9:62000".parse().unwrap()
            ),
            Some(Action::Migrated {
                selected: SelectedPair {
                    remote_addr,
                    peer_reflexive: true,
                    ..
                }
            }) if remote_addr == "198.51.100.9:62000".parse().unwrap()
        ));
    }

    #[test]
    fn idle_selected_connection_restarts_then_disconnects() {
        let now = at(0);
        let mut agent = TraversalAgent::new(
            now,
            AgentConfig::default(),
            IceRole::Controlling,
            10,
            NatKind::Cone,
            NatKind::Cone,
            vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
            vec![candidate(2, CandidateKind::Host, "10.0.0.3:5001")],
        );
        agent.observe_authenticated_packet(now, "10.0.0.3:5001".parse().unwrap());

        assert!(matches!(
            agent.poll(now + Duration::from_secs(5)).as_slice(),
            [Action::IceRestart {
                reason: RestartReason::Idle
            }]
        ));
        assert!(matches!(
            agent.poll(now + Duration::from_secs(15)).as_slice(),
            [Action::Disconnected]
        ));
    }

    #[test]
    fn glare_uses_tie_breaker_to_resolve_roles() {
        let now = at(0);
        let mut agent = TraversalAgent::new(
            now,
            AgentConfig::default(),
            IceRole::Controlling,
            10,
            NatKind::Cone,
            NatKind::Cone,
            vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
            vec![candidate(2, CandidateKind::Host, "10.0.0.3:5001")],
        );
        let request = StunMessage::binding_request(
            TransactionId::from_counter(1),
            None,
            1,
            RoleAttribute::Controlling(11),
            true,
        )
        .encode();

        let _ = agent
            .handle_inbound(now, "10.0.0.3:5001".parse().unwrap(), &request)
            .unwrap();

        assert_eq!(agent.role(), IceRole::Controlled);
    }
}
