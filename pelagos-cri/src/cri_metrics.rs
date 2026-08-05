//! Prometheus metrics endpoint for pelagos-cri.
//!
//! Foundation only (issue #497): a `/metrics` HTTP listener and basic
//! process-level gauges, proving the endpoint works end-to-end. Per-RPC and
//! per-CNI-call instrumentation land in later sub-issues of #496.

use std::net::SocketAddr;
use std::time::Instant;

use metrics::{describe_gauge, gauge};
use metrics_exporter_prometheus::PrometheusBuilder;

/// Installs the Prometheus recorder and binds the `/metrics` HTTP listener.
///
/// Must be called from within a Tokio runtime — the exporter spawns its own
/// listener task on the current runtime.
pub fn install(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    PrometheusBuilder::new()
        .with_http_listener(addr)
        .install()?;

    describe_gauge!(
        "pelagos_cri_build_info",
        "Always 1; labeled with the running pelagos-cri version"
    );
    gauge!("pelagos_cri_build_info", "version" => env!("CARGO_PKG_VERSION")).set(1.0);

    describe_gauge!(
        "pelagos_cri_uptime_seconds",
        "Seconds since this pelagos-cri process started"
    );

    let start = Instant::now();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tick.tick().await;
            gauge!("pelagos_cri_uptime_seconds").set(start.elapsed().as_secs_f64());
        }
    });

    log::info!("pelagos-cri metrics endpoint on http://{addr}/metrics");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// One test, not several: `PrometheusBuilder::install()` registers a
    /// process-global recorder and panics if called twice, so every
    /// assertion about the live endpoint has to happen in a single
    /// install() call.
    #[tokio::test]
    async fn test_metrics_endpoint_serves_build_info() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        drop(listener); // free the port for install() to rebind

        install(addr).expect("install metrics endpoint");

        // Give the exporter's listener task a moment to start accepting.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let body = tokio::task::spawn_blocking(move || {
            let mut stream = TcpStream::connect(addr).expect("connect to /metrics");
            stream
                .write_all(
                    format!("GET /metrics HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .expect("write request");
            let mut response = String::new();
            stream.read_to_string(&mut response).expect("read response");
            response
        })
        .await
        .expect("join blocking task");

        assert!(
            body.starts_with("HTTP/1.1 200"),
            "expected 200 OK, got: {body}"
        );
        assert!(
            body.contains(&format!(
                "pelagos_cri_build_info{{version=\"{}\"}} 1",
                env!("CARGO_PKG_VERSION")
            )),
            "expected build_info gauge with version label in response body: {body}"
        );
    }
}
