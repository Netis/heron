use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

// ---------------------------------------------------------------------------
// MetricKind
// ---------------------------------------------------------------------------

/// Distinguishes counters (monotonically increasing) from gauges (instantaneous
/// point-in-time values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// Monotonically increasing. Reporter outputs `total/delta`.
    Counter,
    /// Instantaneous value. Reporter outputs current value only.
    Gauge,
}

// ---------------------------------------------------------------------------
// MetricGroup — typed report group
// ---------------------------------------------------------------------------

/// Report group for metric output. Controls grouping and output order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MetricGroup {
    Capture,
    Protocol,
    Llm,
    Turn,
    Metrics,
    Storage,
}

impl MetricGroup {
    /// Canonical output order for [`MonitorPoll::format_grouped`].
    pub const ORDER: &[MetricGroup] = &[
        MetricGroup::Capture,
        MetricGroup::Protocol,
        MetricGroup::Llm,
        MetricGroup::Turn,
        MetricGroup::Metrics,
        MetricGroup::Storage,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            MetricGroup::Capture => "capture",
            MetricGroup::Protocol => "protocol",
            MetricGroup::Llm => "llm",
            MetricGroup::Turn => "turn",
            MetricGroup::Metrics => "metrics",
            MetricGroup::Storage => "storage",
        }
    }
}

impl fmt::Display for MetricGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// MetricSpec — per-variant metadata
// ---------------------------------------------------------------------------

/// Static metadata for a single [`Metric`] variant.
pub struct MetricSpec {
    pub kind: MetricKind,
    pub group: MetricGroup,
    pub short_name: &'static str,
}

// ---------------------------------------------------------------------------
// Metric — defined via macro for single source of truth
// ---------------------------------------------------------------------------

macro_rules! define_metrics {
    (
        $(
            $variant:ident => {
                kind: $kind:ident,
                group: $group:ident,
                short: $short:literal $(,)?
            }
        ),* $(,)?
    ) => {
        /// All internal metrics for pipeline diagnostics.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Metric {
            $( $variant, )*
        }

        impl Metric {
            /// All variants in declaration order.
            pub const ALL: &[Metric] = &[
                $( Metric::$variant, )*
            ];

            /// Returns the full metadata spec for this metric.
            pub const fn spec(self) -> MetricSpec {
                match self {
                    $(
                        Metric::$variant => MetricSpec {
                            kind: MetricKind::$kind,
                            group: MetricGroup::$group,
                            short_name: $short,
                        },
                    )*
                }
            }
        }

        impl fmt::Display for Metric {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // snake_case display
                let s = match self {
                    $( Metric::$variant => stringify!($variant), )*
                };
                f.write_str(s)
            }
        }
    };
}

