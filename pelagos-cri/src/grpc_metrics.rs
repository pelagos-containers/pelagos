//! Per-RPC request count + latency metrics, plus an in-flight gauge, for the
//! CRI gRPC server.
//!
//! A generic Tower layer wrapping the whole tonic server, rather than
//! per-handler instrumentation — every `RuntimeService`/`ImageService` call
//! passes through this once, so adding a new labeled method later is a
//! match-arm change in [`grpc_method_label`], not new plumbing at each call
//! site. See #498 (CRI RPC metrics, sub-issue of #496).
//!
//! ## Success/failure and sandbox-not-found detection (#554)
//!
//! `pelagos_cri_grpc_requests_total` originally carried only a `method`
//! label — a failing `CreateContainer` incremented the exact same series as
//! a succeeding one, so the metric could not answer "is this method
//! failing?" at all (see #553, a 41-day-undetected sandbox-state bug that
//! this metric should have surfaced but couldn't). This module now reads
//! the gRPC status of each completed call off the **response headers**
//! (never the body) and adds it as a `code` label, plus emits a dedicated
//! `pelagos_cri_sandbox_not_found_total` counter for the #553 bug class
//! specifically. See [`grpc_status_label`] for why reading headers (and not
//! trailers) is correct for every CRI method pelagos-cri implements today.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use percent_encoding::percent_decode_str;
use tower_layer::Layer;
use tower_service::Service;

#[derive(Clone, Default)]
pub struct MetricsLayer;

impl<S> Layer<S> for MetricsLayer {
    type Service = MetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        metrics::describe_counter!(
            "pelagos_cri_grpc_requests_total",
            "Total CRI gRPC requests completed, labeled by method and gRPC status code \
             (code=\"OK\" on success; see #554)"
        );
        metrics::describe_histogram!(
            "pelagos_cri_grpc_request_duration_seconds",
            "CRI gRPC request duration in seconds, labeled by method"
        );
        metrics::describe_gauge!(
            "pelagos_cri_grpc_requests_in_flight",
            "CRI gRPC requests currently in flight, across all methods (early warning before full saturation)"
        );
        metrics::describe_counter!(
            "pelagos_cri_sandbox_not_found_total",
            "CRI gRPC requests that failed because a referenced pod sandbox was not \
             found or pelagos-cri's tracked sandbox state disagreed with reality — the \
             #553 bug class — labeled by method"
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
            let result = inner.call(req).await;
            metrics::histogram!("pelagos_cri_grpc_request_duration_seconds", "method" => method)
                .record(started.elapsed().as_secs_f64());

            match &result {
                Ok(response) => {
                    let headers = response.headers();
                    let code = grpc_status_label(headers);
                    metrics::counter!(
                        "pelagos_cri_grpc_requests_total",
                        "method" => method,
                        "code" => code,
                    )
                    .increment(1);

                    if code != "OK" && is_sandbox_state_mismatch(headers) {
                        metrics::counter!("pelagos_cri_sandbox_not_found_total", "method" => method)
                            .increment(1);
                    }
                }
                Err(_) => {
                    // A transport-level failure below the gRPC status layer (e.g. the
                    // connection was reset). pelagos-cri's own services never take this
                    // path — tonic always turns a handler's `Err(Status)` into an
                    // `Ok(Response)` carrying the status in headers (`Status::into_http`)
                    // — but the generic Tower bound on `S::Error` allows it, so it still
                    // needs a label rather than silently going uncounted.
                    metrics::counter!(
                        "pelagos_cri_grpc_requests_total",
                        "method" => method,
                        "code" => "TRANSPORT_ERROR",
                    )
                    .increment(1);
                }
            }

            result
        })
    }
}

