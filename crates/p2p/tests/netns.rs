use std::process::Command;

#[test]
fn opt_in_linux_netns_veth_smoke() {
    if std::env::var("CHATT_NETNS_TESTS").ok().as_deref() != Some("1") {
        eprintln!("skipping netns smoke test; set CHATT_NETNS_TESTS=1 to enable");
        return;
    }

    if !cfg!(target_os = "linux") {
        panic!("netns tests are Linux-only");
    }
    let uid = command_output("id", &["-u"]);
    if uid.trim() != "0" {
        panic!("netns tests require root or CAP_NET_ADMIN");
    }

    let suffix = std::process::id();
    let ns_a = format!("chatt-p2p-a-{suffix}");
    let ns_b = format!("chatt-p2p-b-{suffix}");
    let veth_a = format!("tcpa{suffix}");
    let veth_b = format!("tcpb{suffix}");
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
    run(
        "ip",
        &[
            "netns",
            "exec",
            &ns_a,
            "ip",
            "addr",
            "add",
            "10.77.0.1/24",
            "dev",
            &veth_a,
        ],
    );
    run(
        "ip",
        &[
            "netns",
            "exec",
            &ns_b,
            "ip",
            "addr",
            "add",
            "10.77.0.2/24",
            "dev",
            &veth_b,
        ],
    );
    run(
        "ip",
        &["netns", "exec", &ns_a, "ip", "link", "set", &veth_a, "up"],
    );
    run(
        "ip",
        &["netns", "exec", &ns_b, "ip", "link", "set", &veth_b, "up"],
    );

    let output = command_output(
        "ip",
        &["netns", "exec", &ns_a, "ip", "addr", "show", "dev", &veth_a],
    );
    assert!(output.contains("10.77.0.1/24"));

    drop(cleanup);
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