define_metrics! {
    // -- Capture --
    CapturePacketsReceived   => { kind: Counter, group: Capture,  short: "pkts_recv"       },
    CapturePacketsDropped    => { kind: Counter, group: Capture,  short: "pkts_drop"       },
    CaptureBatchesReceived   => { kind: Counter, group: Capture,  short: "batches_recv"    },
    CaptureBatchesDropped    => { kind: Counter, group: Capture,  short: "batches_drop"    },
    CaptureHeartbeatsEmitted => { kind: Counter, group: Capture,  short: "hb_emit"         },
    CaptureSourceErrors      => { kind: Counter, group: Capture,  short: "src_errors"      },
    CaptureDumpErrors        => { kind: Counter, group: Capture,  short: "dump_errors"     },

    // -- Protocol (dispatcher + flow workers) --
    DispatcherPacketsRouted      => { kind: Counter, group: Protocol, short: "dispatched"     },
    DispatcherHeartbeatsDropped  => { kind: Counter, group: Protocol, short: "hb_drop"        },
    NetPacketsParsed             => { kind: Counter, group: Protocol, short: "net_parsed"     },
    HttpRequestsParsed           => { kind: Counter, group: Protocol, short: "http_req"       },
    HttpResponsesParsed          => { kind: Counter, group: Protocol, short: "http_resp"      },
    SseEventsParsed              => { kind: Counter, group: Protocol, short: "sse_events"     },
    HttpResyncEvents             => { kind: Counter, group: Protocol, short: "http_resync"    },
    FlowsTimedOut                => { kind: Counter, group: Protocol, short: "flows_timeout"  },

    // -- HTTP exchange pairing (HttpJoiner) --
    HttpExchangesCompleted  => { kind: Counter, group: Protocol, short: "xchg_done"     },
    HttpExchangesIncomplete => { kind: Counter, group: Protocol, short: "xchg_orphan"   },
    HttpExchangesExpired    => { kind: Counter, group: Protocol, short: "xchg_expired"  },

    // -- LLM extraction --
    LlmRequestsDetected     => { kind: Counter, group: Llm, short: "req_detected"    },
    LlmRequestsIgnored      => { kind: Counter, group: Llm, short: "req_ignored"     },
    LlmCallsCompleted       => { kind: Counter, group: Llm, short: "calls_completed" },
    LlmCallsIdentified      => { kind: Counter, group: Llm, short: "calls_identified"},
    LlmCallsUnidentified    => { kind: Counter, group: Llm, short: "calls_unident"   },

    // -- Turn tracking --
    TurnCallsIngested        => { kind: Counter, group: Turn, short: "calls_ingested" },
    TurnCallsAuxiliary       => { kind: Counter, group: Turn, short: "calls_aux"      },
    TurnsCompleted           => { kind: Counter, group: Turn, short: "completed"      },
    TurnsTimedOut            => { kind: Counter, group: Turn, short: "timed_out"      },
    TurnReorderOrphan        => { kind: Counter, group: Turn, short: "orphan"         },
    TurnFinalizedByGrace     => { kind: Counter, group: Turn, short: "fin_grace"      },
    TurnFinalizedByIdle      => { kind: Counter, group: Turn, short: "fin_idle"       },
    TurnDiscardedNoUserStart => { kind: Counter, group: Turn, short: "no_user_start"  },

    // -- Metrics aggregation --
    MetricsEventsReceived    => { kind: Counter, group: Metrics, short: "events_recv"    },
    MetricsWindowsFlushed    => { kind: Counter, group: Metrics, short: "windows_flush"  },

    // -- Storage --
    StorageRecordsBuffered   => { kind: Counter, group: Storage, short: "buffered"       },
    StorageRecordsFlushed    => { kind: Counter, group: Storage, short: "flushed"        },
    StorageFlushErrors       => { kind: Counter, group: Storage, short: "flush_errors"   },

    // -- Queue depths (gauges) --
    QueueDepthRaw          => { kind: Gauge, group: Protocol, short: "q.raw"           },
    QueueDepthParsed       => { kind: Gauge, group: Protocol, short: "q.parsed"        },
    QueueDepthEvent        => { kind: Gauge, group: Llm,      short: "q.event"         },
    QueueDepthTurnShard    => { kind: Gauge, group: Turn,     short: "q.turn_shard"    },
    QueueDepthMetricsShard => { kind: Gauge, group: Metrics,  short: "q.metrics_shard" },
    QueueDepthCalls          => { kind: Gauge, group: Storage,  short: "q.calls"         },
    QueueDepthTurns          => { kind: Gauge, group: Storage,  short: "q.turns"         },
    QueueDepthMetricsOut     => { kind: Gauge, group: Storage,  short: "q.metrics_out"   },
    QueueDepthHttpExchanges  => { kind: Gauge, group: Storage,  short: "q.exchanges"     },
}

impl Metric {
    pub const fn kind(self) -> MetricKind {
        self.spec().kind
    }

    pub const fn group(self) -> MetricGroup {
        self.spec().group
    }

    pub const fn short_name(self) -> &'static str {
        self.spec().short_name
    }
}

// ---------------------------------------------------------------------------
// MetricHandle — lightweight atomic counter
// ---------------------------------------------------------------------------

/// A lightweight atomic handle. Each registered `(worker, metric)` pair gets
/// its own independent handle.
#[derive(Clone)]
pub struct MetricHandle {
    value: Arc<AtomicU64>,
}

impl MetricHandle {
    fn new() -> Self {
        Self {
            value: Arc::new(AtomicU64::new(0)),
        }
    }

    #[inline]
    pub fn add(&self, v: u64) {
        self.value.fetch_add(v, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc(&self) {
        self.add(1);
    }

    /// Store an absolute value (for gauge metrics).
    #[inline]
    pub fn set(&self, v: u64) {
        self.value.store(v, Ordering::Relaxed);
    }

    #[inline]
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// MetricsWorker — per-role counter set
// ---------------------------------------------------------------------------

/// Identity of a metrics worker: `{role}-{worker_id}`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkerIdentity {
    pub role: String,
    pub worker_id: u32,
}

impl fmt::Display for WorkerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.role, self.worker_id)
    }
}

/// A set of pre-bound counter handles for a single worker.
#[derive(Clone)]
pub struct MetricsWorker {
    #[allow(dead_code)]
    identity: WorkerIdentity,
    counters: BTreeMap<Metric, MetricHandle>,
}

