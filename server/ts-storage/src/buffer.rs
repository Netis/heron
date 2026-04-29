//! Batching consumer that reads records from a pipeline `mpsc::Receiver`,
//! batches them by size-or-time, and invokes a flush function per batch.
//!
//! Runs directly on the upstream channel — no intermediate staging channel —
//! so backpressure propagates straight back to the producing stage.

use std::future::Future;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, error};

use ts_common::error::Result;
use ts_common::internal_metrics::MetricHandle;
use ts_common::throttle::ThrottledWarn;

/// How often flush errors get a log line when failures are continuous. The
/// `StorageFlushErrors` counter still ticks on every drop, so operators can
/// see the true rate via metrics even when the log is throttled.
const FLUSH_ERR_WARN_THROTTLE: Duration = Duration::from_secs(10);

/// Optional metric handles for a WriteBuffer instance.
///
/// `buffered` is a gauge: the current pending batch length, set after every
/// push and reset to 0 after each flush. `flushed` and `errors` remain
/// monotonic counters carrying the throughput / failure signals.
#[derive(Clone)]
pub struct BufferMetrics {
    pub buffered: MetricHandle,
    pub flushed: MetricHandle,
    pub errors: MetricHandle,
}

pub struct WriteBuffer<T> {
    entity: &'static str,
    rx: mpsc::Receiver<T>,
    batch_size: usize,
    flush_interval: Duration,
    metrics: Option<BufferMetrics>,
    err_throttle: ThrottledWarn,
}

