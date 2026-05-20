//! Read-side counterpart to `ts-capture::pcap_dump`. Scans
//! `<base>/<pipeline>/<source_id>/<minute>.pcap[.snappy]` trees, filters
//! by 5-tuple + time window, and emits a single uncompressed pcap byte
//! stream suitable for HTTP download.

pub mod candidates;
pub mod filter;
pub mod format;
pub mod merge;
pub mod output;
pub mod reader;
pub mod types;

pub use types::{ExtractError, ExtractFlow, ExtractRequest, ExtractRequestSet, PipelineRoot};

use std::io;
use std::sync::Arc;

use bytes::Bytes;
use futures::stream::{poll_fn, Stream};

use crate::candidates::list_candidate_files;
use crate::format::DEFAULT_EMPTY_LINK_TYPE;
use crate::merge::MergeIter;
use crate::output::{global_header, record_header};
use crate::reader::{PacketIter, RawRec};

// ---------------------------------------------------------------------------
// ChunkIter — yields one Bytes chunk at a time: first the global header,
// then one (record-header + data) chunk per matched record. Backed by the
// synchronous MergeIter so each call to `next` does at most one pcap read.
// ---------------------------------------------------------------------------

enum ChunkState {
    NeedHeader,
    Records,
}

struct ChunkIter {
    state: ChunkState,
    merge: MergeIter,
    link_type: u32,
}

impl ChunkIter {
    fn new(merge: MergeIter, link_type: u32) -> Self {
        Self {
            state: ChunkState::NeedHeader,
            merge,
            link_type,
        }
    }
}

