//! Wire-equivalence differential — the correctness keystone for the distributed
//! eBPF capture topology.
//!
//! The design's central claim is that the central collector runs the *exact same*
//! downstream pipeline as a local eBPF source — the split is at `RawPacket`, so
//! the bytes are identical. This test proves it: replay each corpus fixture two
//! ways and assert the extracted turns/calls are identical.
//!
//!   (a) LOCAL:        pcap → pipeline                    (today's path)
//!   (b) DISTRIBUTED:  pcap → ProbeUplink ──mTLS──▶ ThinProbeSource → pipeline
//!
//! Both feed the *same* pipeline graph (`common::build_pipeline`) via the same
//! `PcapFileSource`, so any difference is attributable to the wire alone. The
//! projection is `source_id`-free (the central restamps `source_id`, which can't
//! affect turns/calls — flow grouping keys on the 5-tuple inside the bytes), so
//! the assertion needs no normalization. There is no golden here — the existing
//! corpus goldens are the transitive ground truth; this is a pure differential.
//!
//! LFS/absent fixtures are skipped exactly as in `corpus_golden.rs`.

mod common;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distributed_path_is_byte_equivalent_to_local() {
    let mut ran = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for (id, file) in common::active_fixtures() {
        // Both paths need the fixture; skip together if absent / LFS pointer.
        let Some(local) = common::run_pcap_collecting_calls(&file).await else {
            eprintln!("skip {id}: fixture {file} not present");
            skipped += 1;
            continue;
        };
        let Some(dist) = common::run_distributed(&file, false).await else {
            skipped += 1;
            continue;
        };
        ran += 1;

        let (lt, lc) = common::project(&local);
        let (dt, dc) = common::project(&dist);
        if lt.len() != dt.len() || lc.len() != dc.len() {
            failures.push(format!(
                "{id}: count drift — local {} turns/{} calls vs distributed {} turns/{} calls",
                lt.len(),
                lc.len(),
                dt.len(),
                dc.len()
            ));
            continue;
        }
        if lt != dt {
            failures.push(format!(
                "{id}: TURNS differ between local and distributed paths"
            ));
        }
        if lc != dc {
            failures.push(format!(
                "{id}: CALLS differ between local and distributed paths"
            ));
        }
    }

    eprintln!("wire-equivalence: ran={ran} skipped={skipped}");
    assert!(
        failures.is_empty(),
        "the distributed (probe→wire→central) path is NOT byte-equivalent to local capture:\n{}",
        failures.join("\n")
    );
}
