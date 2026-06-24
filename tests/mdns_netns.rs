//! Opt-in Linux netns integration test for mDNS host candidate resolution.
//!
//! Two network namespaces are connected by a veth pair forming a virtual LAN.
//! A responder runs in one namespace publishing a `.local` name, a resolver runs
//! in the other and must resolve that name to the responder's veth address over
//! real multicast DNS. Requires root (CAP_NET_ADMIN) and `CHATT_NETNS_TESTS=1`.

use std::net::IpAddr;
use std::os::fd::AsRawFd;
use std::process::Command;
use std::time::{Duration, Instant};

use chatt::mdns::{MdnsSystem, generate_mdns_name};
use mio::{Events, Poll, Token};

const V4: Token = Token(0);
const V6: Token = Token(1);

#[test]
fn opt_in_netns_mdns_resolves_local_candidate() {
    if std::env::var("CHATT_NETNS_TESTS").ok().as_deref() != Some("1") {
        eprintln!("skipping netns mdns test; set CHATT_NETNS_TESTS=1 to enable");
        return;
    }
    if !cfg!(target_os = "linux") {
        panic!("netns tests are Linux-only");
    }
    if command_output("id", &["-u"]).trim() != "0" {
        panic!("netns tests require root or CAP_NET_ADMIN");
    }

    let suffix = std::process::id();
    let ns_a = format!("chatt-mdns-a-{suffix}");
    let ns_b = format!("chatt-mdns-b-{suffix}");
    let veth_a = format!("mdnsa{suffix}");
    let veth_b = format!("mdnsb{suffix}");
    let cleanup = Cleanup {
        ns_a: ns_a.clone(),
        ns_b: ns_b.clone(),
    };

    run("ip", &["netns", "add", &ns_a]);
    run("ip", &["netns", "add", &ns_b]);
    run(
        "ip",
        &[
            "link", "add", &veth_a, "type", "veth", "peer", "name", &veth_b,
        ],
    );
    run("ip", &["link", "set", &veth_a, "netns", &ns_a]);
    run("ip", &["link", "set", &veth_b, "netns", &ns_b]);
    setup_ns(&ns_a, &veth_a, "10.77.0.1/24");
    setup_ns(&ns_b, &veth_b, "10.77.0.2/24");

    let name = generate_mdns_name(&ring::rand::SystemRandom::new()).unwrap();
    let responder_ip: IpAddr = "10.77.0.1".parse().unwrap();

    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let responder_name = name.clone();
    let responder_ns = netns_path(&ns_a);
    let responder = std::thread::spawn(move || {
        enter_netns(&responder_ns);
        let mut poll = Poll::new().unwrap();
        let mut system = MdnsSystem::bind();
        system.register(poll.registry(), V4, V6).unwrap();
        system.publish_names([(responder_name, responder_ip)]);
        let mut events = Events::with_capacity(16);
        while stop_rx.try_recv().is_err() {
            poll.poll(&mut events, Some(Duration::from_millis(50)))
                .unwrap();
            for event in events.iter() {
                system.handle_readable(event.token(), Instant::now());
            }
        }
    });

    let resolver_name = name.clone();
    let resolver_ns = netns_path(&ns_b);
    let resolver = std::thread::spawn(move || -> Option<IpAddr> {
        enter_netns(&resolver_ns);
        let mut poll = Poll::new().unwrap();
        let mut system = MdnsSystem::bind();
        system.register(poll.registry(), V4, V6).unwrap();
        let mut events = Events::with_capacity(16);
        let start = Instant::now();
        system.start_resolve(&resolver_name, Instant::now());
        while start.elapsed() < Duration::from_secs(5) {
            poll.poll(&mut events, Some(Duration::from_millis(50)))
                .unwrap();
            for event in events.iter() {
                let resolved = system.handle_readable(event.token(), Instant::now());
                if let Some((_, ip)) = resolved.into_iter().next() {
                    return Some(ip);
                }
            }
            // Timed-out queries are evicted so the resolve can be reissued.
            system.handle_timeout(Instant::now());
            system.start_resolve(&resolver_name, Instant::now());
        }
        None
    });

    let resolved = resolver.join().unwrap();
    let _ = stop_tx.send(());
    let _ = responder.join();
    drop(cleanup);

    assert_eq!(resolved, Some(responder_ip));
}

fn setup_ns(ns: &str, veth: &str, cidr: &str) {
    run(
        "ip",
        &["netns", "exec", ns, "ip", "addr", "add", cidr, "dev", veth],
    );
    run(
        "ip",
        &["netns", "exec", ns, "ip", "link", "set", veth, "up"],
    );
    run(
        "ip",
        &["netns", "exec", ns, "ip", "link", "set", "lo", "up"],
    );
    // Route link-local multicast out the veth so mDNS queries traverse the LAN.
    run(
        "ip",
        &[
            "netns",
            "exec",
            ns,
            "ip",
            "route",
            "add",
            "224.0.0.0/4",
            "dev",
            veth,
        ],
    );
}

fn netns_path(name: &str) -> String {
    format!("/var/run/netns/{name}")
}

fn enter_netns(path: &str) {
    let file =
        std::fs::File::open(path).unwrap_or_else(|error| panic!("failed to open {path}: {error}"));
    let rc = unsafe { libc::setns(file.as_raw_fd(), libc::CLONE_NEWNET) };
    assert_eq!(
        rc,
        0,
        "setns({path}) failed: {}",
        std::io::Error::last_os_error()
    );
}

struct Cleanup {
    ns_a: String,
    ns_b: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = Command::new("ip")
            .args(["netns", "del", self.ns_a.as_str()])
            .status();
        let _ = Command::new("ip")
            .args(["netns", "del", self.ns_b.as_str()])
            .status();
    }
}

fn run(program: &str, args: &[&str]) {
    let status = Command::new(program)
        .args(args)
        .status()
        .unwrap_or_else(|error| panic!("failed to run {program}: {error}"));
    assert!(
        status.success(),
        "{program} {} failed with {status}",
        args.join(" ")
    );
}

fn command_output(program: &str, args: &[&str]) -> String {
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {program}: {error}"));
    assert!(
        output.status.success(),
        "{program} {} failed with {}",
        args.join(" "),
        output.status
    );
    String::from_utf8(output.stdout).expect("command output is valid UTF-8")
}