impl Iterator for ChunkIter {
    type Item = io::Result<Bytes>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.state {
            ChunkState::NeedHeader => {
                self.state = ChunkState::Records;
                Some(Ok(Bytes::copy_from_slice(&global_header(self.link_type))))
            }
            ChunkState::Records => {
                let rec: RawRec = self.merge.next()?;
                // Concatenate 16-byte record header + data into a single chunk.
                // This halves the number of Stream items vs yielding them
                // separately and matches what HTTP body writers prefer.
                let mut buf = Vec::with_capacity(16 + rec.data.len());
                buf.extend_from_slice(&record_header(&rec));
                buf.extend_from_slice(&rec.data);
                Some(Ok(Bytes::from(buf)))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API — split into a synchronous `prepare` (open files + validate
// link_type) and `stream_extract` (drives the iterator on a dedicated
// blocking thread, feeding the response via an mpsc channel).
//
// The split lets the HTTP handler:
//   1. Surface link_type mismatches as a clean 500 BEFORE any 200 OK body
//      goes out (the spec requires this; prior single-function `extract()`
//      could only emit the error mid-stream).
//   2. Keep all sync I/O off the Tokio worker driving the response body
//      write loop — file opens AND per-record reads run on blocking threads.
// ---------------------------------------------------------------------------

/// Output of the synchronous prepare phase. Cheap to compute (a directory
/// scan + one 24-byte read per candidate file). Construct via [`prepare`].
pub struct Prep {
    iters: Vec<PacketIter>,
    header_link_type: u32,
    req: Arc<ExtractRequestSet>,
}

/// Synchronous prepare: list candidate files, open them, validate that all
/// candidates share the same `link_type`. Returns `Err` for the rare
/// link_type mismatch so the HTTP handler can respond with 500 *before*
/// any 200 OK body is sent.
///
/// Although prepare itself is cheap, callers in an async context should
/// still wrap it in `tokio::task::spawn_blocking` — short reads on a slow
/// disk can still stall a runtime worker.
pub fn prepare(req: ExtractRequest, roots: &[PipelineRoot]) -> Result<Prep, ExtractError> {
    prepare_many(ExtractRequestSet::from(req), roots)
}

/// Synchronous prepare for a request containing multiple time-bounded flows.
/// This is used by turn-level extraction, where each LLM call contributes
/// its exact observed TCP 5-tuple.
pub fn prepare_many(req: ExtractRequestSet, roots: &[PipelineRoot]) -> Result<Prep, ExtractError> {
    let files = list_candidate_files(&req, roots);

    let mut iters: Vec<PacketIter> = Vec::with_capacity(files.len());
    for f in &files {
        match PacketIter::open(f) {
            Ok(it) => iters.push(it),
            Err(e) => {
                tracing::warn!(
                    path = %f.path.display(),
                    error = %e,
                    "pcap-extract: failed to open candidate; skipping"
                );
            }
        }
    }

    let header_link_type = match iters.first() {
        Some(first) => {
            let lt = first.link_type;
            for it in iters.iter().skip(1) {
                if it.link_type != lt {
                    return Err(ExtractError::LinkTypeMismatch {
                        expected: lt,
                        got: it.link_type,
                    });
                }
            }
            lt
        }
        None => DEFAULT_EMPTY_LINK_TYPE,
    };

    Ok(Prep {
        iters,
        header_link_type,
        req: Arc::new(req),
    })
}

/// Drive the prepared extraction on a dedicated blocking thread, yielding
/// pcap byte chunks through an mpsc channel. The channel back-pressures
/// naturally — `blocking_send` waits for the consumer to drain, so peak
/// memory is bounded by `CHANNEL_CAP × max chunk size`.
///
/// Must be called from inside a Tokio runtime (uses `spawn_blocking`).
/// Returns immediately; actual file I/O happens on the spawned task.
pub fn stream_extract(prep: Prep) -> impl Stream<Item = io::Result<Bytes>> + Send + 'static {
    const CHANNEL_CAP: usize = 64;

    let merge = MergeIter::new(prep.iters, prep.req);
    let chunks = ChunkIter::new(merge, prep.header_link_type);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(CHANNEL_CAP);
    tokio::task::spawn_blocking(move || {
        for chunk in chunks {
            // `blocking_send` returns Err iff the receiver was dropped,
            // i.e. the HTTP client disconnected. Stop quietly; no warning
            // needed (it's the normal shutdown path).
            if tx.blocking_send(chunk).is_err() {
                break;
            }
        }
    });

    poll_fn(move |cx| rx.poll_recv(cx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use futures::StreamExt;
    use std::path::Path;
    use tempfile::tempdir;
    use tokio::runtime::Runtime;

    async fn collect_stream(s: impl Stream<Item = io::Result<Bytes>>) -> Vec<u8> {
        futures::pin_mut!(s);
        let mut out = BytesMut::new();
        while let Some(item) = s.next().await {
            out.extend_from_slice(&item.unwrap());
        }
        out.to_vec()
    }

    fn write_one_record_file(dir: &Path, source: &str) -> std::path::PathBuf {
        use std::fs;
        let src_dir = dir.join(format!("local/{source}"));
        fs::create_dir_all(&src_dir).unwrap();
        let mut frame = Vec::new();
        frame.extend_from_slice(&[0u8; 12]);
        frame.extend_from_slice(&[0x08, 0x00]);
        let ip_total_len: u16 = 40;
        let mut ip = vec![0u8; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&ip_total_len.to_be_bytes());
        ip[8] = 64;
        ip[9] = 6;
        ip[12..16].copy_from_slice(&[10, 0, 0, 1]);
        ip[16..20].copy_from_slice(&[1, 2, 3, 4]);
        frame.extend_from_slice(&ip);
        let mut tcp = vec![0u8; 20];
        tcp[0..2].copy_from_slice(&54321u16.to_be_bytes());
        tcp[2..4].copy_from_slice(&443u16.to_be_bytes());
        tcp[12] = 0x50;
        tcp[13] = 0x10;
        frame.extend_from_slice(&tcp);
        let path = src_dir.join("19700101T0000.pcap");
        let mut f = std::fs::File::create(&path).unwrap();
        std::io::Write::write_all(&mut f, &output::global_header(1)).unwrap();
        let rec = reader::RawRec {
            ts_us: 1_000_000,
            caplen: frame.len() as u32,
            wirelen: frame.len() as u32,
            data: Bytes::from(frame.clone()),
        };
        std::io::Write::write_all(&mut f, &output::record_header(&rec)).unwrap();
        std::io::Write::write_all(&mut f, &frame).unwrap();
        path
    }

    #[test]
    fn header_only_when_no_files() {
        let rt = Runtime::new().unwrap();
        let dir = tempdir().unwrap();
        let req = ExtractRequest {
            source_id: "missing".into(),
            start_us: 0,
            end_us: 30_000_000,
            client_ip: None,
            client_port: None,
            server_ip: None,
            server_port: None,
        };
        let roots = vec![PipelineRoot {
            name: "local".into(),
            dump_dir: dir.path().to_path_buf(),
        }];
        let bytes = rt.block_on(async {
            let prep = prepare(req, &roots).unwrap();
            collect_stream(stream_extract(prep)).await
        });
        assert_eq!(bytes.len(), 24);
        assert_eq!(&bytes[0..4], &format::PCAP_MAGIC.to_le_bytes());
        assert_eq!(&bytes[20..24], &1u32.to_le_bytes()); // default Ethernet
    }

    #[test]
    fn header_plus_records_when_match() {
        let rt = Runtime::new().unwrap();
        let dir = tempdir().unwrap();
        let _ = write_one_record_file(dir.path(), "en0");
        let req = ExtractRequest {
            source_id: "en0".into(),
            start_us: 0,
            end_us: 30_000_000,
            client_ip: None,
            client_port: None,
            server_ip: None,
            server_port: None,
        };
        let roots = vec![PipelineRoot {
            name: "local".into(),
            dump_dir: dir.path().to_path_buf(),
        }];
        let bytes = rt.block_on(async {
            let prep = prepare(req, &roots).unwrap();
            collect_stream(stream_extract(prep)).await
        });
        assert!(bytes.len() > 24);
        assert_eq!(&bytes[0..4], &format::PCAP_MAGIC.to_le_bytes());
    }

    /// Files in adjacent minutes have different `link_type`s in their
    /// global headers — `prepare` must surface this as `Err` so the HTTP
    /// handler can reply 500 BEFORE any body goes on the wire.
    /// (Doesn't happen in production: pcap_dump pins link_type per source.)
    #[test]
    fn prepare_detects_link_type_mismatch() {
        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("local/en0");
        std::fs::create_dir_all(&src_dir).unwrap();

        let mut f0 = std::fs::File::create(src_dir.join("19700101T0000.pcap")).unwrap();
        std::io::Write::write_all(&mut f0, &output::global_header(1)).unwrap();
        drop(f0);
        let mut f1 = std::fs::File::create(src_dir.join("19700101T0001.pcap")).unwrap();
        std::io::Write::write_all(&mut f1, &output::global_header(101)).unwrap();
        drop(f1);

        let req = ExtractRequest {
            source_id: "en0".into(),
            start_us: 0,
            end_us: 120_000_000,
            client_ip: None,
            client_port: None,
            server_ip: None,
            server_port: None,
        };
        let roots = vec![PipelineRoot {
            name: "local".into(),
            dump_dir: dir.path().to_path_buf(),
        }];

        match prepare(req, &roots) {
            Err(ExtractError::LinkTypeMismatch { expected, got }) => {
                assert_eq!(expected, 1);
                assert_eq!(got, 101);
            }
            Err(other) => panic!("expected LinkTypeMismatch, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }
}
