//! Per-RPC request count + latency metrics, plus an in-flight gauge, for the
//! CRI gRPC server.
//!
//! A generic Tower layer wrapping the whole tonic server, rather than
//! per-handler instrumentation — every `RuntimeService`/`ImageService` call
//! passes through this once, so adding a new labeled method later is a
//! match-arm change in [`grpc_method_label`], not new plumbing at each call
//! site. See #498 (CRI RPC metrics, sub-issue of #496).

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use tower_layer::Layer;
use tower_service::Service;

#[derive(Clone, Default)]
pub struct MetricsLayer;

impl<S> Layer<S> for MetricsLayer {
    type Service = MetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        metrics::describe_counter!(
            "pelagos_cri_grpc_requests_total",
            "Total CRI gRPC requests received, labeled by method"
        );
        metrics::describe_histogram!(
            "pelagos_cri_grpc_request_duration_seconds",
            "CRI gRPC request duration in seconds, labeled by method"
        );
        metrics::describe_gauge!(
            "pelagos_cri_grpc_requests_in_flight",
            "CRI gRPC requests currently in flight, across all methods (early warning before full saturation)"
        );
        MetricsService { inner }
    }
}

/// Increments the in-flight gauge on construction, decrements it on `Drop`.
/// A guard (not a plain increment-then-decrement pair around the `.await`)
/// because a client disconnecting mid-request drops the future without
/// running any code after the `.await` point — without this, a cancelled
/// request would leak a permanently-stuck +1 on the gauge. See #501.
struct InFlightGuard;

impl InFlightGuard {
    fn new() -> Self {
        metrics::gauge!("pelagos_cri_grpc_requests_in_flight").increment(1.0);
        Self
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        metrics::gauge!("pelagos_cri_grpc_requests_in_flight").decrement(1.0);
    }
}

#[derive(Clone)]
pub struct MetricsService<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> Service<http::Request<ReqBody>> for MetricsService<S>
where
    S: Service<http::Request<ReqBody>, Response = http::Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ReqBody: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqBody>) -> Self::Future {
        let method = grpc_method_label(req.uri().path());
        let started = Instant::now();

        // Tower services must not be called again until poll_ready resolves;
        // the standard pattern for wrapping in an async block is to swap in a
        // freshly-cloned inner service and move the "real" one into the future.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        Box::pin(async move {
            let _in_flight = InFlightGuard::new();
            metrics::counter!("pelagos_cri_grpc_requests_total", "method" => method).increment(1);
            let result = inner.call(req).await;
            metrics::histogram!("pelagos_cri_grpc_request_duration_seconds", "method" => method)
                .record(started.elapsed().as_secs_f64());
            result
        })
    }
}

