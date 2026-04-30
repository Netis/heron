//! `tokenscope doctor` — pre-flight diagnostics for an installed TokenScope.
//! Self-contained (does not talk to a running instance); each check is
//! independent so a single failure doesn't mask later checks.
//!
//! JSON-by-default so AI agents / CI gates can parse the result; `--text`
//! flips to a column-aligned human rendering.
//!
//! Exit codes:
//! - `0` — every check is `pass` or `warn`
//! - `1` — at least one check is `fail`

use std::path::Path;

use clap::Args;
use serde::Serialize;
use ts_common::config::{
    config_search_paths, discover_config_path, AppConfig, ConfigIssue, IssueSeverity,
};

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Render output as human-readable text instead of JSON (the default).
    #[arg(long)]
    pub text: bool,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    ok: bool,
    checks: Vec<DoctorCheck>,
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: &'static str,
    status: DoctorStatus,
    detail: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum DoctorStatus {
    Pass,
    Warn,
    Fail,
}

impl DoctorCheck {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: DoctorStatus::Pass,
            detail: detail.into(),
        }
    }
    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: DoctorStatus::Warn,
            detail: detail.into(),
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: DoctorStatus::Fail,
            detail: detail.into(),
        }
    }
}

pub async fn run(config_arg: Option<&Path>, args: &DoctorArgs) -> i32 {
    let mut checks: Vec<DoctorCheck> = Vec::new();

    let cfg = collect_config_checks(config_arg, &mut checks);

    checks.push(check_capture_capabilities());

    if let Some(cfg) = &cfg {
        checks.push(check_storage_path(cfg));
        checks.push(check_api_bind(cfg).await);
    } else {
        checks.push(DoctorCheck::warn("storage.path", "skipped (no config)"));
        checks.push(DoctorCheck::warn("api.bind", "skipped (no config)"));
    }

    checks.push(check_console_embedded());

    let ok = !checks.iter().any(|c| c.status == DoctorStatus::Fail);
    let report = DoctorReport { ok, checks };

    if args.text {
        for c in &report.checks {
            let mark = match c.status {
                DoctorStatus::Pass => "ok  ",
                DoctorStatus::Warn => "warn",
                DoctorStatus::Fail => "fail",
            };
            println!("{mark}  {:<28}  {}", c.name, c.detail);
        }
        println!("\noverall: {}", if report.ok { "ok" } else { "fail" });
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("serialize doctor report")
        );
    }

    if report.ok {
        0
    } else {
        1
    }
}