impl<T: Send + 'static> WriteBuffer<T> {
    /// `entity` is a short tag used in tracing logs: `calls`, `turns`, `metrics`.
    pub fn new(
        entity: &'static str,
        rx: mpsc::Receiver<T>,
        batch_size: usize,
        flush_interval: Duration,
        metrics: Option<BufferMetrics>,
    ) -> Self {
        Self {
            entity,
            rx,
            batch_size,
            flush_interval,
            metrics,
            err_throttle: ThrottledWarn::new(FLUSH_ERR_WARN_THROTTLE),
        }
    }

    /// Run the buffer loop. Calls `flush_fn` with each batch.
    /// Returns once the upstream channel closes and any remaining batch has
    /// been flushed.
    pub async fn run<F, Fut>(mut self, flush_fn: F)
    where
        F: Fn(Vec<T>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send,
    {
        let mut batch: Vec<T> = Vec::with_capacity(self.batch_size);
        let mut interval = tokio::time::interval(self.flush_interval);
        // The first tick completes immediately — consume it so the first
        // real deadline is one full interval from now.
        interval.tick().await;

        loop {
            tokio::select! {
                item = self.rx.recv() => {
                    match item {
                        Some(item) => {
                            batch.push(item);
                            if let Some(ref m) = self.metrics {
                                m.buffered.set(batch.len() as u64);
                            }
                            if batch.len() >= self.batch_size {
                                let to_flush = std::mem::replace(
                                    &mut batch,
                                    Vec::with_capacity(self.batch_size),
                                );
                                self.flush(&flush_fn, to_flush, "size").await;
                                if let Some(ref m) = self.metrics { m.buffered.set(0); }
                                interval.reset();
                            }
                        }
                        None => {
                            if !batch.is_empty() {
                                self.flush(&flush_fn, batch, "shutdown").await;
                                if let Some(ref m) = self.metrics { m.buffered.set(0); }
                            }
                            debug!(entity = self.entity, "write buffer stopping: upstream EOF");
                            return;
                        }
                    }
                }
                _ = interval.tick() => {
                    if !batch.is_empty() {
                        let to_flush = std::mem::replace(
                            &mut batch,
                            Vec::with_capacity(self.batch_size),
                        );
                        self.flush(&flush_fn, to_flush, "time").await;
                        if let Some(ref m) = self.metrics { m.buffered.set(0); }
                    }
                }
            }
        }
    }

    async fn flush<F, Fut>(&mut self, flush_fn: &F, batch: Vec<T>, trigger: &'static str)
    where
        F: Fn(Vec<T>) -> Fut + Send,
        Fut: Future<Output = Result<()>> + Send,
    {
        let batch_len = batch.len();
        let entity = self.entity;
        let start = Instant::now();
        match flush_fn(batch).await {
            Ok(()) => {
                let flush_ms = start.elapsed().as_millis() as u64;
                debug!(entity, batch_len, trigger, flush_ms, "flushed");
                if let Some(ref m) = self.metrics {
                    m.flushed.add(batch_len as u64);
                }
            }
            Err(e) => {
                // No generic retry here: Vec<T> isn't Clone in general, and
                // cloning a batch on every flush to enable retry would
                // double-allocate the hot path. Callers should make the
                // backend itself idempotent/durable if needed.
                if let Some(ref m) = self.metrics {
                    m.errors.inc();
                }
                if let Some(suppressed) = self.err_throttle.tick() {
                    if suppressed > 0 {
                        error!(
                            entity, batch_len, trigger, suppressed, error = %e,
                            "flush failed; batch dropped (latest of many)"
                        );
                    } else {
                        error!(
                            entity, batch_len, trigger, error = %e,
                            "flush failed; batch dropped"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn test_flush_on_batch_size() {
        let flush_count = Arc::new(AtomicUsize::new(0));
        let flush_count_clone = flush_count.clone();

        let (tx, rx) = mpsc::channel::<i32>(16);
        let buffer = WriteBuffer::new("test", rx, 3, Duration::from_secs(60), None);

        let task = tokio::spawn(async move {
            buffer
                .run(move |batch| {
                    let fc = flush_count_clone.clone();
                    async move {
                        fc.fetch_add(batch.len(), Ordering::SeqCst);
                        Ok(())
                    }
                })
                .await;
        });

        for i in 0..6 {
            tx.send(i).await.unwrap();
        }

        drop(tx);
        task.await.unwrap();

        assert_eq!(flush_count.load(Ordering::SeqCst), 6);
    }

    #[tokio::test]
    async fn test_flush_on_interval() {
        let flush_count = Arc::new(AtomicUsize::new(0));
        let flush_count_clone = flush_count.clone();

        let (tx, rx) = mpsc::channel::<i32>(16);
        let buffer = WriteBuffer::new("test", rx, 1000, Duration::from_millis(50), None);

        let task = tokio::spawn(async move {
            buffer
                .run(move |batch| {
                    let fc = flush_count_clone.clone();
                    async move {
                        fc.fetch_add(batch.len(), Ordering::SeqCst);
                        Ok(())
                    }
                })
                .await;
        });

        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();

        tokio::time::sleep(Duration::from_millis(150)).await;

        drop(tx);
        task.await.unwrap();

        assert_eq!(flush_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_flush_remaining_on_shutdown() {
        let flush_count = Arc::new(AtomicUsize::new(0));
        let flush_count_clone = flush_count.clone();

        let (tx, rx) = mpsc::channel::<i32>(16);
        let buffer = WriteBuffer::new("test", rx, 100, Duration::from_secs(60), None);

        let task = tokio::spawn(async move {
            buffer
                .run(move |batch| {
                    let fc = flush_count_clone.clone();
                    async move {
                        fc.fetch_add(batch.len(), Ordering::SeqCst);
                        Ok(())
                    }
                })
                .await;
        });

        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();

        drop(tx);
        task.await.unwrap();

        assert_eq!(flush_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_flush_failure_does_not_stop_loop() {
        // First flush returns Err; second flush must still get driven and
        // the shutdown flush must still run.
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_clone = attempts.clone();

        let (tx, rx) = mpsc::channel::<i32>(16);
        let buffer = WriteBuffer::new("test", rx, 2, Duration::from_secs(60), None);

        let task = tokio::spawn(async move {
            buffer
                .run(move |batch| {
                    let a = attempts_clone.clone();
                    async move {
                        let n = a.fetch_add(1, Ordering::SeqCst);
                        let _ = batch;
                        if n == 0 {
                            Err(ts_common::error::AppError::Storage(
                                "synthetic failure".to_string(),
                            ))
                        } else {
                            Ok(())
                        }
                    }
                })
                .await;
        });

        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap(); // triggers size flush → Err (dropped)
        tx.send(3).await.unwrap();
        tx.send(4).await.unwrap(); // triggers size flush → Ok
        drop(tx);

        task.await.unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
}
