//! Golden regression over the curated pcap corpus.
//!
//! Data-driven: reads `testdata/pcaps/corpus.toml`, replays each committed
//! fixture through the FULL pipeline (capture → protocol → llm → turn), projects
//! the extracted `LlmCall`/`AgentTurn` into a DETERMINISTIC JSON shape (no uuids,
//! no timing fields), and compares it to `testdata/pcaps/golden/<id>.json`.
//!
//! - Missing fixtures (or unsmudged git-LFS pointers) are SKIPPED, so the
//!   workspace still builds + tests green without `git lfs pull`.
//! - `HERON_BLESS_GOLDENS=1` (re)writes the goldens instead of asserting.
//!   Use `just corpus bless`.
//!
//! The replay harness + deterministic projection live in `tests/common/mod.rs`,
//! shared with `wire_equivalence.rs` (the local-vs-distributed differential).

mod common;

use std::path::PathBuf;

use common::{build_golden, pcaps_root, run_pcap_collecting_calls};

fn manifest_path() -> PathBuf {
    pcaps_root().join("corpus.toml")
}

fn golden_path(id: &str) -> PathBuf {
    pcaps_root().join("golden").join(format!("{id}.json"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corpus_goldens_match() {
    let bless = std::env::var("HERON_BLESS_GOLDENS").is_ok();

    let manifest_src =
        std::fs::read_to_string(manifest_path()).expect("testdata/pcaps/corpus.toml must exist");
    let manifest: toml::Value = toml::from_str(&manifest_src).expect("corpus.toml parses");
    let fixtures = manifest
        .get("fixture")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();

    let mut ran = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for fx in &fixtures {
        let id = fx.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let file = fx
            .get("file")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        assert!(!id.is_empty() && !file.is_empty(), "fixture needs id + file");

        // `status = "pending"` documents a target matrix cell whose capture
        // hasn't been obtained/scrubbed yet — listed for visibility, skipped.
        let status = fx
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("active");
        if status != "active" {
            eprintln!("skip {id}: status={status} (matrix cell pending capture)");
            skipped += 1;
            continue;
        }

        let Some((turns, calls)) = run_pcap_collecting_calls(&file).await else {
            eprintln!("skip {id}: fixture {file} not present (absent or LFS pointer)");
            skipped += 1;
            continue;
        };
        ran += 1;

        let golden = build_golden(&turns, &calls);
        let got = serde_json::to_string_pretty(&golden).unwrap();
        let gp = golden_path(&id);

        if bless {
            std::fs::create_dir_all(gp.parent().unwrap()).unwrap();
            std::fs::write(&gp, format!("{got}\n")).unwrap();
            eprintln!("blessed {id} -> {}", gp.display());
            continue;
        }

        let want = match std::fs::read_to_string(&gp) {
            Ok(s) => s,
            Err(_) => {
                failures.push(format!(
                    "{id}: missing golden {} (run `just corpus bless`)",
                    gp.display()
                ));
                continue;
            }
        };
        if want.trim_end() != got.trim_end() {
            failures.push(format!(
                "{id}: golden mismatch — extracted output differs from {}.\n\
                 If the parser change is intentional, run `just corpus bless` and review the diff.",
                gp.display()
            ));
        }

        // Belt-and-suspenders: assert the manifest's human-readable contract.
        if let Some(expect) = fx.get("expect") {
            if let Some(tc) = expect.get("turn_count").and_then(|v| v.as_integer()) {
                if turns.len() as i64 != tc {
                    failures.push(format!(
                        "{id}: manifest turn_count={tc} but extracted {}",
                        turns.len()
                    ));
                }
            }
        }
    }

    eprintln!("corpus goldens: ran={ran} skipped={skipped} bless={bless}");
    assert!(
        failures.is_empty(),
        "corpus golden failures:\n{}",
        failures.join("\n")
    );
}
