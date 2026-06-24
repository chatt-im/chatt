use std::{net::SocketAddr, time::Duration};

use crate::{
    Action, AgentConfig, Candidate, CandidateKind, FallbackReason, IceRole, NatClassifier, NatKind,
    ReflexiveObservation, RestartPortPolicy, StunAuth, StunMessage, TransactionId, TraversalAgent,
    candidate::port_guess_candidates,
    interfaces::{InterfaceSnapshot, LocalInterface, is_virtual_interface_name},
    socket::is_ignorable_udp_error,
    stun::{RoleAttribute, is_stun_message},
};

fn at(ms: u64) -> std::time::Instant {
    std::time::Instant::now() + Duration::from_millis(ms)
}

fn candidate(id: u32, kind: CandidateKind, addr: &str) -> Candidate {
    Candidate::with_metadata(
        id,
        1,
        1,
        kind,
        addr.parse().unwrap(),
        None,
        kind == CandidateKind::Host,
    )
}

fn relay() -> Candidate {
    Candidate::with_metadata(
        99,
        1,
        1,
        CandidateKind::Relay,
        "203.0.113.10:41001".parse().unwrap(),
        None,
        true,
    )
}

fn test_config() -> AgentConfig {
    AgentConfig::with_auth(StunAuth::new([0u8; 32], [0u8; 32]))
}

fn fast_config() -> AgentConfig {
    AgentConfig {
        handshake_min_duration: Duration::from_millis(1),
        check_deadline: Duration::from_millis(1),
        max_check_attempts: 1,
        port_guess_limit: 4,
        port_guess_max_delta: 4,
        ..test_config()
    }
}

fn first_stun(actions: Vec<Action>) -> Option<(SocketAddr, Vec<u8>)> {
    actions.into_iter().find_map(|action| match action {
        Action::SendStun { to, bytes, .. } => Some((to, bytes)),
        _ => None,
    })
}

#[test]
fn case_01_symmetric_symmetric_deadlock_immediate_relay() {
    let now = at(0);
    let mut agent = TraversalAgent::new(
        now,
        test_config(),
        IceRole::Controlling,
        1,
        NatKind::Symmetric,
        NatKind::Symmetric,
        vec![candidate(
            1,
            CandidateKind::ServerReflexive,
            "198.51.100.1:55000",
        )],
        vec![
            candidate(2, CandidateKind::ServerReflexive, "198.51.100.2:65000"),
            relay(),
        ],
    );

    assert!(matches!(
        agent.poll(now).as_slice(),
        [Action::UseRelay {
            reason: FallbackReason::SymmetricSymmetric,
            ..
        }]
    ));
}

