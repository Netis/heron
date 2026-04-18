//! Heartbeat timing constants + the cloud-probe per-uuid tracker.
//!
//! `pcap-live` uses only the constants — its single-stream logic lives
//! inline in the capture loop. `cloud-probe` uses `HeartbeatTracker` because
//! it multiplexes many uuids on one socket.

use std::collections::HashMap;

use crate::packet::RawPacket;

/// Interval between heartbeats, event-time microseconds. Hard-coded: the
/// finest downstream window is 1 s.
pub const HEARTBEAT_INTERVAL_US: i64 = 1_000_000;

/// Subtracted from `wall_clock_us()` when pcap-live emits an idle
/// heartbeat so a kernel-stamped packet arriving immediately after cannot
/// appear with a smaller `ts` than the HB we just emitted. pcap-live-only.
pub const SAFETY_MARGIN_US: i64 = 10_000;

/// Per-uuid heartbeat tracker for cloud-probe. Event-time only — no wall
/// clock. Upstream HBs and locally-synthesized HBs are treated identically:
/// both just bump `last_hb_ts`. As long as *something* keeps that counter
/// within `HEARTBEAT_INTERVAL_US`, no local HB is synthesized. If the
/// counter lags (upstream absent or stopped), the next real packet
/// triggers a local HB at the packet's own timestamp.
#[derive(Debug, Default)]
pub struct HeartbeatTracker {
    last_hb_ts: HashMap<String, i64>,
}

impl HeartbeatTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inspect `pkt`, possibly return a synthesized heartbeat the caller
    /// must forward *before* `pkt`.
    ///
    /// Rules:
    /// * If `pkt.is_heartbeat()`: bump `last_hb_ts[stream] = max(…, pkt.ts)`
    ///   and return `None` (caller still forwards the upstream HB).
    /// * Else if the stream has no entry yet: initialize `last_hb_ts =
    ///   pkt.ts` and return `None` (baseline, no synthesis).
    /// * Else if `pkt.ts - last_hb_ts >= HEARTBEAT_INTERVAL_US`:
    ///   synthesize a heartbeat at `pkt.ts`, update `last_hb_ts = pkt.ts`,
    ///   return it.
    /// * Else: return `None`.
    pub fn on_packet(&mut self, pkt: &RawPacket) -> Option<RawPacket> {
        let slot = self.last_hb_ts.entry(pkt.stream_id.clone()).or_insert(0);

        if pkt.is_heartbeat() {
            if pkt.timestamp_us > *slot {
                *slot = pkt.timestamp_us;
            }
            return None;
        }

        if *slot == 0 {
            *slot = pkt.timestamp_us;
            return None;
        }

        if pkt.timestamp_us - *slot < HEARTBEAT_INTERVAL_US {
            return None;
        }

        *slot = pkt.timestamp_us;
        Some(RawPacket::heartbeat(
            pkt.timestamp_us,
            pkt.stream_id.clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn real_pkt(stream_id: &str, ts_us: i64) -> RawPacket {
        let mut buf = [0u8; 14];
        buf[0] = 0xAA;
        buf[12] = 0x08;
        buf[13] = 0x00;
        RawPacket {
            timestamp_us: ts_us,
            caplen: 14,
            wirelen: 14,
            link_type: 1,
            data: Bytes::copy_from_slice(&buf),
            stream_id: stream_id.to_string(),
        }
    }

    fn upstream_hb(stream_id: &str, ts_us: i64) -> RawPacket {
        RawPacket::heartbeat(ts_us, stream_id.to_string())
    }

    #[test]
    fn first_packet_primes_baseline_without_synth() {
        let mut t = HeartbeatTracker::new();
        assert!(t.on_packet(&real_pkt("u1", 1_000_000)).is_none());
    }

    #[test]
    fn interval_elapsed_triggers_synth_at_packet_ts() {
        let mut t = HeartbeatTracker::new();
        assert!(t.on_packet(&real_pkt("u1", 1_000_000)).is_none());
        let hb = t
            .on_packet(&real_pkt("u1", 2_000_000))
            .expect("interval reached → HB expected");
        assert!(hb.is_heartbeat());
        assert_eq!(hb.timestamp_us, 2_000_000);
        assert_eq!(hb.stream_id, "u1");
    }

    #[test]
    fn packets_within_interval_do_not_synth() {
        let mut t = HeartbeatTracker::new();
        assert!(t.on_packet(&real_pkt("u1", 1_000_000)).is_none());
        for i in 1..10 {
            let ts = 1_000_000 + i * 50_000;
            assert!(t.on_packet(&real_pkt("u1", ts)).is_none());
        }
    }

    #[test]
    fn streams_are_independent() {
        let mut t = HeartbeatTracker::new();
        t.on_packet(&real_pkt("a", 1_000_000));
        t.on_packet(&real_pkt("b", 1_000_000));
        let hb_a = t.on_packet(&real_pkt("a", 2_000_000));
        let hb_b = t.on_packet(&real_pkt("b", 2_000_000));
        assert_eq!(hb_a.as_ref().unwrap().stream_id, "a");
        assert_eq!(hb_b.as_ref().unwrap().stream_id, "b");
    }

    #[test]
    fn upstream_hb_bumps_counter_so_local_does_not_duplicate() {
        let mut t = HeartbeatTracker::new();
        // Baseline.
        assert!(t.on_packet(&real_pkt("u1", 1_000_000)).is_none());
        // Upstream HB a bit later — only bumps the counter, forwarded as-is.
        assert!(t.on_packet(&upstream_hb("u1", 1_800_000)).is_none());
        // Packet at 2_000_000: gap from last_hb_ts=1_800_000 is only 200 ms,
        // < INTERVAL → no local HB.
        assert!(t.on_packet(&real_pkt("u1", 2_000_000)).is_none());
    }

    #[test]
    fn local_takes_over_when_upstream_silent() {
        let mut t = HeartbeatTracker::new();
        t.on_packet(&real_pkt("u1", 1_000_000));
        t.on_packet(&upstream_hb("u1", 1_500_000));
        // Upstream stops. Next real packet > 1 s after the last bump triggers
        // local synthesis naturally — no timer, no takeover threshold.
        let hb = t.on_packet(&real_pkt("u1", 1_500_000 + HEARTBEAT_INTERVAL_US));
        assert!(hb.is_some());
        assert_eq!(hb.unwrap().timestamp_us, 1_500_000 + HEARTBEAT_INTERVAL_US);
    }
}