impl MetricsWorker {
    /// Get the handle for a specific metric. Panics if the metric was not
    /// registered for this worker.
    #[inline]
    pub fn counter(&self, metric: Metric) -> &MetricHandle {
        self.counters.get(&metric).unwrap_or_else(|| {
            panic!(
                "metric {:?} not registered for worker {}",
                metric, self.identity
            )
        })
    }
}

// ---------------------------------------------------------------------------
// QueueProbe — reporter-driven queue depth sampling
// ---------------------------------------------------------------------------

/// A queue depth probe that the reporter calls periodically to sample the
/// current queue length.
struct QueueProbe {
    handle: MetricHandle,
    sample: Box<dyn Fn() -> u64 + Send + Sync>,
}

// ---------------------------------------------------------------------------
// MetricsSystem — build-phase registry
// ---------------------------------------------------------------------------

/// Build-phase metrics registry. Workers register during setup. Once finalized
/// via [`start()`], it produces a read-only [`MetricsSvc`].
pub struct MetricsSystem {
    next_worker_id: u32,
    registry: BTreeMap<Metric, Vec<(WorkerIdentity, MetricHandle)>>,
    probes: Vec<QueueProbe>,
}

impl MetricsSystem {
    pub fn new() -> Self {
        Self {
            next_worker_id: 0,
            registry: BTreeMap::new(),
            probes: Vec::new(),
        }
    }

    /// Register a new worker with the given role and metric set.
    pub fn register_worker(&mut self, role: &str, metrics: &[Metric]) -> MetricsWorker {
        let worker_id = self.next_worker_id;
        self.next_worker_id += 1;

        let identity = WorkerIdentity {
            role: role.to_string(),
            worker_id,
        };

        let mut counters = BTreeMap::new();
        for &metric in metrics {
            let handle = MetricHandle::new();
            self.registry
                .entry(metric)
                .or_default()
                .push((identity.clone(), handle.clone()));
            counters.insert(metric, handle);
        }

        MetricsWorker { identity, counters }
    }

    /// Register a queue depth probe. The `sample` closure is called by the
    /// reporter on each poll cycle.
    pub fn register_queue_probe(
        &mut self,
        metric: Metric,
        sample: impl Fn() -> u64 + Send + Sync + 'static,
    ) {
        let handle = MetricHandle::new();
        let identity = WorkerIdentity {
            role: "probe".to_string(),
            worker_id: self.next_worker_id,
        };
        self.next_worker_id += 1;
        self.registry
            .entry(metric)
            .or_default()
            .push((identity, handle.clone()));
        self.probes.push(QueueProbe {
            handle,
            sample: Box::new(sample),
        });
    }

    /// Finalize the build phase and produce a read-only service view.
    pub fn start(self) -> Arc<MetricsSvc> {
        Arc::new(MetricsSvc {
            registry: self.registry,
            probes: self.probes,
        })
    }
}

// ---------------------------------------------------------------------------
// MetricsSvc — read-only aggregation view
// ---------------------------------------------------------------------------

/// Read-only service view of all registered metrics.
pub struct MetricsSvc {
    registry: BTreeMap<Metric, Vec<(WorkerIdentity, MetricHandle)>>,
    probes: Vec<QueueProbe>,
}

// Safety: QueueProbe contains Box<dyn Fn() -> u64 + Send + Sync>, which is
// Send + Sync. MetricHandle uses Arc<AtomicU64> which is Send + Sync.
unsafe impl Send for MetricsSvc {}
unsafe impl Sync for MetricsSvc {}

impl MetricsSvc {
    /// Sample all registered queue probes.
    pub fn sample_probes(&self) {
        for probe in &self.probes {
            probe.handle.set((probe.sample)());
        }
    }

    /// Compute the aggregate (sum across all workers) for a given metric.
    pub fn aggregate(&self, metric: Metric) -> Option<u64> {
        self.registry
            .get(&metric)
            .map(|workers| workers.iter().map(|(_, h)| h.get()).sum())
    }

    /// Take a snapshot of all registered metrics (aggregated across workers).
    pub fn snapshot(&self) -> MetricsSnapshot {
        let mut values = BTreeMap::new();
        for (&metric, workers) in &self.registry {
            let total: u64 = workers.iter().map(|(_, h)| h.get()).sum();
            values.insert(metric, total);
        }
        MetricsSnapshot { values }
    }
}

// ---------------------------------------------------------------------------
// MetricsSnapshot / MetricsMonitor
// ---------------------------------------------------------------------------

/// A point-in-time snapshot of aggregated metric values.
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub values: BTreeMap<Metric, u64>,
}

