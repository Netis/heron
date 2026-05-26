//! K-way merge of N `PacketIter`s into a single time-ordered iterator,
//! filtered through `Filter`. Yields `RawRec`s that pass the filter, in
//! strictly non-decreasing `ts_us` order.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

use crate::reader::{PacketIter, RawRec};

struct HeapEntry {
    ts_us: i64,
    file_idx: usize,
    rec: RawRec,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.ts_us == other.ts_us && self.file_idx == other.file_idx
    }
}
impl Eq for HeapEntry {}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.ts_us
            .cmp(&other.ts_us)
            .then(self.file_idx.cmp(&other.file_idx))
    }
}

pub struct MergeIter {
    iters: Vec<PacketIter>,
    heap: BinaryHeap<Reverse<HeapEntry>>,
    req: Arc<crate::types::ExtractRequest>,
    link_types: Vec<u32>,
}

impl MergeIter {
    pub fn new(iters: Vec<PacketIter>, req: Arc<crate::types::ExtractRequest>) -> Self {
        let link_types: Vec<u32> = iters.iter().map(|it| it.link_type).collect();
        let mut me = Self {
            iters,
            heap: BinaryHeap::new(),
            req,
            link_types,
        };
        for idx in 0..me.iters.len() {
            me.refill(idx);
        }
        me
    }

    fn refill(&mut self, idx: usize) {
        let lt = self.link_types[idx];
        let f = crate::filter::Filter {
            req: &self.req,
            link_type: lt,
        };
        for rec in self.iters[idx].by_ref() {
            if f.matches(&rec) {
                self.heap.push(Reverse(HeapEntry {
                    ts_us: rec.ts_us,
                    file_idx: idx,
                    rec,
                }));
                return;
            }
        }
    }
}

impl Iterator for MergeIter {
    type Item = RawRec;

    fn next(&mut self) -> Option<RawRec> {
        let Reverse(entry) = self.heap.pop()?;
        let HeapEntry { rec, file_idx, .. } = entry;
        self.refill(file_idx);
        Some(rec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidates::CandidateFile;
    use crate::output::{global_header, record_header};
    use bytes::Bytes;
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use tempfile::tempdir;

    fn write_file(path: &Path, link_type: u32, recs: &[(i64, &[u8])]) {
        let mut f = File::create(path).unwrap();
        f.write_all(&global_header(link_type)).unwrap();
        for (ts_us, data) in recs {
            // forge a record
            let rec = RawRec {
                ts_us: *ts_us,
                caplen: data.len() as u32,
                wirelen: data.len() as u32,
                data: Bytes::copy_from_slice(data),
            };
            f.write_all(&record_header(&rec)).unwrap();
            f.write_all(data).unwrap();
        }
    }

    fn ipv4_tcp_pkt(src: [u8; 4], sp: u16, dst: [u8; 4], dp: u16) -> Vec<u8> {
        // Reuse the helper from filter::tests via copy here so this module's
        // tests are self-contained.
        let mut frame = Vec::new();
        frame.extend_from_slice(&[0u8; 12]);
        frame.extend_from_slice(&[0x08, 0x00]);
        let ip_total_len: u16 = 40;
        let mut ip = vec![0u8; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&ip_total_len.to_be_bytes());
        ip[8] = 64;
        ip[9] = 6;
        ip[12..16].copy_from_slice(&src);
        ip[16..20].copy_from_slice(&dst);
        frame.extend_from_slice(&ip);
        let mut tcp = vec![0u8; 20];
        tcp[0..2].copy_from_slice(&sp.to_be_bytes());
        tcp[2..4].copy_from_slice(&dp.to_be_bytes());
        tcp[12] = 0x50;
        tcp[13] = 0x10;
        frame.extend_from_slice(&tcp);
        frame
    }

    #[test]
    fn merges_two_files_in_time_order() {
        let dir = tempdir().unwrap();
        let p1 = dir.path().join("a.pcap");
        let p2 = dir.path().join("b.pcap");
        let pkt = ipv4_tcp_pkt([10, 0, 0, 1], 1, [1, 2, 3, 4], 80);
        write_file(&p1, 1, &[(1_000_000, &pkt), (3_000_000, &pkt)]);
        write_file(&p2, 1, &[(2_000_000, &pkt), (4_000_000, &pkt)]);

        let req = crate::types::ExtractRequest {
            source_id: "x".into(),
            start_us: 0,
            end_us: 10_000_000,
            client_ip: None,
            client_port: None,
            server_ip: None,
            server_port: None,
        };
        let iters = vec![
            PacketIter::open(&CandidateFile {
                path: p1,
                compressed: false,
            })
            .unwrap(),
            PacketIter::open(&CandidateFile {
                path: p2,
                compressed: false,
            })
            .unwrap(),
        ];
        let timestamps: Vec<i64> = MergeIter::new(iters, std::sync::Arc::new(req))
            .map(|r| r.ts_us)
            .collect();
        assert_eq!(timestamps, vec![1_000_000, 2_000_000, 3_000_000, 4_000_000]);
    }

    #[test]
    fn skips_records_filtered_out() {
        let dir = tempdir().unwrap();
        let p1 = dir.path().join("a.pcap");
        let pkt_match = ipv4_tcp_pkt([10, 0, 0, 1], 1, [1, 2, 3, 4], 80);
        let pkt_other = ipv4_tcp_pkt([10, 0, 0, 2], 1, [1, 2, 3, 4], 80);
        write_file(&p1, 1, &[(1_000_000, &pkt_other), (2_000_000, &pkt_match)]);

        let req = crate::types::ExtractRequest {
            source_id: "x".into(),
            start_us: 0,
            end_us: 10_000_000,
            client_ip: "10.0.0.1".parse().ok(),
            client_port: None,
            server_ip: None,
            server_port: None,
        };
        let iters = vec![PacketIter::open(&CandidateFile {
            path: p1,
            compressed: false,
        })
        .unwrap()];
        let timestamps: Vec<i64> = MergeIter::new(iters, std::sync::Arc::new(req))
            .map(|r| r.ts_us)
            .collect();
        assert_eq!(timestamps, vec![2_000_000]);
    }
}
