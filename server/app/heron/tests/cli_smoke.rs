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

/// `aglake-props` is pure (no config, no I/O) — exercises its `run` end to end.
#[test]
fn aglake_props_bare_emits_sourcetype_stanzas() {
    let out = Command::new(bin())
        .args(["aglake-props", "--bare"])
        .output()
        .expect("spawn heron aglake-props --bare");
    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("[sourcetype."),
        "expected sourcetype stanzas, got: {stdout}"
    );
}

/// `config validate` on an unreadable path is the IO-error branch (exit 2).
#[test]
fn config_validate_on_missing_file_exits_2() {
    let out = Command::new(bin())
        .args([
            "-c",
            "/nonexistent/heron-does-not-exist.toml",
            "config",
            "validate",
        ])
        .output()
        .expect("spawn heron config validate");
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit 2 for a missing config"
    );
}

/// `config validate` on the shipped default config must load + validate
/// (exit 0 or 1 for issues — never the exit-2 parse/IO class).
#[test]
fn config_validate_accepts_shipped_default() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let cfg = format!("{manifest}/../../config/default.toml");
    let out = Command::new(bin())
        .args(["-c", &cfg, "config", "validate"])
        .output()
        .expect("spawn heron config validate");
    let code = out.status.code();
    assert!(
        code == Some(0) || code == Some(1),
        "default config should load; got exit {code:?}, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ok") || stdout.contains("issues"),
        "expected a JSON validation report, got: {stdout}"
    );
}

/// `doctor` is self-contained; must emit its JSON report and exit 0/1.
#[test]
fn doctor_emits_json_report() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let cfg = format!("{manifest}/../../config/default.toml");
    let out = Command::new(bin())
        .args(["-c", &cfg, "doctor"])
        .output()
        .expect("spawn heron doctor");
    let code = out.status.code();
    assert!(
        code == Some(0) || code == Some(1),
        "doctor should report, got exit {code:?}, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("checks") || stdout.contains("ok"),
        "expected a doctor JSON report, got: {stdout}"
    );
}