impl MetricsSnapshot {
    pub fn empty() -> Self {
        Self {
            values: BTreeMap::new(),
        }
    }
}

/// Maintains previous snapshot to compute deltas.
pub struct MetricsMonitor {
    svc: Arc<MetricsSvc>,
    prev: MetricsSnapshot,
}

impl MetricsMonitor {
    pub fn new(svc: Arc<MetricsSvc>) -> Self {
        Self {
            svc,
            prev: MetricsSnapshot::empty(),
        }
    }

    /// Take a new snapshot, compute deltas from previous, return both.
    pub fn poll(&mut self) -> MonitorPoll {
        let current = self.svc.snapshot();
        let mut deltas = BTreeMap::new();
        for (&metric, &total) in &current.values {
            let delta = match metric.kind() {
                MetricKind::Counter => {
                    let prev = self.prev.values.get(&metric).copied().unwrap_or(0);
                    total.saturating_sub(prev)
                }
                MetricKind::Gauge => 0,
            };
            deltas.insert(metric, delta);
        }
        self.prev = current.clone();
        MonitorPoll {
            snapshot: current,
            deltas,
        }
    }
}

/// Result of a monitor poll: current totals and deltas since last poll.
pub struct MonitorPoll {
    pub snapshot: MetricsSnapshot,
    pub deltas: BTreeMap<Metric, u64>,
}

