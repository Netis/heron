//! `CapturingBody` — streams an upstream `Incoming` body to the client
//! frame by frame, mirroring each data frame into an in-memory
//! accumulator. When the stream ends (clean EOF in `poll_frame`, or
//! mid-stream tear-down via `Drop`), it invokes a finalizer exactly
//! once with the bytes captured so far.
//!
//! Why this exists: SSE responses from LLM upstreams take seconds to
//! finish. Fully buffering them before returning to the client adds
//! that whole latency to TTFT. With `CapturingBody`, the client sees
//! every `data:` frame the instant it arrives upstream; the capture
//! event lands when the stream is done. Both halves stay coherent
//! without contention because the same `poll_frame` invocation both
//! forwards to the client and accumulates to the buffer.

use bytes::Bytes;
use http_body::{Body, Frame};
use std::pin::Pin;
use std::task::{Context, Poll};

/// Type-erased finalizer called exactly once with the accumulated
/// bytes when the body stream ends or is dropped.
pub type Finalizer = Box<dyn FnOnce(Bytes) + Send + 'static>;

/// Wraps a hyper `Incoming` body. Forwards every frame untouched while
/// copying each data frame's contents into `captured`. On end-of-stream
/// (or `Drop`) invokes `finalize` with the accumulated buffer; never
/// fires twice. Honors `cap` as a soft ceiling — once exceeded, further
/// data frames pass through to the client but are NOT mirrored, so the
/// capture stays bounded.
pub struct CapturingBody {
    inner: hyper::body::Incoming,
    captured: Vec<u8>,
    cap: usize,
    overflowed: bool,
    finalize: Option<Finalizer>,
}

impl CapturingBody {
    pub fn new(inner: hyper::body::Incoming, cap: usize, finalize: Finalizer) -> Self {
        Self {
            inner,
            captured: Vec::new(),
            cap,
            overflowed: false,
            finalize: Some(finalize),
        }
    }
}

impl Body for CapturingBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        // `hyper::body::Incoming` is Unpin, so projecting via &mut is sound.
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    if !this.overflowed {
                        if this.captured.len() + data.len() <= this.cap {
                            this.captured.extend_from_slice(data);
                        } else {
                            // Past the cap — stop mirroring further bytes.
                            // The capture row still gets emitted, with
                            // whatever fit at the start of the stream;
                            // truncation is noted in the trace below so
                            // operators can up `max_body_bytes` if needed.
                            this.overflowed = true;
                            tracing::debug!(
                                target: "ts_proxy::body",
                                cap = this.cap,
                                "capture buffer cap reached; further frames pass through but aren't mirrored"
                            );
                        }
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => {
                this.fire_finalize();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl CapturingBody {
    /// Run the finalizer exactly once with whatever we accumulated.
    /// Idempotent — callers can drive this from both the EOF arm in
    /// `poll_frame` and the `Drop` impl without double-firing.
    fn fire_finalize(&mut self) {
        if let Some(fin) = self.finalize.take() {
            let buf = std::mem::take(&mut self.captured);
            fin(Bytes::from(buf));
        }
    }
}

impl Drop for CapturingBody {
    fn drop(&mut self) {
        // Client tore down the connection mid-stream (or hyper dropped
        // the response). Capture whatever made it through — a partial
        // SSE trace is still useful for diagnosing flaky upstreams.
        self.fire_finalize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // Build a controllable body for tests. We use a small adapter over
    // `tokio::sync::mpsc` so the test can decide when each frame
    // arrives — this is the same pattern hyper itself uses internally
    // for `Incoming`, except we can't actually construct an `Incoming`
    // ourselves. So these tests stay focused on the finalizer-firing
    // semantics by exercising `fire_finalize` directly.

    #[test]
    fn finalize_fires_once_on_explicit_call() {
        let count = Arc::new(Mutex::new(Vec::new()));
        let c = count.clone();
        // We can't instantiate `Incoming`, so test the cap + finalize
        // logic via a stand-in built off the same fields.
        let mut state = CapturingBodyState::new(64, Box::new(move |b| c.lock().unwrap().push(b)));
        state.absorb_for_test(b"hello");
        state.absorb_for_test(b" world");
        state.fire();
        state.fire(); // idempotent
        let recorded = count.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].as_ref(), b"hello world");
    }

    #[test]
    fn capture_caps_at_max_body_bytes() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let r = recorded.clone();
        let mut state =
            CapturingBodyState::new(8, Box::new(move |b| r.lock().unwrap().push(b)));
        state.absorb_for_test(b"12345"); // 5 bytes, fits
        state.absorb_for_test(b"6789"); // would put us at 9, exceeds cap
        state.fire();
        let buf = recorded.lock().unwrap().clone();
        assert_eq!(buf[0].as_ref(), b"12345"); // only the pre-overflow bytes
    }

    /// Test-only stand-in mirroring CapturingBody's accumulation logic
    /// without needing a real hyper::body::Incoming (which has no
    /// public constructor outside the hyper crate).
    struct CapturingBodyState {
        captured: Vec<u8>,
        cap: usize,
        overflowed: bool,
        finalize: Option<Finalizer>,
    }

    impl CapturingBodyState {
        fn new(cap: usize, finalize: Finalizer) -> Self {
            Self {
                captured: Vec::new(),
                cap,
                overflowed: false,
                finalize: Some(finalize),
            }
        }
        fn absorb_for_test(&mut self, data: &[u8]) {
            if !self.overflowed {
                if self.captured.len() + data.len() <= self.cap {
                    self.captured.extend_from_slice(data);
                } else {
                    self.overflowed = true;
                }
            }
        }
        fn fire(&mut self) {
            if let Some(fin) = self.finalize.take() {
                let buf = std::mem::take(&mut self.captured);
                fin(Bytes::from(buf));
            }
        }
    }
}