/// Maps a gRPC request path (`/runtime.v1.RuntimeService/RunPodSandbox`) to a
/// metric label. Hot-path methods get their own label; everything else
/// collapses to `"other"` deliberately — the full ~40-method surface isn't
/// labeled individually yet (that's a separate, later pass), and an
/// intentionally bounded label set is the whole point of doing this as a
/// match statement instead of using the raw method name as the label
/// (unbounded cardinality from a malformed/unexpected path would otherwise
/// leak straight into Prometheus).
fn grpc_method_label(path: &str) -> &'static str {
    match path.rsplit('/').next().unwrap_or("") {
        "RunPodSandbox" => "RunPodSandbox",
        "StopPodSandbox" => "StopPodSandbox",
        "RemovePodSandbox" => "RemovePodSandbox",
        "CreateContainer" => "CreateContainer",
        "StartContainer" => "StartContainer",
        "StopContainer" => "StopContainer",
        "RemoveContainer" => "RemoveContainer",
        "ExecSync" => "ExecSync",
        "PullImage" => "PullImage",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grpc_method_label_hot_path_methods() {
        assert_eq!(
            grpc_method_label("/runtime.v1.RuntimeService/RunPodSandbox"),
            "RunPodSandbox"
        );
        assert_eq!(
            grpc_method_label("/runtime.v1.ImageService/PullImage"),
            "PullImage"
        );
    }

    #[test]
    fn test_grpc_method_label_unlisted_methods_collapse_to_other() {
        assert_eq!(
            grpc_method_label("/runtime.v1.RuntimeService/Version"),
            "other"
        );
        assert_eq!(grpc_method_label("/not/a/grpc/path/at/all"), "other");
        assert_eq!(grpc_method_label(""), "other");
    }

    /// A trivial `Service` standing in for the tonic-generated one — the
    /// `MetricsLayer`/`MetricsService` code doesn't know or care that it's
    /// wrapping a real gRPC handler, only that it's a `tower_service::Service`
    /// over `http::Request`/`http::Response`.
    #[derive(Clone)]
    struct EchoService;

    impl Service<http::Request<()>> for EchoService {
        type Response = http::Response<()>;
        type Error = std::convert::Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: http::Request<()>) -> Self::Future {
            Box::pin(async { Ok(http::Response::new(())) })
        }
    }

    /// A `Service` whose response doesn't resolve until the test releases it —
    /// needed to observe the in-flight gauge mid-request rather than only
    /// before/after.
    #[derive(Clone)]
    struct SlowEchoService {
        release: std::sync::Arc<tokio::sync::Notify>,
    }

    impl Service<http::Request<()>> for SlowEchoService {
        type Response = http::Response<()>;
        type Error = std::convert::Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: http::Request<()>) -> Self::Future {
            let release = self.release.clone();
            Box::pin(async move {
                release.notified().await;
                Ok(http::Response::new(()))
            })
        }
    }

    fn gauge_value(
        snapshot: &[(
            metrics_util::CompositeKey,
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            metrics_util::debugging::DebugValue,
        )],
        name: &str,
    ) -> Option<f64> {
        use metrics_util::debugging::DebugValue;
        snapshot.iter().find_map(|(ck, _, _, v)| {
            (ck.key().name() == name)
                .then_some(v)
                .and_then(|v| match v {
                    DebugValue::Gauge(g) => Some(g.into_inner()),
                    _ => None,
                })
        })
    }

    /// #501: the in-flight gauge must reflect a request that is genuinely
    /// still pending, not just increment-then-immediately-decrement around a
    /// call that always resolves synchronously. `metrics_util`'s
    /// `DebuggingRecorder` reports gauges as the delta since the last
    /// snapshot (its `Snapshotter::snapshot()` swaps the stored value to 0),
    /// so — deliberately — this test takes exactly one snapshot, while the
    /// request is still pending. See `test_in_flight_gauge_nets_to_zero_...`
    /// below for the balanced increment/decrement check via a second,
    /// independent single-snapshot test.
    #[tokio::test(flavor = "current_thread")]
    async fn test_in_flight_gauge_reflects_pending_request() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut svc = MetricsLayer.layer(SlowEchoService {
            release: release.clone(),
        });
        let req = http::Request::builder()
            .uri("/runtime.v1.RuntimeService/ExecSync")
            .body(())
            .unwrap();

        let call_future = svc.call(req);
        let handle = tokio::task::spawn(call_future);
        // Let the spawned task actually run up to its `.await` point before
        // we inspect the gauge.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let snapshot = snapshotter.snapshot().into_vec();
        assert_eq!(
            gauge_value(&snapshot, "pelagos_cri_grpc_requests_in_flight"),
            Some(1.0),
            "expected in-flight gauge to read 1 while a request is genuinely pending, snapshot: {snapshot:?}"
        );

        // Release the held request so the spawned task doesn't leak past the test.
        release.notify_one();
        handle.await.unwrap().unwrap();
    }

    /// #501: companion to the test above — over the full lifecycle of one
    /// request (increment on start, decrement on completion), the net change
    /// reported by a single snapshot taken after completion must be zero.
    /// A missing decrement would show +1; a missing increment (impossible
    /// given the current code, but this is what would catch it) would show
    /// -1.
    #[tokio::test(flavor = "current_thread")]
    async fn test_in_flight_gauge_nets_to_zero_after_request_completes() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let mut svc = MetricsLayer.layer(EchoService);
        let req = http::Request::builder()
            .uri("/runtime.v1.RuntimeService/ExecSync")
            .body(())
            .unwrap();
        svc.call(req).await.unwrap();

        let snapshot = snapshotter.snapshot().into_vec();
        assert_eq!(
            gauge_value(&snapshot, "pelagos_cri_grpc_requests_in_flight"),
            Some(0.0),
            "expected increment (+1) and decrement (-1) to net to zero after the request completed, snapshot: {snapshot:?}"
        );
    }

    /// #498: a request through `MetricsLayer` must record both a request
    /// count and a latency sample under the method label derived from the
    /// request path, without altering the inner service's response.
    /// `current_thread` runtime + `set_default_local_recorder` for the same
    /// reason as the `#[serial(cni_semaphore)]` tests in `cni.rs` — the
    /// thread-local test recorder must stay active across `.await` points.
    #[tokio::test(flavor = "current_thread")]
    async fn test_metrics_layer_records_request_for_hot_path_method() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let mut svc = MetricsLayer.layer(EchoService);
        let req = http::Request::builder()
            .uri("/runtime.v1.RuntimeService/RunPodSandbox")
            .body(())
            .unwrap();

        let response = svc.call(req).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);

        let snapshot = snapshotter.snapshot().into_vec();

        let count = snapshot.iter().find_map(|(ck, _, _, v)| {
            let matches = ck.key().name() == "pelagos_cri_grpc_requests_total"
                && ck
                    .key()
                    .labels()
                    .any(|l| l.key() == "method" && l.value() == "RunPodSandbox");
            matches.then_some(v).and_then(|v| match v {
                DebugValue::Counter(n) => Some(*n),
                _ => None,
            })
        });
        assert_eq!(
            count,
            Some(1),
            "expected requests_total to increment for method=RunPodSandbox, snapshot: {snapshot:?}"
        );

        let latency_samples = snapshot.iter().find_map(|(ck, _, _, v)| {
            let matches = ck.key().name() == "pelagos_cri_grpc_request_duration_seconds"
                && ck
                    .key()
                    .labels()
                    .any(|l| l.key() == "method" && l.value() == "RunPodSandbox");
            matches.then_some(v).and_then(|v| match v {
                DebugValue::Histogram(samples) => Some(samples.len()),
                _ => None,
            })
        });
        assert_eq!(
            latency_samples,
            Some(1),
            "expected one duration sample for method=RunPodSandbox, snapshot: {snapshot:?}"
        );
    }
}
