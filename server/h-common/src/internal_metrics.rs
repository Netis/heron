use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;
use tokio::task::JoinHandle;

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
    Ebpf,
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
        MetricGroup::Ebpf,
        MetricGroup::Protocol,
        MetricGroup::Llm,
        MetricGroup::Turn,
        MetricGroup::Metrics,
        MetricGroup::Storage,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            MetricGroup::Capture => "capture",
            MetricGroup::Ebpf => "ebpf",
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
    CapturePacketsReceived       => { kind: Counter, group: Capture,  short: "pkts_received"        },
    CaptureKernelPacketsDropped  => { kind: Counter, group: Capture,  short: "pkts_dropped_kernel"  },
    CaptureTruncatedPackets      => { kind: Counter, group: Capture,  short: "pkts_truncated"       },
    CaptureBatchesReceived       => { kind: Counter, group: Capture,  short: "batches_received"     },
    CaptureZmqBatchesDropped     => { kind: Counter, group: Capture,  short: "batches_dropped_zmq"  },
    CaptureHeartbeatsEmitted     => { kind: Counter, group: Capture,  short: "heartbeats_emitted"   },
    CaptureReadErrors            => { kind: Counter, group: Capture,  short: "read_errors"          },
    CaptureDumpErrors            => { kind: Counter, group: Capture,  short: "dump_errors"          },
    CaptureDumpLateMinutePackets => { kind: Counter, group: Capture,  short: "dump_late_minute_pkts" },
    CaptureDumpRetentionFilesDeleted => { kind: Counter, group: Capture, short: "dump_retention_files_deleted" },
    CaptureDumpRetentionBytesDeleted => { kind: Counter, group: Capture, short: "dump_retention_bytes_deleted" },
    CaptureDumpRetentionErrors       => { kind: Counter, group: Capture, short: "dump_retention_errors" },

    // -- eBPF SSL-uprobe capture (Linux, `ebpf` feature) --
    // Emitted only by the eBPF source; stay 0 for packet-tap sources. Together
    // they tell the on-host TLS story: kernel events in → bytes → synthetic
    // frames out, with attach health + live connection / process-cache gauges.
    EbpfEventsReceived       => { kind: Counter, group: Ebpf, short: "ebpf_events_received"    },
    EbpfEventsDropped        => { kind: Counter, group: Ebpf, short: "ebpf_events_dropped"     },
    EbpfBytesCaptured        => { kind: Counter, group: Ebpf, short: "ebpf_bytes_captured"     },
    EbpfFramesSynthesized    => { kind: Counter, group: Ebpf, short: "ebpf_frames_synthesized" },
    EbpfUprobesAttached      => { kind: Gauge,   group: Ebpf, short: "ebpf_uprobes_attached"   },
    EbpfConnectionsActive    => { kind: Gauge,   group: Ebpf, short: "ebpf_connections_active" },
    EbpfProcessCacheSize     => { kind: Gauge,   group: Ebpf, short: "ebpf_process_cache_size" },

    // -- Protocol (dispatcher + flow workers) --
    // Heartbeat received/dropped are both attributed to the destination
    // (flow worker), even though `_dropped` is bumped at the dispatcher's
    // try_send site — co-grouping the pair keeps "received N / dropped M"
    // visually adjacent in logs and the All-Metrics table.
    DispatcherPacketsRouted      => { kind: Counter, group: Protocol, short: "pkts_routed"               },
    FlowHeartbeatsReceived       => { kind: Counter, group: Protocol, short: "flow_heartbeats_received"  },
    FlowHeartbeatsDropped        => { kind: Counter, group: Protocol, short: "flow_heartbeats_dropped"   },
    NetPacketsParsed             => { kind: Counter, group: Protocol, short: "pkts_parsed"               },
    NetParseDroppedNotIp         => { kind: Counter, group: Protocol, short: "pkts_dropped_not_ip"    },
    NetParseDroppedNotTcp        => { kind: Counter, group: Protocol, short: "pkts_dropped_not_tcp"   },
    NetParseDroppedMalformed     => { kind: Counter, group: Protocol, short: "pkts_dropped_malformed" },
    HttpParseReq                 => { kind: Counter, group: Protocol, short: "http_reqs_parsed"       },
    HttpParseResp                => { kind: Counter, group: Protocol, short: "http_resps_parsed"      },
    SseEventsParsed              => { kind: Counter, group: Protocol, short: "sse_events_parsed"      },
    HttpResyncEvents             => { kind: Counter, group: Protocol, short: "http_resyncs"           },
    TcpOutOfOrderDrops           => { kind: Counter, group: Protocol, short: "tcp_ooo_dropped"        },
    TcpOutOfOrderBuffered        => { kind: Counter, group: Protocol, short: "tcp_ooo_buffered"       },
    TcpRetransmissionsIgnored    => { kind: Counter, group: Protocol, short: "tcp_rexmits_ignored"    },
    FlowsExpired                 => { kind: Counter, group: Protocol, short: "flows_expired"          },
    FlowsActive                  => { kind: Gauge,   group: Protocol, short: "flows_active"           },

    // -- HTTP exchange pairing (HttpJoiner) --
    HttpJoinerDone             => { kind: Counter, group: Protocol, short: "http_exchanges_joined"    },
    HttpJoinerUnpaired         => { kind: Counter, group: Protocol, short: "http_exchanges_unpaired"  },
    HttpJoinerExpired          => { kind: Counter, group: Protocol, short: "http_exchanges_expired"   },
    HttpJoinerPending          => { kind: Gauge,   group: Protocol, short: "http_exchanges_pending"   },
    JoinerHeartbeatsReceived   => { kind: Counter, group: Protocol, short: "joiner_heartbeats_received" },

    // -- LLM extraction --
    WireDetected                   => { kind: Counter, group: Llm, short: "wires_detected"               },
    WireIgnored                    => { kind: Counter, group: Llm, short: "wires_ignored"                },
    LlmCallsWithAgent              => { kind: Counter, group: Llm, short: "calls_with_agent"             },
    LlmCallsWithoutAgent           => { kind: Counter, group: Llm, short: "calls_without_agent"          },
    LlmGenericToolIdCanonicalized  => { kind: Counter, group: Llm, short: "generic_tool_id_canonicalized"   },
    LlmGenericSessionIdSynthFailed => { kind: Counter, group: Llm, short: "generic_session_id_synth_failed" },
    LlmHeartbeatsReceived          => { kind: Counter, group: Llm, short: "llm_heartbeats_received"      },
    LlmTokensEstimated             => { kind: Counter, group: Llm, short: "tokens_estimated"            },

    // -- Turn tracking --
    // `turn_heartbeats_dropped` is bumped at the LLM stage's try_send site
    // when the turn shard input channel is full; attributed to Turn so the
    // (received, dropped) pair shows together.
    TurnCallsIngested        => { kind: Counter, group: Turn, short: "calls_ingested"                },
    TurnCallsAuxiliary       => { kind: Counter, group: Turn, short: "calls_auxiliary"               },
    TurnCallsDroppedLate     => { kind: Counter, group: Turn, short: "calls_dropped_late"            },
    TurnsCompleted           => { kind: Counter, group: Turn, short: "turns_completed"               },
    TurnClosedByGrace        => { kind: Counter, group: Turn, short: "turns_closed_grace"            },
    TurnClosedByIdle         => { kind: Counter, group: Turn, short: "turns_closed_idle"             },
    TurnDiscardedNoUserStart => { kind: Counter, group: Turn, short: "turns_discarded_no_user_start" },
    // A partition with no main-agent `is_user_turn_start` would normally be
    // discarded (see `TurnDiscardedNoUserStart`), but when its calls carry a
    // common eBPF process attribution (`call.process`), they are provably one
    // process's contiguous agent traffic — usually a turn whose opening call
    // was missed by the eBPF source (connection-setup / uprobe-attach timing).
    // Counted here so the fallback's hit rate is observable; it indirectly
    // surfaces the eBPF miss rate for opening calls.
    TurnKeptByPidAttribution => { kind: Counter, group: Turn, short: "turns_kept_by_pid_attribution" },
    TurnHeartbeatsReceived   => { kind: Counter, group: Turn, short: "turn_heartbeats_received"      },
    TurnHeartbeatsDropped    => { kind: Counter, group: Turn, short: "turn_heartbeats_dropped"       },
    TurnActive               => { kind: Gauge,   group: Turn, short: "turn_calls_buffered"           },
    // Process-wide registry of in-progress agent turns (size of
    // ActiveTurnRegistry). Distinct from `turn_calls_buffered`, which sums
    // pending LLM calls inside per-session turn buffers — that gauge counts
    // calls, not conversations. `agent_turns_open` is the truthful
    // "concurrent in-flight agent turns" signal and is what the dashboard
    // "Active Agent Turns" chart reads.
    TurnRegistryActive       => { kind: Gauge,   group: Turn, short: "agent_turns_open"              },

    // -- Metrics aggregation --
    // Start/Complete are kept under the `llm_events_*` family because the
    // LlmEvent variant split (start vs complete) is the meaningful axis.
    // Heartbeat is renamed to `metrics_heartbeats_received` so it pairs
    // with `metrics_heartbeats_dropped` (bumped at LLM stage's try_send to
    // metrics shards). Splitting heartbeat from start/complete remains
    // important: heartbeat fan-out (= flow_shards × metrics_shards) would
    // otherwise drown out the real call signal in any combined counter.
    MetricsLlmEventsStart       => { kind: Counter, group: Metrics, short: "llm_events_start"            },
    MetricsLlmEventsComplete    => { kind: Counter, group: Metrics, short: "llm_events_complete"         },
    MetricsHeartbeatsReceived   => { kind: Counter, group: Metrics, short: "metrics_heartbeats_received" },
    MetricsHeartbeatsDropped    => { kind: Counter, group: Metrics, short: "metrics_heartbeats_dropped"  },
    MetricsWindowsEmitted       => { kind: Counter, group: Metrics, short: "windows_emitted"             },
    // open_buckets: in-window aggregation slots awaiting drain (sawtooth).
    // concurrency_table: size of the per-dim active-call counter map; entries
    // are never removed (aggregator.rs clamps to 0, doesn't .remove()), so
    // the gauge is a cardinality canary for OOM diagnosis.
    MetricsAggregatorOpenBuckets      => { kind: Gauge, group: Metrics, short: "aggregator_open_buckets"      },
    MetricsAggregatorConcurrencyTable => { kind: Gauge, group: Metrics, short: "aggregator_concurrency_table" },

    // -- Storage --
    // buffered/flushed split per entity so the line tells you which stream
    // dominates (was previously one shared counter across 4 WriteBuffers).
    // buffered_* are gauges of the WriteBuffer's current pending batch length
    // (rises on push, drops to 0 after each flush). Counterpart `flushed_*`
    // remain counters and carry the throughput signal.
    StorageBufferedCalls         => { kind: Gauge,   group: Storage, short: "buf_calls"         },
    StorageBufferedTurns         => { kind: Gauge,   group: Storage, short: "buf_turns"         },
    StorageBufferedMetrics       => { kind: Gauge,   group: Storage, short: "buf_metrics"       },
    StorageBufferedHttpExchanges => { kind: Gauge,   group: Storage, short: "buf_exchanges"     },
    StorageFlushedCalls          => { kind: Counter, group: Storage, short: "flushed_calls"     },
    StorageFlushedTurns          => { kind: Counter, group: Storage, short: "flushed_turns"     },
    StorageFlushedMetrics        => { kind: Counter, group: Storage, short: "flushed_metrics"   },
    StorageFlushedHttpExchanges  => { kind: Counter, group: Storage, short: "flushed_exchanges" },
    StorageFlushErrors           => { kind: Counter, group: Storage, short: "flush_errors"      },

    // -- Queue depths (gauges) --
    // Each queue is named after the content it carries (not in/out),
    // so grep lands uniquely regardless of which side of the stage
    // you're thinking from.
    QueueDepthRaw                  => { kind: Gauge, group: Protocol, short: "q_raw_pkts"           },
    QueueDepthParsed               => { kind: Gauge, group: Protocol, short: "q_parsed_pkts"        },
    QueueDepthHttpParseEvent       => { kind: Gauge, group: Llm,      short: "q_http_parse_events"  },
    QueueDepthHttpJoinerEvent      => { kind: Gauge, group: Llm,      short: "q_http_joiner_events" },
    QueueDepthAgentCall            => { kind: Gauge, group: Turn,     short: "q_agent_calls"        },
    QueueDepthLlmEvent             => { kind: Gauge, group: Metrics,  short: "q_llm_events"         },
    StorageQueueDepthCalls         => { kind: Gauge, group: Storage,  short: "q_calls"              },
    StorageQueueDepthTurns         => { kind: Gauge, group: Storage,  short: "q_turns"              },
    StorageQueueDepthMetrics       => { kind: Gauge, group: Storage,  short: "q_metrics"            },
    StorageQueueDepthHttpExchanges => { kind: Gauge, group: Storage,  short: "q_exchanges"          },

    // -- Agent classifier --
    AgentClassifierUnknownCount    => { kind: Counter, group: Llm, short: "classifier_unknown"     },
    AgentClassifierMixedCount      => { kind: Counter, group: Llm, short: "classifier_mixed"       },
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
///
/// `capacity` is captured at registration time. tokio mpsc channels never
/// resize their buffer, so a static value is sufficient and lets the reporter
/// render `name=used/cap(pct%)` without re-querying. `None` means the gauge
/// has no meaningful upper bound (e.g. `flows_active`, `turn_active`).
struct QueueProbe {
    metric: Metric,
    handle: MetricHandle,
    capacity: Option<u64>,
    sample: Box<dyn Fn() -> u64 + Send + Sync>,
}

