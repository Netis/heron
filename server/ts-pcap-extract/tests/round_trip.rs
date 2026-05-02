//! End-to-end: write packets via the same on-disk format as
//! `ts-capture::pcap_dump`, then `extract()` them, then re-open the
//! resulting bytes with libpcap to confirm they're a valid `.pcap`.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use tempfile::tempdir;
use tokio::runtime::Runtime;

use ts_pcap_extract::output::{global_header, record_header};
use ts_pcap_extract::reader::RawRec;
use ts_pcap_extract::{prepare, stream_extract, ExtractRequest, PipelineRoot};

fn ipv4_tcp_pkt(src: [u8;4], sp: u16, dst: [u8;4], dp: u16) -> Vec<u8> {
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

fn write_minute_file(dir: &Path, label: &str, link_type: u32, recs: &[(i64, &[u8])]) {
    let path = dir.join(format!("{label}.pcap"));
    let mut f = File::create(&path).unwrap();
    f.write_all(&global_header(link_type)).unwrap();
    for (ts_us, data) in recs {
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

async fn collect(s: impl Stream<Item = std::io::Result<Bytes>>) -> Vec<u8> {
    futures::pin_mut!(s);
    let mut out = BytesMut::new();
    while let Some(item) = s.next().await {
        out.extend_from_slice(&item.unwrap());
    }
    out.to_vec()
}

#[test]
fn round_trip_libpcap_can_open_extract_output() {
    let rt = Runtime::new().unwrap();
    let base = tempdir().unwrap();
    let src_dir = base.path().join("local/en0");
    std::fs::create_dir_all(&src_dir).unwrap();

    let pkt_a = ipv4_tcp_pkt([10,0,0,1], 54321, [1,2,3,4], 443);
    let pkt_b = ipv4_tcp_pkt([1,2,3,4], 443, [10,0,0,1], 54321);
    write_minute_file(&src_dir, "19700101T0000", 1, &[
        (1_000_000, &pkt_a),
        (1_500_000, &pkt_b),
        (2_000_000, &pkt_a),
    ]);
    write_minute_file(&src_dir, "19700101T0001", 1, &[
        (60_500_000, &pkt_a),
    ]);

    let req = ExtractRequest {
        source_id: "en0".into(),
        start_us: 0,
        end_us: 120_000_000,
        client_ip: "10.0.0.1".parse().ok(),
        client_port: Some(54321),
        server_ip: "1.2.3.4".parse().ok(),
        server_port: Some(443),
    };
    let roots = vec![PipelineRoot { name: "local".into(), dump_dir: base.path().to_path_buf() }];
    let bytes = rt.block_on(async {
        let prep = prepare(req, &roots).unwrap();
        collect(stream_extract(prep)).await
    });

    // Spit to a temp file and feed to libpcap.
    let out_path = base.path().join("extract.pcap");
    std::fs::write(&out_path, &bytes).unwrap();
    let mut cap = pcap::Capture::from_file(&out_path).expect("libpcap opens result");
    let mut tss = Vec::new();
    while let Ok(p) = cap.next_packet() {
        // `tv_sec` and `tv_usec` are `i64` on macOS but `i32` on 32-bit Linux;
        // the `as i64` cast is needed for portability even though clippy
        // flags it as redundant on the host platform.
        #[allow(clippy::unnecessary_cast)]
        let ts = p.header.ts.tv_sec as i64 * 1_000_000 + p.header.ts.tv_usec as i64;
        tss.push(ts);
    }
    assert_eq!(tss, vec![1_000_000, 1_500_000, 2_000_000, 60_500_000]);
}