/// Maps a gRPC request path (`/runtime.v1.RuntimeService/RunPodSandbox`) to a
/// metric label. Covers the full CRI v1 RPC surface (`pelagos-cri/proto/api.proto`)
/// so steady-state polling traffic (PLEG relist, stats collection, health
/// checks) is attributable instead of collapsing into `"other"`. An
/// intentionally bounded label set is still the whole point of doing this as
/// a match statement instead of using the raw method name as the label
/// (unbounded cardinality from a malformed/unexpected path would otherwise
/// leak straight into Prometheus) — `_ => "other"` remains the catch-all for
/// anything not in the proto. See #507.
fn grpc_method_label(path: &str) -> &'static str {
    match path.rsplit('/').next().unwrap_or("") {
        "RunPodSandbox" => "RunPodSandbox",
        "StopPodSandbox" => "StopPodSandbox",
        "RemovePodSandbox" => "RemovePodSandbox",
        "PodSandboxStatus" => "PodSandboxStatus",
        "ListPodSandbox" => "ListPodSandbox",
        "StreamPodSandboxes" => "StreamPodSandboxes",
        "CreateContainer" => "CreateContainer",
        "StartContainer" => "StartContainer",
        "StopContainer" => "StopContainer",
        "RemoveContainer" => "RemoveContainer",
        "ListContainers" => "ListContainers",
        "StreamContainers" => "StreamContainers",
        "ContainerStatus" => "ContainerStatus",
        "UpdateContainerResources" => "UpdateContainerResources",
        "ReopenContainerLog" => "ReopenContainerLog",
        "ExecSync" => "ExecSync",
        "Exec" => "Exec",
        "Attach" => "Attach",
        "PortForward" => "PortForward",
        "ContainerStats" => "ContainerStats",
        "ListContainerStats" => "ListContainerStats",
        "StreamContainerStats" => "StreamContainerStats",
        "PodSandboxStats" => "PodSandboxStats",
        "ListPodSandboxStats" => "ListPodSandboxStats",
        "StreamPodSandboxStats" => "StreamPodSandboxStats",
        "UpdateRuntimeConfig" => "UpdateRuntimeConfig",
        "Status" => "Status",
        "CheckpointContainer" => "CheckpointContainer",
        "GetContainerEvents" => "GetContainerEvents",
        "ListMetricDescriptors" => "ListMetricDescriptors",
        "ListPodSandboxMetrics" => "ListPodSandboxMetrics",
        "StreamPodSandboxMetrics" => "StreamPodSandboxMetrics",
        "RuntimeConfig" => "RuntimeConfig",
        "UpdatePodSandboxResources" => "UpdatePodSandboxResources",
        "ListImages" => "ListImages",
        "StreamImages" => "StreamImages",
        "ImageStatus" => "ImageStatus",
        "PullImage" => "PullImage",
        "RemoveImage" => "RemoveImage",
        "ImageFsInfo" => "ImageFsInfo",
        "Version" => "Version",
        _ => "other",
    }
}

/// Reads the gRPC status of a completed response and maps it to its
/// canonical name — a bounded, well-known set of 17 values (same
/// cardinality discipline as [`grpc_method_label`]).
///
/// This reads the **response headers**, not HTTP/2 trailers, and that is
/// deliberately correct for pelagos-cri today, not a shortcut: tonic sends
/// a handler's `Err(status)` as a Trailers-Only response — `grpc-status`
/// lands directly in the immediate response headers, with an empty body
/// (`tonic::Status::into_http`, used by `Grpc::unary`/`Grpc::server_streaming`
/// for every synchronous handler error). Only a *successful* response's
/// `grpc-status: 0` is deferred to real HTTP/2 trailers, written after the
/// body's data frames — so treating "no `grpc-status` header" as `"OK"` is
/// exact, not a guess. Every CRI method pelagos-cri implements resolves to a
/// single `Ok(Response)`/`Err(Status)` before any body framing begins (its
/// `Stream*` RPCs are unimplemented stubs that error immediately, and
/// `Exec`/`Attach`/`PortForward` return one redirect-URL message, not a
/// long-lived stream) — none of them can fail *after* headers are sent. A
/// future genuinely bidirectional-streaming RPC that errors mid-stream would
/// be undercounted as `"OK"` here; catching that would require wrapping the
/// response body to inspect its closing trailer frame, which is a larger
/// change left as a follow-up if/when such an RPC is actually implemented.
fn grpc_status_label(headers: &http::HeaderMap) -> &'static str {
    match headers
        .get(tonic::Status::GRPC_STATUS)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i32>().ok())
    {
        None => "OK",
        Some(code) => grpc_code_name(code),
    }
}

