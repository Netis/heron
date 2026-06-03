//! Surgical TOML rewrites for the Settings UI.
//!
//! Patches a specific `[[pipeline]]`'s `[[pipeline.sources]]` array in the
//! on-disk config without rewriting unrelated sections. `toml_edit` preserves
//! comments and ordering outside the rewritten array; comments *inside* the
//! sources array are dropped (acceptable — users edit through the UI, not
//! by hand, after this hits production).

use std::path::Path;

use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

use crate::config::CaptureSourceConfig;

#[derive(Debug, thiserror::Error)]
pub enum ConfigEditError {
    #[error("read config file: {0}")]
    Read(std::io::Error),
    #[error("parse TOML: {0}")]
    Parse(toml_edit::TomlError),
    #[error("write config file: {0}")]
    Write(std::io::Error),
    #[error("pipeline '{0}' not found in config")]
    PipelineNotFound(String),
    #[error("config root has no [[pipeline]] array (config-mode disabled?)")]
    NoPipelineArray,
}

/// Replace `[[pipeline.sources]]` for the pipeline named `pipeline_name`.
/// All existing source entries for that pipeline are dropped; the new list
/// is written in order.
///
/// Atomic via write-tmp + rename: a partially-written config cannot ever
/// be observed by the loader on the next start.
pub fn patch_pipeline_sources(
    path: &Path,
    pipeline_name: &str,
    sources: &[CaptureSourceConfig],
) -> Result<(), ConfigEditError> {
    let toml = std::fs::read_to_string(path).map_err(ConfigEditError::Read)?;
    let mut doc: DocumentMut = toml.parse().map_err(ConfigEditError::Parse)?;

    let pipelines = doc
        .get_mut("pipeline")
        .and_then(|i| i.as_array_of_tables_mut())
        .ok_or(ConfigEditError::NoPipelineArray)?;

    let mut found = false;
    for pipeline in pipelines.iter_mut() {
        let name_matches = pipeline
            .get("name")
            .and_then(|i| i.as_str())
            .map(|n| n == pipeline_name)
            .unwrap_or(false);
        if !name_matches {
            continue;
        }
        // Remove existing sources entirely (both [[pipeline.sources]] AOT
        // and any inline `sources = [...]` form).
        pipeline.remove("sources");
        let mut aot = ArrayOfTables::new();
        for s in sources {
            aot.push(source_to_table(s));
        }
        pipeline.insert("sources", Item::ArrayOfTables(aot));
        found = true;
        break;
    }
    if !found {
        return Err(ConfigEditError::PipelineNotFound(pipeline_name.to_string()));
    }

    // Atomic write: tmp file in the same dir → rename. Same-dir rename is
    // atomic on POSIX, so a crash mid-write leaves the original intact.
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("heron-config")
    ));
    std::fs::write(&tmp, doc.to_string()).map_err(ConfigEditError::Write)?;
    std::fs::rename(&tmp, path).map_err(ConfigEditError::Write)?;
    Ok(())
}

fn source_to_table(s: &CaptureSourceConfig) -> Table {
    let mut t = Table::new();
    match s {
        CaptureSourceConfig::Pcap {
            interface,
            bpf_filter,
            snaplen,
            source_id,
        } => {
            t["type"] = value("pcap");
            t["interface"] = value(interface.as_str());
            if let Some(bpf) = bpf_filter {
                t["bpf_filter"] = value(bpf.as_str());
            }
            t["snaplen"] = value(i64::from(*snaplen));
            if let Some(id) = source_id {
                t["source_id"] = value(id.as_str());
            }
        }
        CaptureSourceConfig::PcapFile {
            path,
            realtime,
            source_id,
            loop_count,
            loop_secs,
            rate_pps,
        } => {
            t["type"] = value("pcap-file");
            t["path"] = value(path.as_str());
            t["realtime"] = value(*realtime);
            if let Some(id) = source_id {
                t["source_id"] = value(id.as_str());
            }
            // Loop/duration/rate replay knobs are soak-only — emit them only
            // when non-default so a normal capture source's TOML stays clean.
            if *loop_count != 1 {
                t["loop_count"] = value(i64::from(*loop_count));
            }
            if *loop_secs != 0 {
                t["loop_secs"] = value(*loop_secs as i64);
            }
            if *rate_pps != 0 {
                t["rate_pps"] = value(i64::from(*rate_pps));
            }
        }
        CaptureSourceConfig::CloudProbe { endpoint, recv_hwm } => {
            t["type"] = value("cloud-probe");
            t["endpoint"] = value(endpoint.as_str());
            t["recv_hwm"] = value(i64::from(*recv_hwm));
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CaptureSourceConfig;

    fn write_tmp(content: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn rewrites_sources_preserves_other_fields() {
        let input = r#"# top comment
[[pipeline]]
name = "local"
dispatcher_count = 2
flow_shard_count = 4

[[pipeline.sources]]
type = "pcap"
interface = "eth0"
snaplen = 65535

[pipeline.turn]
idle_timeout_secs = 600

[storage]
backend = "duckdb"
"#;
        let f = write_tmp(input);
        let new = vec![CaptureSourceConfig::Pcap {
            interface: "any".to_string(),
            bpf_filter: Some("tcp port 4210".to_string()),
            snaplen: 262_144,
            source_id: None,
        }];
        patch_pipeline_sources(f.path(), "local", &new).unwrap();
        let out = std::fs::read_to_string(f.path()).unwrap();
        // dispatcher_count + pipeline.turn + storage are still there
        assert!(out.contains("dispatcher_count = 2"), "out:\n{out}");
        assert!(out.contains("[pipeline.turn]"), "out:\n{out}");
        assert!(out.contains("backend = \"duckdb\""), "out:\n{out}");
        // sources have been replaced
        assert!(out.contains("interface = \"any\""), "out:\n{out}");
        assert!(
            out.contains("bpf_filter = \"tcp port 4210\""),
            "out:\n{out}"
        );
        assert!(!out.contains("interface = \"eth0\""), "out:\n{out}");
    }

    #[test]
    fn unknown_pipeline_errors() {
        let input = r#"
[[pipeline]]
name = "local"
[[pipeline.sources]]
type = "pcap"
interface = "eth0"
"#;
        let f = write_tmp(input);
        let err = patch_pipeline_sources(f.path(), "nope", &[]).unwrap_err();
        assert!(matches!(err, ConfigEditError::PipelineNotFound(_)));
    }
}
