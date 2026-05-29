//! `heron config validate` — load and validate a config without
//! starting any pipelines. JSON-by-default so CI / agent gates can parse
//! the result; `--text` flips to a human-readable rendering.
//!
//! Exit codes:
//! - `0` — config loaded and `AppConfig::validate()` returned no issues
//! - `1` — config loaded, validation surfaced one or more `ConfigIssue`s
//! - `2` — config could not be discovered, read, or parsed (IO/parse error)

use std::path::{Path, PathBuf};

use clap::Args;
use h_common::config::{
    config_search_paths, discover_config_path, AnnotatedConfigIssue, AppConfig, IssueSeverity,
};

#[derive(Debug, Args)]
pub struct ValidateArgs {
    /// Render output as human-readable text instead of JSON (the default).
    #[arg(long)]
    pub text: bool,
}

pub fn run(config_arg: Option<&Path>, args: &ValidateArgs) -> i32 {
    let path = match resolve_config_path(config_arg) {
        Ok(p) => p,
        Err(err) => {
            emit_io_error(args.text, &err);
            return 2;
        }
    };

    let cfg = match AppConfig::load(&path) {
        Ok(c) => c,
        Err(e) => {
            emit_parse_error(args.text, &path, &e.to_string());
            return 2;
        }
    };

    let issues = cfg.validate();
    let error_count = issues
        .iter()
        .filter(|i| i.severity() == IssueSeverity::Error)
        .count();
    let warn_count = issues.len() - error_count;
    let ok = error_count == 0;
    let annotated: Vec<AnnotatedConfigIssue<'_>> =
        issues.iter().map(AnnotatedConfigIssue::from).collect();

    if args.text {
        if issues.is_empty() {
            println!("ok    config valid: {}", path.display());
        } else {
            println!(
                "{}  {} error(s), {} warning(s) in {}:",
                if ok { "ok  " } else { "fail" },
                error_count,
                warn_count,
                path.display()
            );
            for i in &issues {
                let mark = match i.severity() {
                    IssueSeverity::Warn => "warn",
                    IssueSeverity::Error => "error",
                };
                println!("  [{mark}] {i}");
            }
        }
    } else {
        let v = serde_json::json!({
            "ok": ok,
            "config_path": path.display().to_string(),
            "issues": annotated,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&v).expect("serialize validate result")
        );
    }

    if ok {
        0
    } else {
        1
    }
}

enum ResolveError {
    NotAFile(PathBuf),
    NoneFound(Vec<PathBuf>),
}

fn resolve_config_path(config_arg: Option<&Path>) -> Result<PathBuf, ResolveError> {
    match config_arg {
        Some(p) => {
            if p.is_file() {
                Ok(p.to_path_buf())
            } else {
                Err(ResolveError::NotAFile(p.to_path_buf()))
            }
        }
        None => match discover_config_path() {
            Some(p) => Ok(p),
            None => Err(ResolveError::NoneFound(config_search_paths())),
        },
    }
}

fn emit_io_error(text: bool, err: &ResolveError) {
    match err {
        ResolveError::NotAFile(p) => {
            if text {
                eprintln!("fail  config file not found: {}", p.display());
            } else {
                let v = serde_json::json!({
                    "ok": false,
                    "error": "io",
                    "detail": format!("config file not found: {}", p.display()),
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            }
        }
        ResolveError::NoneFound(searched) => {
            if text {
                eprintln!("fail  no configuration file found. Searched (in order):");
                for p in searched {
                    eprintln!("  - {}", p.display());
                }
            } else {
                let searched: Vec<String> =
                    searched.iter().map(|p| p.display().to_string()).collect();
                let v = serde_json::json!({
                    "ok": false,
                    "error": "discovery",
                    "detail": "no configuration file found",
                    "searched": searched,
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            }
        }
    }
}

fn emit_parse_error(text: bool, path: &Path, detail: &str) {
    if text {
        eprintln!("fail  parse error in {}: {detail}", path.display());
    } else {
        let v = serde_json::json!({
            "ok": false,
            "config_path": path.display().to_string(),
            "error": "parse",
            "detail": detail,
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    }
}
