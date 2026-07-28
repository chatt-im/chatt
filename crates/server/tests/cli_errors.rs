use std::{net::TcpListener, process::Command};

#[test]
fn bind_failure_is_logged_to_stdout_with_the_failed_address() {
    let occupied = TcpListener::bind("127.0.0.1:0").expect("reserve a TCP address");
    let occupied_addr = occupied.local_addr().expect("reserved TCP address");
    let server_dir = tempfile::tempdir().expect("temporary server directory");

    let output = Command::new(env!("CARGO_BIN_EXE_chatt-server"))
        .args(["serve", "--dir"])
        .arg(server_dir.path())
        .arg(format!("--network.bind.tcp={occupied_addr}"))
        .arg("--network.bind.udp=127.0.0.1:0")
        .arg("--network.p2p=false")
        .env("KVLOG_COLLECTOR_CONFIG", "Stdout")
        .env_remove("CHATT_LOGFILE")
        .output()
        .expect("run chatt-server");

    assert!(!output.status.success(), "bind failure must exit nonzero");
    assert!(
        output.stderr.is_empty(),
        "bind failure bypassed logging and reached stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("server exited with error"),
        "missing terminal error log:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("tcp bind 1 ({occupied_addr})")),
        "error did not identify the failed bind:\n{stdout}"
    );
}

#[test]
fn configuration_diagnostics_render_to_stdout_without_log_wrapping() {
    let server_dir = tempfile::tempdir().expect("temporary server directory");
    let config_path = server_dir.path().join("invalid.toml");
    std::fs::write(&config_path, "this is not valid TOML = [")
        .expect("write invalid server configuration");

    let output = Command::new(env!("CARGO_BIN_EXE_chatt-server"))
        .args(["serve"])
        .arg(&config_path)
        .env("KVLOG_COLLECTOR_CONFIG", "Stdout")
        .env_remove("CHATT_LOGFILE")
        .output()
        .expect("run chatt-server");

    assert!(
        !output.status.success(),
        "invalid configuration must exit nonzero"
    );
    assert!(
        output.stderr.is_empty(),
        "configuration diagnostics reached stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("this is not valid TOML"),
        "missing rendered configuration source:\n{stdout}"
    );
    assert!(
        !stdout.contains("server configuration diagnostics"),
        "configuration diagnostic was flattened into a log event:\n{stdout}"
    );
    assert!(
        stdout.contains("server exited with error"),
        "missing terminal error log:\n{stdout}"
    );
}

#[test]
fn unusable_logfile_is_reported_on_stdout_before_exit() {
    let server_dir = tempfile::tempdir().expect("temporary server directory");

    let output = Command::new(env!("CARGO_BIN_EXE_chatt-server"))
        .args(["serve", "--dir"])
        .arg(server_dir.path().join("instance"))
        .arg("--logfile")
        .arg(server_dir.path())
        .env("KVLOG_COLLECTOR_CONFIG", "Stdout")
        .env_remove("CHATT_LOGFILE")
        .output()
        .expect("run chatt-server");

    assert!(
        !output.status.success(),
        "invalid logfile must exit nonzero"
    );
    assert!(
        output.stderr.is_empty(),
        "logfile error bypassed logging and reached stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("server logfile unavailable"),
        "missing logfile error:\n{stdout}"
    );
    assert!(
        stdout.contains(&server_dir.path().display().to_string()),
        "logfile error omitted the requested path:\n{stdout}"
    );
}