// ---------------------------------------------------------------------------
// MetricsSystem — build-phase registry
// ---------------------------------------------------------------------------

/// Build-phase metrics registry. Workers register during setup. Once finalized
/// via [`start()`], it produces a read-only [`MetricsSvc`].
#[derive(Default)]
pub struct MetricsSystem {
    next_worker_id: u32,
    registry: BTreeMap<Metric, Vec<(WorkerIdentity, MetricHandle)>>,
    probes: Vec<QueueProbe>,
}

impl MetricsSystem {
    pub fn new() -> Self {
        Self::default()
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

    /// Register a gauge probe with no fixed capacity. Used for unbounded
    /// counts like active flow / active turn gauges. Reporter renders
    /// `name=value`.
    pub fn register_queue_probe(
        &mut self,
        metric: Metric,
        sample: impl Fn() -> u64 + Send + Sync + 'static,
    ) {
        self.register_probe_inner(metric, None, Box::new(sample));
    }

    /// Register a bounded queue probe. `capacity` is the channel's
    /// `max_capacity()`. Reporter renders `name=used/cap(pct%)`.
    pub fn register_queue_probe_capped(
        &mut self,
        metric: Metric,
        capacity: u64,
        sample: impl Fn() -> u64 + Send + Sync + 'static,
    ) {
        self.register_probe_inner(metric, Some(capacity), Box::new(sample));
    }

    fn register_probe_inner(
        &mut self,
        metric: Metric,
        capacity: Option<u64>,
        sample: Box<dyn Fn() -> u64 + Send + Sync>,
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
            metric,
            handle,
            capacity,
            sample,
        });
    }

    /// Finalize the build phase and produce a read-only service view.
    pub fn start(self) -> Arc<MetricsSvc> {
        let mut capacities: BTreeMap<Metric, u64> = BTreeMap::new();
        for probe in &self.probes {
            if let Some(cap) = probe.capacity {
                capacities.insert(probe.metric, cap);
            }
        }
        Arc::new(MetricsSvc {
            registry: self.registry,
            probes: self.probes,
            capacities: Arc::new(capacities),
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
    /// Static per-metric capacity, populated for queue probes registered
    /// via [`MetricsSystem::register_queue_probe_capped`]. Shared with
    /// [`MonitorPoll`] for `name=used/cap(pct%)` formatting.
    capacities: Arc<BTreeMap<Metric, u64>>,
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

    /// Capacity map for bounded gauges (queue probes registered with a cap).
    pub fn capacities(&self) -> Arc<BTreeMap<Metric, u64>> {
        self.capacities.clone()
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
            capacities: self.svc.capacities(),
        }
    }
}

/// Result of a monitor poll: current totals and deltas since last poll.
pub struct MonitorPoll {
    pub snapshot: MetricsSnapshot,
    pub deltas: BTreeMap<Metric, u64>,
    pub capacities: Arc<BTreeMap<Metric, u64>>,
}

impl MonitorPoll {
    /// Format metrics grouped by category for human-readable log output.
    ///
    /// Returns a list of `(group_name, formatted_line)` pairs.
    /// - Counter: `short_name=total/delta`
    /// - Gauge with known capacity: `short_name=used/cap(pct%)`
    /// - Gauge without capacity:    `short_name=value`
    pub fn format_grouped(&self) -> Vec<(&'static str, String)> {
        let mut by_group: BTreeMap<MetricGroup, Vec<String>> = BTreeMap::new();

        for (&metric, &total) in &self.snapshot.values {
            let spec = metric.spec();
            let part = match spec.kind {
                MetricKind::Counter => {
                    let delta = self.deltas.get(&metric).copied().unwrap_or(0);
                    format!("{}={}/{}", spec.short_name, total, delta)
                }
                MetricKind::Gauge => match self.capacities.get(&metric).copied() {
                    Some(cap) if cap > 0 => {
                        let pct = (total as u128 * 100 / cap as u128) as u64;
                        format!("{}={}/{}({}%)", spec.short_name, total, cap, pct)
                    }
                    _ => format!("{}={}", spec.short_name, total),
                },
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

/// Handle returned by [`MetricsReporter::start`]. Hold both pieces:
///
/// * `stop_tx` — drop or `send(())` to ask the reporter to print its final
///   tick and exit.
/// * `join` — `await` it after stopping to know the final tick has actually
///   been logged. Awaiting before sending stop hangs forever (the reporter
///   only exits on the stop signal, not by drop).
///
/// Holding only `stop_tx` and dropping `join` keeps the reporter task alive
/// but loses the final-flush guarantee — the runtime may abort the task
/// before its final report logs. Most callers should keep both.
pub struct ReporterHandle {
    pub stop_tx: watch::Sender<()>,
    pub join: JoinHandle<()>,
}

/// Periodic metrics reporter. Spawns a Tokio task that polls and logs metrics.
pub struct MetricsReporter;

impl MetricsReporter {
    /// Start the reporter as a background Tokio task.
    ///
    /// `label` is prefixed onto every log line so multiple reporters (e.g.
    /// one per capture source) can be told apart in the output.
    ///
    /// Returns a [`ReporterHandle`]: send/drop `stop_tx` to stop, then `await`
    /// `join` to ensure the final tick was logged.
    pub fn start(svc: Arc<MetricsSvc>, label: &str, interval: Duration) -> ReporterHandle {
        let (stop_tx, mut stop_rx) = watch::channel(());
        let label = label.to_string();

        let join = tokio::spawn(async move {
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

        ReporterHandle { stop_tx, join }
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
// AggregateHistory — in-memory time-series ring for selected gauges
// ---------------------------------------------------------------------------

/// One sample in the in-memory history ring.
#[derive(Debug, Clone, Copy)]
pub struct HistoryPoint {
    /// Wall-clock unix timestamp in milliseconds (when the sample was taken).
    pub ts_ms: i64,
    /// Aggregated value at that instant (summed across the contributing svcs
    /// the recorder was given — typically all per-pipeline `MetricsSvc`s plus
    /// the global one).
    pub value: u64,
}

struct HistoryFrame {
    ts_ms: i64,
    values: BTreeMap<Metric, u64>,
}

/// Bounded in-memory ring of timestamped gauge samples, sampled by
/// [`HistoryRecorder`] at a fixed interval. Reads are cheap (single
/// `RwLock` shared read), writes are amortized O(1) (`VecDeque` push back,
/// pop front when at capacity).
///
/// Capacity is computed by the caller from retention / interval (e.g. 24h
/// at 10s ⇒ 8640 frames). At < 64 bytes / frame including the per-metric
/// `BTreeMap` entries, a 24h ring for two gauges fits in ~500 KB — small
/// enough to ignore.
///
/// History is gone on process restart; this is by design (gauges are
/// "active now" signals — the cross-restart story isn't useful here).
pub struct AggregateHistory {
    tracked: Vec<Metric>,
    capacity: usize,
    samples: RwLock<VecDeque<HistoryFrame>>,
}

impl AggregateHistory {
    /// Create a new history ring for the given tracked metrics.
    pub fn new(tracked: Vec<Metric>, capacity: usize) -> Arc<Self> {
        let cap = capacity.max(1);
        Arc::new(Self {
            tracked,
            capacity: cap,
            samples: RwLock::new(VecDeque::with_capacity(cap)),
        })
    }

    /// Metrics this ring records (subset of `Metric::ALL`).
    pub fn tracked(&self) -> &[Metric] {
        &self.tracked
    }

    /// Push a frame, evicting the oldest if at capacity.
    pub fn push(&self, ts_ms: i64, values: BTreeMap<Metric, u64>) {
        let mut buf = self.samples.write().expect("history lock poisoned");
        if buf.len() >= self.capacity {
            buf.pop_front();
        }
        buf.push_back(HistoryFrame { ts_ms, values });
    }

    /// Return all samples for `metric` with `ts_ms >= since_ms`.
    ///
    /// If the ring is empty or the metric was never tracked, returns an
    /// empty vector.
    pub fn series(&self, metric: Metric, since_ms: i64) -> Vec<HistoryPoint> {
        let buf = self.samples.read().expect("history lock poisoned");
        buf.iter()
            .filter(|f| f.ts_ms >= since_ms)
            .map(|f| HistoryPoint {
                ts_ms: f.ts_ms,
                value: f.values.get(&metric).copied().unwrap_or(0),
            })
            .collect()
    }

    /// Number of samples currently held (test/diag helper).
    pub fn len(&self) -> usize {
        self.samples.read().expect("history lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Periodic recorder that samples every contributor `MetricsSvc` and pushes
/// one summed frame into the shared [`AggregateHistory`].
///
/// Drives independent of [`MetricsReporter`] so it can keep ticking when the
/// reporter is disabled (`internal_metrics.enabled = false` in config), and
/// so the recorder cadence does not have to equal the log cadence — though
/// for v1 they share `internal_metrics.interval_secs`.
pub struct HistoryRecorder;

impl HistoryRecorder {
    /// Spawn the recorder task. `svcs` is the set of `MetricsSvc`s whose
    /// values are summed at every tick (typically per-pipeline svcs + the
    /// global one). The handle works the same way as [`MetricsReporter`]'s.
    pub fn start(
        svcs: Vec<Arc<MetricsSvc>>,
        history: Arc<AggregateHistory>,
        interval: Duration,
    ) -> ReporterHandle {
        let (stop_tx, mut stop_rx) = watch::channel(());

        let join = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // first tick is immediate, skip

            loop {
                tokio::select! {
                    _ = ticker.tick() => Self::record(&svcs, &history),
                    _ = stop_rx.changed() => {
                        Self::record(&svcs, &history);
                        break;
                    }
                }
            }
        });

        ReporterHandle { stop_tx, join }
    }

    /// Take one synchronous sample. Pub for unit-test reuse — production
    /// callers should let [`start`](Self::start)'s tokio task drive this.
    pub fn record(svcs: &[Arc<MetricsSvc>], history: &AggregateHistory) {
        let mut sum: BTreeMap<Metric, u64> = history.tracked().iter().map(|&m| (m, 0u64)).collect();
        for svc in svcs {
            svc.sample_probes();
            for &m in history.tracked() {
                if let Some(v) = svc.aggregate(m) {
                    if let Some(slot) = sum.get_mut(&m) {
                        *slot = slot.saturating_add(v);
                    }
                }
            }
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        history.push(ts, sum);
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

        let w1 = sys.register_worker("worker", &[Metric::NetPacketsParsed, Metric::HttpParseReq]);
        let w2 = sys.register_worker("worker", &[Metric::NetPacketsParsed, Metric::HttpParseReq]);

        w1.counter(Metric::NetPacketsParsed).add(100);
        w2.counter(Metric::NetPacketsParsed).add(200);
        w1.counter(Metric::HttpParseReq).add(10);
        w2.counter(Metric::HttpParseReq).add(20);

        let svc = sys.start();
        assert_eq!(svc.aggregate(Metric::NetPacketsParsed), Some(300));
        assert_eq!(svc.aggregate(Metric::HttpParseReq), Some(30));
        assert_eq!(svc.aggregate(Metric::CapturePacketsReceived), None);
    }

    #[test]
    fn test_monitor_total_and_delta() {
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker("test", &[Metric::LlmCallsWithAgent]);
        let svc = sys.start();
        let mut mon = MetricsMonitor::new(svc);

        w.counter(Metric::LlmCallsWithAgent).add(5);
        let poll1 = mon.poll();
        assert_eq!(poll1.snapshot.values[&Metric::LlmCallsWithAgent], 5);
        assert_eq!(poll1.deltas[&Metric::LlmCallsWithAgent], 5);

        w.counter(Metric::LlmCallsWithAgent).add(3);
        let poll2 = mon.poll();
        assert_eq!(poll2.snapshot.values[&Metric::LlmCallsWithAgent], 8);
        assert_eq!(poll2.deltas[&Metric::LlmCallsWithAgent], 3);
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
                Metric::StorageBufferedCalls,
            ],
        );
        sys.register_queue_probe(Metric::StorageQueueDepthCalls, || 5);

        let svc = sys.start();
        svc.sample_probes();
        let mut mon = MetricsMonitor::new(svc);

        w.counter(Metric::CapturePacketsReceived).add(1000);
        w.counter(Metric::NetPacketsParsed).add(500);
        // buf_calls is a Gauge: set the current pending batch length directly.
        w.counter(Metric::StorageBufferedCalls).set(3);

        let poll = mon.poll();
        let grouped = poll.format_grouped();

        assert_eq!(grouped.len(), 3);
        assert_eq!(grouped[0].0, "capture");
        assert!(grouped[0].1.contains("pkts_received=1000/1000"));
        assert_eq!(grouped[1].0, "protocol");
        assert!(grouped[1].1.contains("pkts_parsed=500/500"));
        assert_eq!(grouped[2].0, "storage");
        assert!(
            grouped[2].1.contains("buf_calls=3"),
            "got: {}",
            grouped[2].1
        );
        assert!(
            !grouped[2].1.contains("buf_calls=3/"),
            "gauge must not include /delta, got: {}",
            grouped[2].1
        );
        assert!(grouped[2].1.contains("q_calls=5"));
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
    fn test_capped_gauge_renders_with_capacity() {
        let mut sys = MetricsSystem::new();
        sys.register_queue_probe_capped(Metric::QueueDepthRaw, 4096, || 4000);
        sys.register_queue_probe(Metric::FlowsActive, || 7); // uncapped
        let svc = sys.start();
        svc.sample_probes();

        let mut mon = MetricsMonitor::new(svc);
        let poll = mon.poll();
        let grouped = poll.format_grouped();
        // Single "protocol" line containing both gauges.
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].0, "protocol");
        let line = &grouped[0].1;
        assert!(line.contains("q_raw_pkts=4000/4096(97%)"), "got: {line}");
        assert!(line.contains("flows_active=7"), "got: {line}");
        assert!(!line.contains("flows_active=7/"), "got: {line}");
    }

    #[test]
    fn test_capped_gauge_zero_capacity_falls_back() {
        let mut sys = MetricsSystem::new();
        sys.register_queue_probe_capped(Metric::QueueDepthRaw, 0, || 5);
        let svc = sys.start();
        svc.sample_probes();

        let mut mon = MetricsMonitor::new(svc);
        let poll = mon.poll();
        let grouped = poll.format_grouped();
        // Capacity 0 means no division — fall back to plain `name=value`.
        assert!(
            grouped[0].1.contains("q_raw_pkts=5"),
            "got: {}",
            grouped[0].1
        );
        assert!(
            !grouped[0].1.contains("q_raw_pkts=5/"),
            "got: {}",
            grouped[0].1
        );
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

    #[test]
    fn aggregate_history_ring_evicts_oldest() {
        let history = AggregateHistory::new(vec![Metric::FlowsActive], 3);
        for (ts, v) in [(1, 10u64), (2, 20), (3, 30), (4, 40)] {
            let mut frame = BTreeMap::new();
            frame.insert(Metric::FlowsActive, v);
            history.push(ts, frame);
        }
        assert_eq!(history.len(), 3, "ring should evict to capacity");

        let pts = history.series(Metric::FlowsActive, 0);
        let values: Vec<u64> = pts.iter().map(|p| p.value).collect();
        assert_eq!(values, vec![20, 30, 40]);
        let timestamps: Vec<i64> = pts.iter().map(|p| p.ts_ms).collect();
        assert_eq!(timestamps, vec![2, 3, 4]);
    }

    #[test]
    fn aggregate_history_series_since_filter() {
        let history = AggregateHistory::new(vec![Metric::TurnActive], 10);
        for (ts, v) in [(100, 1u64), (200, 2), (300, 3), (400, 4)] {
            let mut frame = BTreeMap::new();
            frame.insert(Metric::TurnActive, v);
            history.push(ts, frame);
        }
        let pts = history.series(Metric::TurnActive, 250);
        let values: Vec<u64> = pts.iter().map(|p| p.value).collect();
        assert_eq!(values, vec![3, 4]);
    }

    #[test]
    fn aggregate_history_untracked_metric_yields_zero() {
        let history = AggregateHistory::new(vec![Metric::FlowsActive], 10);
        let mut frame = BTreeMap::new();
        frame.insert(Metric::FlowsActive, 7);
        history.push(50, frame);

        // Querying a metric we didn't track should return frames with 0,
        // not panic — matches the "missing == zero" reader semantics.
        let pts = history.series(Metric::TurnActive, 0);
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].value, 0);
    }

    #[test]
    fn history_recorder_sums_pipelines_and_global() {
        let mut sys_a = MetricsSystem::new();
        sys_a.register_queue_probe(Metric::FlowsActive, || 3);
        sys_a.register_queue_probe(Metric::TurnActive, || 1);
        let svc_a = sys_a.start();

        let mut sys_b = MetricsSystem::new();
        sys_b.register_queue_probe(Metric::FlowsActive, || 5);
        sys_b.register_queue_probe(Metric::TurnActive, || 2);
        let svc_b = sys_b.start();

        let history = AggregateHistory::new(vec![Metric::FlowsActive, Metric::TurnActive], 16);

        HistoryRecorder::record(&[svc_a, svc_b], &history);
        HistoryRecorder::record(&[], &history); // empty svcs should yield 0 frame

        let flows = history.series(Metric::FlowsActive, 0);
        let turns = history.series(Metric::TurnActive, 0);
        assert_eq!(flows.len(), 2);
        assert_eq!(turns.len(), 2);
        // Frame 1: a(3) + b(5) = 8 flows, a(1) + b(2) = 3 turns.
        assert_eq!(flows[0].value, 8);
        assert_eq!(turns[0].value, 3);
        // Frame 2: no contributors → 0.
        assert_eq!(flows[1].value, 0);
        assert_eq!(turns[1].value, 0);
    }
}
