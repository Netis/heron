//! Spawn-the-binary smoke tests — guard against the kind of clap regression
//! that's invisible to `cargo check` but breaks `heron --version` or
//! `heron <subcommand> --help`. Adding subcommands is exactly when
//! these stop working silently, so the test exists to catch that.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_heron")
}

#[test]
fn version_prints_pkg_version() {
    let out = Command::new(bin())
        .arg("--version")
        .output()
        .expect("spawn heron --version");
    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "expected '{}' in output, got: {stdout}",
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn help_lists_subcommands() {
    let out = Command::new(bin())
        .arg("--help")
        .output()
        .expect("spawn heron --help");
    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Both subcommands must appear in the help summary.
    assert!(
        stdout.contains("config") && stdout.contains("doctor"),
        "expected 'config' and 'doctor' in help output, got: {stdout}"
    );
    // Existing run-mode flags must still be listed.
    assert!(
        stdout.contains("--pcap-file") && stdout.contains("--interface"),
        "expected --pcap-file and --interface in help output, got: {stdout}"
    );
    // Batch-mode opt-out for pcap replay must stay surfaced in help so users
    // can find it after EOF when the process parks instead of exiting.
    assert!(
        stdout.contains("--exit-after-drain"),
        "expected --exit-after-drain in help output, got: {stdout}"
    );
}

#[test]
fn config_validate_help_is_reachable() {
    let out = Command::new(bin())
        .args(["config", "validate", "--help"])
        .output()
        .expect("spawn heron config validate --help");
    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--text"),
        "expected --text flag in 'config validate' help, got: {stdout}"
    );
}

#[test]
fn doctor_help_is_reachable() {
    let out = Command::new(bin())
        .args(["doctor", "--help"])
        .output()
        .expect("spawn heron doctor --help");
    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--text"),
        "expected --text flag in 'doctor' help, got: {stdout}"
    );
}