#[test]
fn case_02_symmetric_to_cone_uses_inbound_peer_reflexive_port() {
    let now = at(0);
    let mut cone = TraversalAgent::new(
        now,
        test_config(),
        IceRole::Controlled,
        2,
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
    assert!(
        cone.poll(now)
            .iter()
            .all(|action| !matches!(action, Action::SendStun { .. }))
    );

    let request = StunMessage::binding_request(
        TransactionId::from_counter(1),
        None,
        1,
        RoleAttribute::Controlling(9),
        true,
    )
    .encode(Some(&[0u8; 32]));
    let actions = cone
        .handle_inbound(now, "198.51.100.2:6001".parse().unwrap(), &request)
        .unwrap();
    assert!(matches!(
        actions.as_slice(),
        [Action::SendStunResponse { .. }, Action::DirectReady { selected }]
            if selected.remote_addr == "198.51.100.2:6001".parse().unwrap()
    ));
}

#[test]
fn case_03_eim_deviation_runs_limited_sequential_port_guessing() {
    let mut next_id = 10;
    let remote = candidate(2, CandidateKind::ServerReflexive, "198.51.100.2:55000");
    let guesses = port_guess_candidates(&mut next_id, &remote, 4, 4);
    let ports = guesses
        .iter()
        .map(|candidate| candidate.addr.port())
        .collect::<Vec<_>>();
    assert_eq!(ports, vec![55001, 54999, 55002, 54998]);
}

#[test]
fn case_04_multilayer_nat_races_host_reflexive_and_relay_candidates() {
    let now = at(0);
    let mut agent = TraversalAgent::new(
        now,
        test_config(),
        IceRole::Controlling,
        1,
        NatKind::Unknown,
        NatKind::Unknown,
        vec![
            candidate(1, CandidateKind::Host, "10.0.0.2:5000"),
            candidate(2, CandidateKind::ServerReflexive, "198.51.100.1:55000"),
            relay(),
        ],
        vec![
            candidate(3, CandidateKind::Host, "10.0.0.3:5000"),
            candidate(4, CandidateKind::ServerReflexive, "198.51.100.2:56000"),
            relay(),
        ],
    );
    let actions = agent.poll(now);
    assert!(
        actions
            .iter()
            .any(|action| matches!(action, Action::UseRelay { .. }))
    );
    assert!(
        actions
            .iter()
            .any(|action| matches!(action, Action::SendStun { .. }))
    );
}

#[test]
fn case_05_port_preservation_failure_uses_observed_reflexive_port() {
    let mut classifier = NatClassifier::new();
    classifier.observe(ReflexiveObservation {
        server_addr: "203.0.113.10:41001".parse().unwrap(),
        mapped_addr: "198.51.100.5:62000".parse().unwrap(),
    });
    classifier.observe(ReflexiveObservation {
        server_addr: "203.0.113.10:41002".parse().unwrap(),
        mapped_addr: "198.51.100.5:62000".parse().unwrap(),
    });
    assert_eq!(classifier.primary_reflexive_addr().unwrap().port(), 62000);
}

#[test]
fn case_06_no_hairpin_prefers_lan_host_path() {
    let now = at(0);
    let mut agent = TraversalAgent::new(
        now,
        test_config(),
        IceRole::Controlling,
        1,
        NatKind::Cone,
        NatKind::Cone,
        vec![
            candidate(1, CandidateKind::Host, "192.168.1.2:5000"),
            candidate(2, CandidateKind::ServerReflexive, "198.51.100.1:55000"),
        ],
        vec![
            candidate(3, CandidateKind::Host, "192.168.1.3:5000"),
            candidate(4, CandidateKind::ServerReflexive, "198.51.100.1:56000"),
        ],
    );
    assert_eq!(
        first_stun(agent.poll(now)).unwrap().0,
        "192.168.1.3:5000".parse().unwrap()
    );
}

#[test]
fn case_07_ipv4_ipv6_mismatch_falls_back_to_relay() {
    let now = at(0);
    let mut agent = TraversalAgent::new(
        now,
        test_config(),
        IceRole::Controlling,
        1,
        NatKind::Cone,
        NatKind::Cone,
        vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
        vec![
            candidate(2, CandidateKind::Host, "[2001:db8::2]:5000"),
            relay(),
        ],
    );
    assert!(matches!(
        agent.poll(now).as_slice(),
        [Action::UseRelay {
            reason: FallbackReason::NoCommonAddressFamily,
            ..
        }]
    ));
}

#[test]
fn case_08_ipv6_privacy_rotation_detects_interface_change() {
    let before = snapshot("2001:db8::1".parse().unwrap());
    let after = snapshot("2001:db8::2".parse().unwrap());
    assert!(after.changed_from(&before));
}

#[test]
fn case_09_split_tunnel_vpn_interfaces_are_filtered() {
    assert!(is_virtual_interface_name("tun0"));
    assert!(is_virtual_interface_name("wg-home"));
    assert!(!is_virtual_interface_name("eth0"));
}

#[test]
fn case_10_connectivity_checks_are_paced_at_middlebox_safe_interval() {
    let now = at(0);
    let mut agent = TraversalAgent::new(
        now,
        test_config(),
        IceRole::Controlling,
        1,
        NatKind::Cone,
        NatKind::Cone,
        vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
        vec![
            candidate(2, CandidateKind::Host, "10.0.0.3:5000"),
            candidate(3, CandidateKind::ServerReflexive, "198.51.100.3:5000"),
        ],
    );
    assert!(first_stun(agent.poll(now)).is_some());
    assert!(first_stun(agent.poll(now + Duration::from_millis(20))).is_none());
    assert!(first_stun(agent.poll(now + Duration::from_millis(25))).is_some());
}

#[test]
fn case_11_dpi_safe_connectivity_checks_are_stun_binding_requests() {
    let now = at(0);
    let mut agent = TraversalAgent::new(
        now,
        test_config(),
        IceRole::Controlling,
        1,
        NatKind::Cone,
        NatKind::Cone,
        vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
        vec![candidate(2, CandidateKind::Host, "10.0.0.3:5000")],
    );
    let (_, bytes) = first_stun(agent.poll(now)).unwrap();
    assert!(is_stun_message(&bytes));
}

#[test]
fn case_12_icmp_unreachable_socket_errors_are_ignored_for_udp() {
    assert!(is_ignorable_udp_error(&std::io::Error::from(
        std::io::ErrorKind::ConnectionRefused
    )));
}

#[test]
fn case_13_buggy_upnp_mapping_is_only_a_verified_candidate() {
    let now = at(0);
    let mut agent = TraversalAgent::new(
        now,
        fast_config(),
        IceRole::Controlling,
        1,
        NatKind::Cone,
        NatKind::Cone,
        vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
        vec![
            candidate(2, CandidateKind::PortMapped, "198.51.100.2:40000"),
            relay(),
        ],
    );
    assert!(first_stun(agent.poll(now)).is_some());
    let actions = agent.poll(now + Duration::from_millis(25));
    assert!(
        actions
            .iter()
            .all(|action| !matches!(action, Action::DirectReady { .. }))
    );
}

#[test]
fn case_14_same_socket_metadata_survives_host_reflexive_and_relay_candidates() {
    let candidates = [
        Candidate::with_metadata(
            1,
            7,
            2,
            CandidateKind::Host,
            "10.0.0.2:5000".parse().unwrap(),
            None,
            true,
        ),
        Candidate::with_metadata(
            2,
            7,
            2,
            CandidateKind::ServerReflexive,
            "198.51.100.2:55000".parse().unwrap(),
            None,
            true,
        ),
        Candidate::with_metadata(
            3,
            7,
            2,
            CandidateKind::Relay,
            "203.0.113.10:41001".parse().unwrap(),
            None,
            true,
        ),
    ];
    assert!(candidates.iter().all(|candidate| candidate.socket_id == 7));
}

#[test]
fn case_15_valid_handshake_updates_active_source_address() {
    let now = at(0);
    let mut agent = TraversalAgent::new(
        now,
        test_config(),
        IceRole::Controlling,
        1,
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
    let tx = match action {
        Action::SendStun { transaction_id, .. } => transaction_id,
        _ => panic!("expected send stun"),
    };
    let response =
        StunMessage::binding_success(tx, "10.0.0.2:5000".parse().unwrap()).encode(Some(&[0u8; 32]));
    let actions = agent
        .handle_inbound(now, "198.51.100.2:55003".parse().unwrap(), &response)
        .unwrap();
    assert!(matches!(
        actions.as_slice(),
        [Action::DirectReady { selected }] if selected.remote_addr == "198.51.100.2:55003".parse().unwrap()
    ));
}

#[test]
fn case_16_sleep_wake_idle_triggers_restart_then_disconnect() {
    let now = at(0);
    let mut agent = selected_agent(now);
    assert!(matches!(
        agent.poll(now + Duration::from_secs(5)).as_slice(),
        [Action::IceRestart { .. }]
    ));
    assert!(matches!(
        agent.poll(now + Duration::from_secs(15)).as_slice(),
        [Action::Disconnected]
    ));
}

#[test]
fn case_17_keepalive_is_sent_before_aggressive_nat_expiry() {
    let now = at(0);
    let mut agent = selected_agent(now);
    assert!(
        agent
            .poll(now + Duration::from_secs(10))
            .iter()
            .any(|action| matches!(action, Action::SendKeepalive { .. }))
    );
}

#[test]
fn case_18_mobile_roaming_migrates_by_authenticated_connection_id() {
    let now = at(0);
    let mut agent = selected_agent(now);
    assert!(matches!(
        agent.observe_authenticated_packet(now + Duration::from_millis(1), "198.51.100.9:62000".parse().unwrap()),
        Some(Action::Migrated { selected }) if selected.remote_addr == "198.51.100.9:62000".parse().unwrap()
    ));
}

#[test]
fn case_19_glare_uses_tie_breaker_roles() {
    let now = at(0);
    let mut agent = TraversalAgent::new(
        now,
        test_config(),
        IceRole::Controlling,
        10,
        NatKind::Cone,
        NatKind::Cone,
        vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
        vec![candidate(2, CandidateKind::Host, "10.0.0.3:5000")],
    );
    let request = StunMessage::binding_request(
        TransactionId::from_counter(1),
        None,
        1,
        RoleAttribute::Controlling(11),
        true,
    )
    .encode(Some(&[0u8; 32]));
    let _ = agent.handle_inbound(now, "10.0.0.3:5000".parse().unwrap(), &request);
    assert_eq!(agent.role(), IceRole::Controlled);
}

#[test]
fn case_20_zombie_socket_timeout_emits_disconnect() {
    let now = at(0);
    let mut agent = selected_agent(now);
    assert!(matches!(
        agent.poll(now + Duration::from_secs(15)).as_slice(),
        [Action::Disconnected]
    ));
}

#[test]
fn case_21_asymmetric_latency_does_not_fail_before_minimum_deadline() {
    let now = at(0);
    let mut agent = TraversalAgent::new(
        now,
        fast_config(),
        IceRole::Controlling,
        1,
        NatKind::Cone,
        NatKind::Cone,
        vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
        vec![candidate(2, CandidateKind::Host, "10.0.0.3:5000"), relay()],
    );
    let _ = agent.poll(now);
    assert!(agent.poll(now).iter().all(|action| {
        !matches!(
            action,
            Action::UseRelay {
                reason: FallbackReason::DirectChecksFailed,
                ..
            }
        )
    }));
}

#[test]
fn case_22_packet_loss_uses_exponential_backoff_retransmits() {
    let now = at(0);
    let mut agent = TraversalAgent::new(
        now,
        test_config(),
        IceRole::Controlling,
        1,
        NatKind::Cone,
        NatKind::Cone,
        vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
        vec![candidate(2, CandidateKind::Host, "10.0.0.3:5000")],
    );
    assert!(matches!(
        agent.poll(now).pop(),
        Some(Action::SendStun {
            retransmit: false,
            ..
        })
    ));
    assert!(matches!(
        agent.poll(now + Duration::from_millis(100)).pop(),
        Some(Action::SendStun {
            retransmit: true,
            ..
        })
    ));
    assert!(matches!(
        agent.poll(now + Duration::from_millis(300)).pop(),
        Some(Action::SendStun {
            retransmit: true,
            ..
        })
    ));
}

#[test]
fn case_23_port_reuse_timeout_requires_fresh_ephemeral_rebind() {
    let mut policy = RestartPortPolicy::default();
    policy.record(5000);
    assert!(!policy.accepts(5000));
    assert_eq!(
        RestartPortPolicy::bind_addr_for_restart("127.0.0.1:5000".parse().unwrap()).port(),
        0
    );
}

fn authenticated_agent(now: std::time::Instant, key: [u8; 32]) -> TraversalAgent {
    let config = AgentConfig::with_auth(StunAuth::new(key, [9u8; 32]));
    TraversalAgent::new(
        now,
        config,
        IceRole::Controlling,
        1,
        NatKind::Cone,
        NatKind::Cone,
        vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
        vec![candidate(
            2,
            CandidateKind::ServerReflexive,
            "198.51.100.2:55000",
        )],
    )
}

#[test]
fn case_24_forged_binding_success_without_valid_integrity_does_not_select() {
    let now = at(0);
    let key = [0x42u8; 32];
    let mut agent = authenticated_agent(now, key);
    let tx = match agent.poll(now).pop().unwrap() {
        Action::SendStun { transaction_id, .. } => transaction_id,
        other => panic!("expected send stun, got {other:?}"),
    };
    let forged_addr: SocketAddr = "203.0.113.9:62000".parse().unwrap();

    // An off-path attacker who guessed the in-flight transaction id forges a
    // success choosing its own mapped address, but cannot sign with the shared key.
    let unsigned = StunMessage::binding_success(tx, forged_addr).encode(None);
    assert!(
        agent
            .handle_inbound(now + Duration::from_millis(1), forged_addr, &unsigned)
            .unwrap()
            .is_empty()
    );
    let wrong_key = StunMessage::binding_success(tx, forged_addr).encode(Some(&[0u8; 32]));
    assert!(
        agent
            .handle_inbound(now + Duration::from_millis(2), forged_addr, &wrong_key)
            .unwrap()
            .is_empty()
    );
    assert!(agent.selected().is_none());
}

#[test]
fn case_25_authenticated_binding_success_selects_legitimate_path() {
    let now = at(0);
    let key = [0x42u8; 32];
    let mut agent = authenticated_agent(now, key);
    let tx = match agent.poll(now).pop().unwrap() {
        Action::SendStun { transaction_id, .. } => transaction_id,
        other => panic!("expected send stun, got {other:?}"),
    };
    let mapped: SocketAddr = "10.0.0.2:5000".parse().unwrap();
    let response = StunMessage::binding_success(tx, mapped).encode(Some(&key));
    let src: SocketAddr = "198.51.100.2:55003".parse().unwrap();
    let actions = agent
        .handle_inbound(now + Duration::from_millis(50), src, &response)
        .unwrap();
    assert!(matches!(
        actions.as_slice(),
        [Action::DirectReady { selected }] if selected.remote_addr == src
    ));
    assert_eq!(agent.selected().map(|pair| pair.remote_addr), Some(src));
}

fn selected_agent(now: std::time::Instant) -> TraversalAgent {
    // A long consent timeout keeps the idle-latch cases (restart, disconnect,
    // keepalive) free of consent interference. Consent expiry is exercised
    // separately with the client's tighter keepalive cadence.
    let config = AgentConfig {
        consent_timeout: Duration::from_secs(60),
        ..test_config()
    };
    let mut agent = TraversalAgent::new(
        now,
        config,
        IceRole::Controlling,
        1,
        NatKind::Cone,
        NatKind::Cone,
        vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
        vec![candidate(2, CandidateKind::Host, "10.0.0.3:5000")],
    );
    let _ = agent.observe_authenticated_packet(now, "10.0.0.3:5000".parse().unwrap());
    agent
}

fn snapshot(addr: std::net::IpAddr) -> InterfaceSnapshot {
    InterfaceSnapshot::from_interfaces(vec![LocalInterface {
        index: 1,
        name: "eth0".to_string(),
        addr,
        is_up: true,
        is_loopback: false,
        is_virtual: false,
    }])
    .unwrap()
}

/// Authenticated agent with the client's tight 1 s keepalive cadence, parameterized
/// by STUN `key` and `salt` so consent and jitter behavior can be driven.
fn keepalive_agent(now: std::time::Instant, key: [u8; 32], salt: [u8; 32]) -> TraversalAgent {
    // The client's active values: a 1 s keepalive and a 10 s consent timeout
    // below `disconnect_after_idle`.
    let config = AgentConfig {
        keepalive_interval: Duration::from_secs(1),
        consent_timeout: Duration::from_secs(10),
        ..AgentConfig::with_auth(StunAuth::new(key, salt))
    };
    TraversalAgent::new(
        now,
        config,
        IceRole::Controlling,
        1,
        NatKind::Cone,
        NatKind::Cone,
        vec![candidate(1, CandidateKind::Host, "10.0.0.2:5000")],
        vec![candidate(
            2,
            CandidateKind::ServerReflexive,
            "198.51.100.2:55000",
        )],
    )
}

/// Drives `keepalive_agent` to 12 s, answering every keepalive from the selected
/// address, and returns the millisecond offsets at which each keepalive fired.
fn keepalive_fire_times(key: [u8; 32], salt: [u8; 32]) -> Vec<u64> {
    let now = at(0);
    let mut agent = keepalive_agent(now, key, salt);
    let peer: SocketAddr = "198.51.100.2:55003".parse().unwrap();
    agent.observe_authenticated_packet(now, peer);
    let mut fires = Vec::new();
    let mut t = 0u64;
    while t < 12_000 {
        t += 10;
        let at_t = now + Duration::from_millis(t);
        for action in agent.poll(at_t) {
            if let Action::SendKeepalive {
                to, transaction_id, ..
            } = action
            {
                fires.push(t);
                let response = StunMessage::binding_success(transaction_id, to).encode(Some(&key));
                let _ = agent.handle_inbound(at_t, to, &response);
            }
        }
    }
    fires
}

#[test]
fn case_26_silent_peer_expires_consent_and_clears_selection() {
    let now = at(0);
    let mut agent = keepalive_agent(now, [0x42u8; 32], [9u8; 32]);
    agent.observe_authenticated_packet(now, "198.51.100.2:55003".parse().unwrap());
    // No inbound after selection: consent expires at the 10 s default, before the
    // 15 s disconnect teardown.
    assert_eq!(
        agent.poll(now + Duration::from_secs(10)).as_slice(),
        [Action::ConsentExpired]
    );
    assert!(agent.selected().is_none());
    // Emitted once: the selection is gone, so later polls cannot re-fire it.
    assert!(
        !agent
            .poll(now + Duration::from_secs(11))
            .iter()
            .any(|action| matches!(action, Action::ConsentExpired))
    );
}

#[test]
fn case_27_answered_keepalives_keep_consent_fresh() {
    let now = at(0);
    let key = [0x42u8; 32];
    let mut agent = keepalive_agent(now, key, [9u8; 32]);
    let peer: SocketAddr = "198.51.100.2:55003".parse().unwrap();
    agent.observe_authenticated_packet(now, peer);

    let mut t = 0u64;
    while t < 30_000 {
        t += 200;
        let at_t = now + Duration::from_millis(t);
        for action in agent.poll(at_t) {
            assert!(
                !matches!(action, Action::ConsentExpired),
                "consent expired despite answered keepalives at {t} ms"
            );
            if let Action::SendKeepalive {
                to, transaction_id, ..
            } = action
            {
                let response = StunMessage::binding_success(transaction_id, to).encode(Some(&key));
                let _ = agent.handle_inbound(at_t, to, &response);
            }
        }
    }
    assert!(agent.selected().is_some());
}

#[test]
fn case_28_keepalive_jitter_is_deterministic_and_bounded() {
    let key = [0x42u8; 32];
    let first = keepalive_fire_times(key, [7u8; 32]);
    let second = keepalive_fire_times(key, [7u8; 32]);
    // Same salt yields an identical schedule: no RNG in poll.
    assert_eq!(first, second);
    assert!(
        first.len() >= 5,
        "expected several keepalives, got {first:?}"
    );
    // Gaps stay within [0.8, 1.2] of the 1 s interval, allowing 10 ms poll
    // granularity at each edge.
    for window in first.windows(2) {
        let gap = window[1] - window[0];
        assert!(
            (790..=1210).contains(&gap),
            "keepalive gap {gap} ms outside jitter range"
        );
    }
    // A different salt de-synchronizes the schedule.
    let other = keepalive_fire_times(key, [8u8; 32]);
    assert_ne!(first, other);
}

#[test]
fn case_29_consent_expiry_stops_the_keepalive_flood_but_allows_bounded_recovery() {
    let now = at(0);
    let mut agent = keepalive_agent(now, [0x42u8; 32], [9u8; 32]);
    // Select a peer-reflexive victim address with no configured candidate pair,
    // modeling a NAT rebinding the peer's port onto a third party.
    let victim: SocketAddr = "203.0.113.50:9000".parse().unwrap();
    agent.observe_authenticated_packet(now, victim);
    assert_eq!(agent.selected().map(|pair| pair.remote_addr), Some(victim));

    assert!(
        agent
            .poll(now + Duration::from_secs(10))
            .iter()
            .any(|action| matches!(action, Action::ConsentExpired))
    );
    assert!(agent.selected().is_none());

    // After expiry the keepalive flood to the stale address stops outright, since
    // keepalives only fire on a selected path. ICE connectivity checks may still
    // reach it as paced, bounded consent re-acquisition (a transient outage must
    // not permanently ban the address), so those are counted separately and
    // capped by `max_check_attempts`.
    let mut keepalives_to_victim = 0;
    let mut checks_to_victim = 0;
    let mut t = 10_000u64;
    while t < 25_000 {
        t += 25;
        for action in agent.poll(now + Duration::from_millis(t)) {
            match action {
                Action::SendKeepalive { to, .. } if to == victim => keepalives_to_victim += 1,
                Action::SendStun { to, .. } if to == victim => checks_to_victim += 1,
                _ => {}
            }
        }
    }
    assert_eq!(keepalives_to_victim, 0, "keepalive flood must stop");
    assert!(
        checks_to_victim <= usize::from(test_config().max_check_attempts),
        "recovery checks must stay bounded, got {checks_to_victim}"
    );
}

#[test]
fn case_30_transient_outage_recovers_after_consent_expiry() {
    let now = at(0);
    let mut agent = keepalive_agent(now, [0x42u8; 32], [9u8; 32]);
    let peer: SocketAddr = "198.51.100.2:55003".parse().unwrap();
    agent.observe_authenticated_packet(now, peer);

    assert!(
        agent
            .poll(now + Duration::from_secs(10))
            .iter()
            .any(|action| matches!(action, Action::ConsentExpired))
    );
    assert!(agent.selected().is_none());

    // The peer returns at the same address: authenticated traffic re-selects it,
    // so consent expiry did not permanently ban the path.
    assert!(matches!(
        agent.observe_authenticated_packet(now + Duration::from_millis(10_500), peer),
        Some(Action::DirectReady { selected }) if selected.remote_addr == peer
    ));
    assert_eq!(agent.selected().map(|pair| pair.remote_addr), Some(peer));
}
