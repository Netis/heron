//! Walk every supplied pipeline root, find `<root>/<pipeline>/<source_id>/`
//! if it exists, then enumerate `<minute_label>.pcap` and
//! `<minute_label>.pcap.snappy` for each minute key in the request window.

use std::path::{Path, PathBuf};

use ts_common::path::sanitize_path_component;

use crate::format::{minute_label, MICROS_PER_MINUTE};
use crate::types::{ExtractRequest, PipelineRoot};

/// One physical file we will read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateFile {
    pub path: PathBuf,
    pub compressed: bool,
}

pub fn list_candidate_files(req: &ExtractRequest, roots: &[PipelineRoot]) -> Vec<CandidateFile> {
    let safe_source = match sanitize_path_component(&req.source_id) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let minute_lo = req.start_us.div_euclid(MICROS_PER_MINUTE);
    let minute_hi = req.end_us.div_euclid(MICROS_PER_MINUTE);

    let mut out = Vec::new();
    for root in roots {
        let safe_pipeline = match sanitize_path_component(&root.name) {
            Some(s) => s,
            None => continue,
        };
        let src_dir = root.dump_dir.join(safe_pipeline).join(&safe_source);
        if !src_dir.is_dir() {
            continue;
        }
        for k in minute_lo..=minute_hi {
            let label = minute_label(k);
            push_if_file(&mut out, &src_dir, &label, ".pcap", false);
            push_if_file(&mut out, &src_dir, &label, ".pcap.snappy", true);
        }
    }
    out
}

fn push_if_file(out: &mut Vec<CandidateFile>, dir: &Path, label: &str, ext: &str, compressed: bool) {
    let path = dir.join(format!("{label}{ext}"));
    if path.is_file() {
        out.push(CandidateFile { path, compressed });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn req(source_id: &str, start_us: i64, end_us: i64) -> ExtractRequest {
        ExtractRequest {
            source_id: source_id.into(),
            start_us,
            end_us,
            client_ip: None,
            client_port: None,
            server_ip: None,
            server_port: None,
        }
    }

    fn root(name: &str, dir: &Path) -> PipelineRoot {
        PipelineRoot { name: name.into(), dump_dir: dir.to_path_buf() }
    }

    #[test]
    fn lists_plain_and_snappy_in_same_minute() {
        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("local/en0");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("19700101T0000.pcap"), b"x").unwrap();
        fs::write(src_dir.join("19700101T0000.pcap.snappy"), b"x").unwrap();

        let roots = vec![root("local", dir.path())];
        let files = list_candidate_files(&req("en0", 0, 30_000_000), &roots);
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|c| !c.compressed));
        assert!(files.iter().any(|c| c.compressed));
    }

    #[test]
    fn skips_missing_minutes() {
        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("local/en0");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("19700101T0000.pcap"), b"x").unwrap();
        // 19700101T0001.pcap intentionally missing
        fs::write(src_dir.join("19700101T0002.pcap"), b"x").unwrap();

        let roots = vec![root("local", dir.path())];
        let files = list_candidate_files(&req("en0", 0, 121_000_000), &roots);
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn scans_multiple_pipelines_for_same_source_id() {
        let dir = tempdir().unwrap();
        for pipeline in &["alpha", "beta"] {
            let src_dir = dir.path().join(format!("{pipeline}/en0"));
            fs::create_dir_all(&src_dir).unwrap();
            fs::write(src_dir.join("19700101T0000.pcap"), b"x").unwrap();
        }
        let roots = vec![root("alpha", dir.path()), root("beta", dir.path())];
        let files = list_candidate_files(&req("en0", 0, 30_000_000), &roots);
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|c| c.path.to_string_lossy().contains("alpha/en0")));
        assert!(files.iter().any(|c| c.path.to_string_lossy().contains("beta/en0")));
    }

    #[test]
    fn missing_source_dir_yields_empty() {
        let dir = tempdir().unwrap();
        let roots = vec![root("local", dir.path())];
        let files = list_candidate_files(&req("nope", 0, 30_000_000), &roots);
        assert!(files.is_empty());
    }

    #[test]
    fn unsafe_source_id_yields_empty() {
        let dir = tempdir().unwrap();
        let roots = vec![root("local", dir.path())];
        let files = list_candidate_files(&req("..", 0, 30_000_000), &roots);
        assert!(files.is_empty());
    }
}