fn collect_config_checks(
    config_arg: Option<&Path>,
    checks: &mut Vec<DoctorCheck>,
) -> Option<AppConfig> {
    let path = match config_arg {
        Some(p) => {
            if p.is_file() {
                checks.push(DoctorCheck::pass(
                    "config.discovery",
                    p.display().to_string(),
                ));
                p.to_path_buf()
            } else {
                checks.push(DoctorCheck::fail(
                    "config.discovery",
                    format!("file not found: {}", p.display()),
                ));
                checks.push(DoctorCheck::warn("config.parse", "skipped (no config)"));
                checks.push(DoctorCheck::warn("config.validate", "skipped (no config)"));
                return None;
            }
        }
        None => match discover_config_path() {
            Some(p) => {
                checks.push(DoctorCheck::pass(
                    "config.discovery",
                    p.display().to_string(),
                ));
                p
            }
            None => {
                let searched: Vec<String> = config_search_paths()
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect();
                checks.push(DoctorCheck::fail(
                    "config.discovery",
                    format!("no config in cascade: {}", searched.join(", ")),
                ));
                checks.push(DoctorCheck::warn("config.parse", "skipped (no config)"));
                checks.push(DoctorCheck::warn("config.validate", "skipped (no config)"));
                return None;
            }
        },
    };

    match AppConfig::load(&path) {
        Ok(cfg) => {
            checks.push(DoctorCheck::pass("config.parse", "loaded"));
            let issues = cfg.validate();
            if issues.is_empty() {
                checks.push(DoctorCheck::pass("config.validate", "no issues"));
            } else {
                let any_error = issues
                    .iter()
                    .any(|i| i.severity() == IssueSeverity::Error);
                let detail = issues
                    .iter()
                    .map(ConfigIssue::to_string)
                    .collect::<Vec<_>>()
                    .join("; ");
                if any_error {
                    checks.push(DoctorCheck::fail("config.validate", detail));
                } else {
                    checks.push(DoctorCheck::warn("config.validate", detail));
                }
            }
            Some(cfg)
        }
        Err(e) => {
            checks.push(DoctorCheck::fail("config.parse", e.to_string()));
            checks.push(DoctorCheck::warn(
                "config.validate",
                "skipped (parse failed)",
            ));
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn check_capture_capabilities() -> DoctorCheck {
    // Linux capability bits (see capabilities(7)). Hex bitmask in
    // /proc/self/status `CapEff:` line.
    const CAP_NET_ADMIN_BIT: u64 = 12;
    const CAP_NET_RAW_BIT: u64 = 13;

    let content = match std::fs::read_to_string("/proc/self/status") {
        Ok(c) => c,
        Err(e) => {
            return DoctorCheck::warn(
                "capture.capabilities",
                format!("cannot read /proc/self/status: {e}"),
            );
        }
    };
    let line = match content.lines().find(|l| l.starts_with("CapEff:")) {
        Some(l) => l,
        None => {
            return DoctorCheck::warn(
                "capture.capabilities",
                "CapEff line missing from /proc/self/status",
            );
        }
    };
    let hex = line.trim_start_matches("CapEff:").trim();
    let bits = match u64::from_str_radix(hex, 16) {
        Ok(b) => b,
        Err(e) => {
            return DoctorCheck::warn(
                "capture.capabilities",
                format!("could not parse CapEff '{hex}': {e}"),
            );
        }
    };
    let has_raw = (bits >> CAP_NET_RAW_BIT) & 1 == 1;
    let has_admin = (bits >> CAP_NET_ADMIN_BIT) & 1 == 1;
    match (has_raw, has_admin) {
        (true, true) => DoctorCheck::pass(
            "capture.capabilities",
            "CAP_NET_RAW + CAP_NET_ADMIN both set",
        ),
        (true, false) => DoctorCheck::warn(
            "capture.capabilities",
            "CAP_NET_RAW set; CAP_NET_ADMIN missing (BPF filter set may fail)",
        ),
        (false, true) => DoctorCheck::warn(
            "capture.capabilities",
            "CAP_NET_ADMIN set; CAP_NET_RAW missing (live capture will fail)",
        ),
        (false, false) => DoctorCheck::fail(
            "capture.capabilities",
            "neither CAP_NET_RAW nor CAP_NET_ADMIN set; live capture requires sudo or `setcap cap_net_raw,cap_net_admin=eip <bin>`",
        ),
    }
}

#[cfg(target_os = "macos")]
fn check_capture_capabilities() -> DoctorCheck {
    // ChmodBPF (Wireshark) grants the user access to /dev/bpf*. Probe the
    // first 16 device nodes — if any opens read-only, we're good.
    use std::fs::OpenOptions;
    for i in 0..16 {
        let path = format!("/dev/bpf{i}");
        if !std::path::Path::new(&path).exists() {
            continue;
        }
        if OpenOptions::new().read(true).open(&path).is_ok() {
            return DoctorCheck::pass(
                "capture.capabilities",
                format!("BPF device readable: {path}"),
            );
        }
    }
    DoctorCheck::warn(
        "capture.capabilities",
        "no readable /dev/bpf* device; install ChmodBPF (bundled with Wireshark) or run with sudo",
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn check_capture_capabilities() -> DoctorCheck {
    DoctorCheck::warn(
        "capture.capabilities",
        "platform-specific capture-privilege check not implemented",
    )
}

fn check_storage_path(cfg: &AppConfig) -> DoctorCheck {
    if cfg.storage.backend != "duckdb" {
        return DoctorCheck::pass(
            "storage.path",
            format!("backend={}; path probe skipped", cfg.storage.backend),
        );
    }
    let path = Path::new(&cfg.storage.duckdb.path);
    if path.exists() {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        {
            Ok(_) => DoctorCheck::pass(
                "storage.path",
                format!("duckdb file rw: {}", path.display()),
            ),
            Err(e) => DoctorCheck::fail(
                "storage.path",
                format!("duckdb file not rw ({e}): {}", path.display()),
            ),
        }
    } else {
        // Reuse the validator's writability probe — already implemented
        // there, so doctor stays a thin orchestrator.
        let unwritable = cfg
            .validate()
            .iter()
            .any(|i| matches!(i, ConfigIssue::StoragePathParentUnwritable { .. }));
        if unwritable {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            DoctorCheck::fail(
                "storage.path",
                format!("duckdb parent dir not writable: {}", parent.display()),
            )
        } else {
            DoctorCheck::pass(
                "storage.path",
                format!("duckdb path creatable: {}", path.display()),
            )
        }
    }
}

async fn check_api_bind(cfg: &AppConfig) -> DoctorCheck {
    let addr = format!("{}:{}", cfg.api.listen, cfg.api.port);
    match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => {
            // Drop immediately so the running tokenscope can take this port.
            drop(listener);
            DoctorCheck::pass("api.bind", format!("port available: {addr}"))
        }
        Err(e) => DoctorCheck::fail("api.bind", format!("cannot bind {addr}: {e}")),
    }
}

#[cfg(feature = "console")]
fn check_console_embedded() -> DoctorCheck {
    let count = crate::console::Assets::iter().count();
    if count > 0 {
        DoctorCheck::pass(
            "console.embedded",
            format!("{count} static assets embedded"),
        )
    } else {
        DoctorCheck::fail(
            "console.embedded",
            "console feature on but Assets is empty (broken build)",
        )
    }
}

#[cfg(not(feature = "console"))]
fn check_console_embedded() -> DoctorCheck {
    DoctorCheck::warn(
        "console.embedded",
        "not compiled in (build with --features console for the embedded UI)",
    )
}
