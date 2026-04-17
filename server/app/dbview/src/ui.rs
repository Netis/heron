//! Terminal UI helpers: alt-screen, input, formatters.

use std::io::{self, Write};

/// Enter alternate screen buffer (like vim/top — main scrollback untouched)
pub fn enter_alt_screen() {
    print!("\x1B[?1049h\x1B[2J\x1B[H");
    io::stdout().flush().unwrap();
}

/// Leave alternate screen buffer (restores original terminal content)
pub fn leave_alt_screen() {
    print!("\x1B[?1049l");
    io::stdout().flush().unwrap();
}

/// Clear the alternate screen (cursor home + clear)
pub fn clear_screen() {
    print!("\x1B[2J\x1B[H");
    io::stdout().flush().unwrap();
}

pub fn read_line(prompt: &str) -> String {
    print!("{prompt}");
    io::stdout().flush().unwrap();
    let mut buf = String::new();
    io::stdin().read_line(&mut buf).unwrap();
    buf.trim().to_string()
}

pub fn fmt_opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "-".into())
}

pub fn fmt_f64(v: Option<f64>) -> String {
    v.map(|f| format!("{f:.1}")).unwrap_or_else(|| "-".into())
}

pub fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

pub fn fmt_duration_ms(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{:.1}m", ms as f64 / 60_000.0)
    } else if ms >= 1_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{ms}ms")
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
        out.push_str("...");
        out
    }
}

pub fn pretty_json(s: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(v) => serde_json::to_string_pretty(&v)
            .unwrap_or_else(|_| s.to_string())
            .lines()
            .map(|l| format!("    {l}"))
            .collect::<Vec<_>>()
            .join("\n"),
        Err(_) => format!("    {s}"),
    }
}

/// RAII guard: leaves alternate screen when dropped (normal exit or panic)
pub struct AltScreenGuard;

impl AltScreenGuard {
    pub fn new() -> Self {
        enter_alt_screen();
        Self
    }
}

impl Drop for AltScreenGuard {
    fn drop(&mut self) {
        leave_alt_screen();
    }
}