/// Canonical gRPC status code names (`google.rpc.Code`), 0-16.
fn grpc_code_name(code: i32) -> &'static str {
    match code {
        0 => "OK",
        1 => "CANCELLED",
        2 => "UNKNOWN",
        3 => "INVALID_ARGUMENT",
        4 => "DEADLINE_EXCEEDED",
        5 => "NOT_FOUND",
        6 => "ALREADY_EXISTS",
        7 => "PERMISSION_DENIED",
        8 => "RESOURCE_EXHAUSTED",
        9 => "FAILED_PRECONDITION",
        10 => "ABORTED",
        11 => "OUT_OF_RANGE",
        12 => "UNIMPLEMENTED",
        13 => "INTERNAL",
        14 => "UNAVAILABLE",
        15 => "DATA_LOSS",
        16 => "UNAUTHENTICATED",
        _ => "UNKNOWN_CODE",
    }
}

/// Detects the #553 bug class from a failed response's `grpc-message`:
/// pelagos-cri's tracked sandbox state disagreeing with reality. This
/// surfaces from two different code paths that both end up in the message
/// text but don't share a type: a direct `Status::not_found(...)` from a
/// CRI-level sandbox lookup (`pod_sandbox_status`, `port_forward`,
/// `pod_sandbox_stats` — e.g. `"sandbox {id} not found"`), or a
/// `Status::internal("pelagos run failed: {stderr}")` wrapping the native
/// runtime's `SandboxError::NotFound` from a `pelagos run --sandbox <id>`
/// subprocess (`start_container` — #553's actual failure mode, e.g.
/// `"pelagos run failed: sandbox not found: <id>"`).
///
/// Matched loosely — the message contains both "sandbox" and "not found",
/// case-insensitively — rather than one fixed string, because the exact
/// wording differs by call site and is not a stable API in either
/// direction. This is the same reasoning [`grpc_method_label`] documents
/// for not keying metrics on unbounded strings: the message text is only
/// ever used here to decide whether to increment a bounded,
/// already-`method`-labeled counter, never as a label value itself.
fn is_sandbox_state_mismatch(headers: &http::HeaderMap) -> bool {
    let Some(value) = headers.get(tonic::Status::GRPC_MESSAGE) else {
        return false;
    };
    let Ok(raw) = value.to_str() else {
        return false;
    };
    let decoded = percent_decode_str(raw).decode_utf8_lossy().to_lowercase();
    decoded.contains("sandbox") && decoded.contains("not found")
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

    /// #507: the steady-state PLEG/stats/health-polling methods that used to
    /// collapse into `"other"` now get their own label.
    #[test]
    fn test_grpc_method_label_steady_state_polling_methods() {
        assert_eq!(
            grpc_method_label("/runtime.v1.RuntimeService/ListPodSandbox"),
            "ListPodSandbox"
        );
        assert_eq!(
            grpc_method_label("/runtime.v1.RuntimeService/ListContainers"),
            "ListContainers"
        );
        assert_eq!(
            grpc_method_label("/runtime.v1.RuntimeService/PodSandboxStatus"),
            "PodSandboxStatus"
        );
        assert_eq!(
            grpc_method_label("/runtime.v1.RuntimeService/ContainerStatus"),
            "ContainerStatus"
        );
        assert_eq!(
            grpc_method_label("/runtime.v1.RuntimeService/ListContainerStats"),
            "ListContainerStats"
        );
        assert_eq!(
            grpc_method_label("/runtime.v1.RuntimeService/Status"),
            "Status"
        );
        assert_eq!(
            grpc_method_label("/runtime.v1.RuntimeService/Version"),
            "Version"
        );
    }

    #[test]
    fn test_grpc_method_label_unlisted_methods_collapse_to_other() {
        assert_eq!(
            grpc_method_label("/runtime.v1.RuntimeService/NotARealCriMethod"),
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

        // #554: a successful response must also record code="OK".
        let ok_count = snapshot.iter().find_map(|(ck, _, _, v)| {
            let matches = ck.key().name() == "pelagos_cri_grpc_requests_total"
                && ck
                    .key()
                    .labels()
                    .any(|l| l.key() == "method" && l.value() == "RunPodSandbox")
                && ck
                    .key()
                    .labels()
                    .any(|l| l.key() == "code" && l.value() == "OK");
            matches.then_some(v).and_then(|v| match v {
                DebugValue::Counter(n) => Some(*n),
                _ => None,
            })
        });
        assert_eq!(
            ok_count,
            Some(1),
            "expected requests_total{{method=RunPodSandbox,code=OK}} == 1, snapshot: {snapshot:?}"
        );
    }

    // ── #554: gRPC status label + sandbox-not-found detection ──────────────

    #[test]
    fn test_grpc_status_label_defaults_to_ok_without_header() {
        let headers = http::HeaderMap::new();
        assert_eq!(grpc_status_label(&headers), "OK");
    }

    #[test]
    fn test_grpc_status_label_maps_known_codes() {
        for (code, name) in [
            (0, "OK"),
            (5, "NOT_FOUND"),
            (13, "INTERNAL"),
            (14, "UNAVAILABLE"),
            (12, "UNIMPLEMENTED"),
        ] {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                tonic::Status::GRPC_STATUS,
                code.to_string().parse().unwrap(),
            );
            assert_eq!(
                grpc_status_label(&headers),
                name,
                "grpc-status {code} should map to {name}"
            );
        }
    }

    #[test]
    fn test_grpc_status_label_unknown_or_malformed_falls_back() {
        let mut out_of_range = http::HeaderMap::new();
        out_of_range.insert(tonic::Status::GRPC_STATUS, "999".parse().unwrap());
        assert_eq!(grpc_status_label(&out_of_range), "UNKNOWN_CODE");

        let mut malformed = http::HeaderMap::new();
        malformed.insert(tonic::Status::GRPC_STATUS, "not-a-number".parse().unwrap());
        assert_eq!(grpc_status_label(&malformed), "UNKNOWN_CODE");
    }

    /// The real failure text from `SandboxError::NotFound` wrapped by
    /// `start_container` (#553), percent-encoded the way tonic actually
    /// encodes `grpc-message` (spaces become `%20`) — this must decode
    /// correctly, not just match on luck because the raw bytes happen to
    /// contain "sandbox" and "not" and "found" as encoded substrings.
    #[test]
    fn test_is_sandbox_state_mismatch_matches_percent_encoded_message() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            tonic::Status::GRPC_MESSAGE,
            "pelagos%20run%20failed%3A%20sandbox%20not%20found%3A%20abc123"
                .parse()
                .unwrap(),
        );
        assert!(is_sandbox_state_mismatch(&headers));
    }

    /// The CRI-level lookup phrasing ("sandbox {id} not found") has the id
    /// between the two words — must still match via the "contains both
    /// tokens" check, not a single fixed substring.
    #[test]
    fn test_is_sandbox_state_mismatch_matches_id_in_the_middle() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            tonic::Status::GRPC_MESSAGE,
            "sandbox%20abc123%20not%20found".parse().unwrap(),
        );
        assert!(is_sandbox_state_mismatch(&headers));
    }

    #[test]
    fn test_is_sandbox_state_mismatch_ignores_unrelated_errors() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            tonic::Status::GRPC_MESSAGE,
            "container%20not%20found".parse().unwrap(),
        );
        assert!(!is_sandbox_state_mismatch(&headers));

        let mut no_message = http::HeaderMap::new();
        no_message.insert(tonic::Status::GRPC_STATUS, "13".parse().unwrap());
        assert!(!is_sandbox_state_mismatch(&no_message));
    }

    /// A `Service` that fails with a real tonic-encoded `Status`, via
    /// `Status::into_http()` — the exact conversion `tonic::server::Grpc`
    /// applies to every handler's `Err(status)` return (see
    /// `grpc_status_label`'s doc comment). This drives the layer against
    /// tonic's actual wire encoding (including real percent-encoding of the
    /// message), not a hand-rolled header, so the test below is exercising
    /// the same code path a live failing `CreateContainer`/`StartContainer`
    /// call takes in production.
    #[derive(Clone)]
    struct FailingService {
        status_msg: &'static str,
        code: tonic::Code,
    }

    impl Service<http::Request<()>> for FailingService {
        type Response = http::Response<tonic::body::BoxBody>;
        type Error = std::convert::Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: http::Request<()>) -> Self::Future {
            let status = tonic::Status::new(self.code, self.status_msg);
            Box::pin(async move { Ok(status.into_http()) })
        }
    }

    fn find_counter(
        snapshot: &[(
            metrics_util::CompositeKey,
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            metrics_util::debugging::DebugValue,
        )],
        name: &str,
        labels: &[(&str, &str)],
    ) -> Option<u64> {
        use metrics_util::debugging::DebugValue;
        snapshot.iter().find_map(|(ck, _, _, v)| {
            let matches = ck.key().name() == name
                && labels.iter().all(|(k, val)| {
                    ck.key()
                        .labels()
                        .any(|l| l.key() == *k && l.value() == *val)
                });
            matches.then_some(v).and_then(|v| match v {
                DebugValue::Counter(n) => Some(*n),
                _ => None,
            })
        })
    }

    /// End-to-end: a real tonic `Status::not_found` from `StartContainer`
    /// (#553's failure mode) through `MetricsLayer` must record both
    /// `pelagos_cri_grpc_requests_total{method=StartContainer,code=NOT_FOUND}`
    /// and `pelagos_cri_sandbox_not_found_total{method=StartContainer}`.
    #[tokio::test(flavor = "current_thread")]
    async fn test_metrics_layer_counts_real_sandbox_not_found_status() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let mut svc = MetricsLayer.layer(FailingService {
            status_msg: "pelagos run failed: sandbox not found: abc123",
            code: tonic::Code::NotFound,
        });
        let req = http::Request::builder()
            .uri("/runtime.v1.RuntimeService/StartContainer")
            .body(())
            .unwrap();

        let response = svc.call(req).await.unwrap();
        // Trailers-Only error response: grpc-status is in the immediate headers.
        assert_eq!(
            response.headers().get("grpc-status").unwrap(),
            "5" // NOT_FOUND
        );

        let snapshot = snapshotter.snapshot().into_vec();

        assert_eq!(
            find_counter(
                &snapshot,
                "pelagos_cri_grpc_requests_total",
                &[("method", "StartContainer"), ("code", "NOT_FOUND")],
            ),
            Some(1),
            "expected requests_total{{method=StartContainer,code=NOT_FOUND}} == 1, snapshot: {snapshot:?}"
        );
        assert_eq!(
            find_counter(
                &snapshot,
                "pelagos_cri_sandbox_not_found_total",
                &[("method", "StartContainer")],
            ),
            Some(1),
            "expected sandbox_not_found_total{{method=StartContainer}} == 1, snapshot: {snapshot:?}"
        );
    }

    /// A `NOT_FOUND` that is NOT about a sandbox (e.g. "container not found"
    /// from `container_status`) must still count in `requests_total` under
    /// `code=NOT_FOUND`, but must NOT increment `sandbox_not_found_total` —
    /// that counter is specifically the #553 bug class, not every not-found.
    #[tokio::test(flavor = "current_thread")]
    async fn test_metrics_layer_does_not_miscount_unrelated_not_found_as_sandbox() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let mut svc = MetricsLayer.layer(FailingService {
            status_msg: "container not found",
            code: tonic::Code::NotFound,
        });
        let req = http::Request::builder()
            .uri("/runtime.v1.RuntimeService/ContainerStatus")
            .body(())
            .unwrap();
        svc.call(req).await.unwrap();

        let snapshot = snapshotter.snapshot().into_vec();
        assert_eq!(
            find_counter(
                &snapshot,
                "pelagos_cri_grpc_requests_total",
                &[("method", "ContainerStatus"), ("code", "NOT_FOUND")],
            ),
            Some(1),
        );
        assert_eq!(
            find_counter(
                &snapshot,
                "pelagos_cri_sandbox_not_found_total",
                &[("method", "ContainerStatus")],
            ),
            None,
            "a plain 'container not found' must not be counted as a sandbox mismatch, snapshot: {snapshot:?}"
        );
    }
}