impl MonitorPoll {
    /// Format metrics grouped by category for human-readable log output.
    ///
    /// Returns a list of `(group_name, formatted_line)` pairs.
    /// - Counter: `short_name: total/delta`
    /// - Gauge:   `short_name: value`
    pub fn format_grouped(&self) -> Vec<(&'static str, String)> {
        let mut by_group: BTreeMap<MetricGroup, Vec<String>> = BTreeMap::new();

        for (&metric, &total) in &self.snapshot.values {
            let spec = metric.spec();
            let part = match spec.kind {
                MetricKind::Counter => {
                    let delta = self.deltas.get(&metric).copied().unwrap_or(0);
                    format!("{}={}/{}", spec.short_name, total, delta)
                }
                MetricKind::Gauge => {
                    format!("{}={}", spec.short_name, total)
                }
            };
            by_group.entry(spec.group).or_default().push(part);
        }

        MetricGroup::ORDER
            .iter()
            .filter_map(|&group| {
                by_group
                    .remove(&group)
                    .map(|parts| (group.as_str(), parts.join(" ")))
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// MetricsReporter — async Tokio task
// ---------------------------------------------------------------------------

/// Periodic metrics reporter. Spawns a Tokio task that polls and logs metrics.
pub struct MetricsReporter;

impl MetricsReporter {
    /// Start the reporter as a background Tokio task.
    ///
    /// `label` is prefixed onto every log line so multiple reporters (e.g.
    /// one per capture source) can be told apart in the output.
    ///
    /// Returns a shutdown handle — drop the sender or send `()` to stop.
    pub fn start(svc: Arc<MetricsSvc>, label: &str, interval: Duration) -> watch::Sender<()> {
        let (stop_tx, mut stop_rx) = watch::channel(());
        let label = label.to_string();

        tokio::spawn(async move {
            let mut monitor = MetricsMonitor::new(svc.clone());
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // first tick is immediate, skip it

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        Self::report(&label, &svc, &mut monitor);
                    }
                    _ = stop_rx.changed() => {
                        // Final report before exit.
                        Self::report(&label, &svc, &mut monitor);
                        tracing::info!("[INTERNAL] {label} | metrics reporter stopped");
                        break;
                    }
                }
            }
        });

        stop_tx
    }

    fn report(label: &str, svc: &MetricsSvc, monitor: &mut MetricsMonitor) {
        svc.sample_probes();
        let poll = monitor.poll();
        for (group, line) in poll.format_grouped() {
            tracing::info!("[INTERNAL] {label} | {:<8} | {}", group, line);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worker_registration_and_aggregation() {
        let mut sys = MetricsSystem::new();

        let w1 = sys.register_worker(
            "worker",
            &[Metric::NetPacketsParsed, Metric::HttpRequestsParsed],
        );
        let w2 = sys.register_worker(
            "worker",
            &[Metric::NetPacketsParsed, Metric::HttpRequestsParsed],
        );

        w1.counter(Metric::NetPacketsParsed).add(100);
        w2.counter(Metric::NetPacketsParsed).add(200);
        w1.counter(Metric::HttpRequestsParsed).add(10);
        w2.counter(Metric::HttpRequestsParsed).add(20);

        let svc = sys.start();
        assert_eq!(svc.aggregate(Metric::NetPacketsParsed), Some(300));
        assert_eq!(svc.aggregate(Metric::HttpRequestsParsed), Some(30));
        assert_eq!(svc.aggregate(Metric::CapturePacketsReceived), None);
    }

    #[test]
    fn test_monitor_total_and_delta() {
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker("test", &[Metric::LlmCallsCompleted]);
        let svc = sys.start();
        let mut mon = MetricsMonitor::new(svc);

        w.counter(Metric::LlmCallsCompleted).add(5);
        let poll1 = mon.poll();
        assert_eq!(poll1.snapshot.values[&Metric::LlmCallsCompleted], 5);
        assert_eq!(poll1.deltas[&Metric::LlmCallsCompleted], 5);

        w.counter(Metric::LlmCallsCompleted).add(3);
        let poll2 = mon.poll();
        assert_eq!(poll2.snapshot.values[&Metric::LlmCallsCompleted], 8);
        assert_eq!(poll2.deltas[&Metric::LlmCallsCompleted], 3);
    }

    #[test]
    fn test_gauge_delta_is_zero() {
        let mut sys = MetricsSystem::new();
        sys.register_queue_probe(Metric::QueueDepthRaw, || 42);
        let svc = sys.start();
        svc.sample_probes();

        let mut mon = MetricsMonitor::new(svc);
        let poll = mon.poll();
        assert_eq!(poll.snapshot.values[&Metric::QueueDepthRaw], 42);
        assert_eq!(poll.deltas[&Metric::QueueDepthRaw], 0);
    }

    #[test]
    fn test_format_grouped() {
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker(
            "test",
            &[
                Metric::CapturePacketsReceived,
                Metric::NetPacketsParsed,
                Metric::StorageRecordsBuffered,
            ],
        );
        sys.register_queue_probe(Metric::QueueDepthCalls, || 5);

        let svc = sys.start();
        svc.sample_probes();
        let mut mon = MetricsMonitor::new(svc);

        w.counter(Metric::CapturePacketsReceived).add(1000);
        w.counter(Metric::NetPacketsParsed).add(500);
        w.counter(Metric::StorageRecordsBuffered).add(100);

        let poll = mon.poll();
        let grouped = poll.format_grouped();

        assert_eq!(grouped.len(), 3);
        assert_eq!(grouped[0].0, "capture");
        assert!(grouped[0].1.contains("pkts_recv=1000/1000"));
        assert_eq!(grouped[1].0, "protocol");
        assert!(grouped[1].1.contains("net_parsed=500/500"));
        assert_eq!(grouped[2].0, "storage");
        assert!(grouped[2].1.contains("buffered=100/100"));
        assert!(grouped[2].1.contains("q.calls=5"));
    }

    #[test]
    fn test_all_variants_have_metadata() {
        for &m in Metric::ALL {
            let spec = m.spec();
            assert!(!spec.short_name.is_empty(), "{m:?} has empty short_name");
            assert!(
                MetricGroup::ORDER.contains(&spec.group),
                "{m:?} has group {:?} not in ORDER",
                spec.group,
            );
        }
    }

    #[test]
    fn test_handle_set_and_add() {
        let h = MetricHandle::new();
        h.set(42);
        assert_eq!(h.get(), 42);
        h.add(8);
        assert_eq!(h.get(), 50);
    }

    #[test]
    fn test_worker_id_monotonic() {
        let mut sys = MetricsSystem::new();
        let w0 = sys.register_worker("a", &[Metric::CapturePacketsReceived]);
        let w1 = sys.register_worker("b", &[Metric::CapturePacketsReceived]);
        assert_eq!(w0.identity.worker_id, 0);
        assert_eq!(w1.identity.worker_id, 1);
    }

    #[test]
    fn test_queue_probe_sampling() {
        let depth = Arc::new(AtomicU64::new(0));
        let mut sys = MetricsSystem::new();

        let depth_clone = depth.clone();
        sys.register_queue_probe(Metric::QueueDepthRaw, move || {
            depth_clone.load(Ordering::Relaxed)
        });

        let svc = sys.start();

        assert_eq!(svc.aggregate(Metric::QueueDepthRaw), Some(0));

        depth.store(42, Ordering::Relaxed);
        svc.sample_probes();
        assert_eq!(svc.aggregate(Metric::QueueDepthRaw), Some(42));

        depth.store(5, Ordering::Relaxed);
        svc.sample_probes();
        assert_eq!(svc.aggregate(Metric::QueueDepthRaw), Some(5));
    }
}
