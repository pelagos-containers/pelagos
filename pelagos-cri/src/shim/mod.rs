//! containerd shim v2 *client* (epic #574, Kata and friends).
//!
//! `pelagos-cri` plays the role containerd / CRI-O play in the standard stack:
//! it runs the shim binary's `start` bootstrap, learns the shim's ttrpc
//! address, and then speaks the Task API to it. See
//! `docs/SHIM_V2_CLIENT_DESIGN.md`.
//!
//! - [`bootstrap`]: legacy `start` / `delete` subcommand protocol.
//! - [`bundle`]: per-container bundle directory and persisted shim address.
//! - [`client`]: ttrpc Task client wrapper.
//! - [`events`]: ttrpc `Events` sink shims publish lifecycle events to.
//! - [`exit`]: decodes those events and tracks per-container exit state.
//! - [`rootfs`]: overlay `Mount` descriptors over the layer store.
//! - [`spec`]: CRI -> OCI `config.json` generation for shim bundles.
//!
//! Nothing here is wired into the CRI request path yet; later epic items
//! (OCI spec generation, events sink, re-adoption) consume it.
#![allow(dead_code)]

pub mod bootstrap;
pub mod bundle;
pub mod client;
pub mod events;
pub mod exit;
pub mod rootfs;
pub mod spec;

/// Loopback integration: a fake shim `start` script prints the address of an
/// in-process ttrpc Task server; we bootstrap it, persist the address, connect
/// and run a Create/Start/State/Kill/Wait/Delete round trip. Needs no root,
/// KVM or real shim.
#[cfg(test)]
mod roundtrip {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use containerd_shim_protos::api;
    use containerd_shim_protos::shim_async::{create_task, Task};
    use containerd_shim_protos::ttrpc::asynchronous::{Server, TtrpcContext};
    use containerd_shim_protos::ttrpc::{self, Code};

    use super::bootstrap::{self, StartParams};
    use super::bundle::Bundle;
    use super::client::ShimClient;

    fn generated_config() -> Vec<u8> {
        use super::spec::{build_sandbox_spec, to_config_json, SandboxSpecInput};
        let sb: crate::state::CriSandbox = serde_json::from_value(serde_json::json!({
            "id": "ctr1", "name": "pod", "namespace": "default", "uid": "u",
            "attempt": 0, "labels": {}, "annotations": {}, "created_at_ns": 0,
            "state": "Running",
        }))
        .unwrap();
        let spec = build_sandbox_spec(&SandboxSpecInput {
            sandbox: &sb,
            netns_path: None,
            resources: None,
            hostname: None,
            pause_args: &[],
        })
        .unwrap();
        to_config_json(&spec).unwrap()
    }

    #[derive(Default)]
    struct MockTask {
        calls: Mutex<Vec<String>>,
    }

    impl MockTask {
        fn log(&self, s: &str) {
            self.calls.lock().unwrap().push(s.to_string());
        }
    }

    #[async_trait::async_trait]
    impl Task for MockTask {
        async fn create(
            &self,
            _: &TtrpcContext,
            req: api::CreateTaskRequest,
        ) -> ttrpc::Result<api::CreateTaskResponse> {
            self.log(&format!("create:{}:{}", req.id, req.bundle));
            let mut r = api::CreateTaskResponse::new();
            r.pid = 4242;
            Ok(r)
        }
        async fn start(
            &self,
            _: &TtrpcContext,
            req: api::StartRequest,
        ) -> ttrpc::Result<api::StartResponse> {
            self.log(&format!("start:{}", req.id));
            let mut r = api::StartResponse::new();
            r.pid = 4242;
            Ok(r)
        }
        async fn state(
            &self,
            _: &TtrpcContext,
            req: api::StateRequest,
        ) -> ttrpc::Result<api::StateResponse> {
            self.log(&format!("state:{}", req.id));
            let mut r = api::StateResponse::new();
            r.id = req.id;
            r.pid = 4242;
            Ok(r)
        }
        async fn kill(&self, _: &TtrpcContext, req: api::KillRequest) -> ttrpc::Result<api::Empty> {
            self.log(&format!("kill:{}:{}", req.id, req.signal));
            Ok(api::Empty::new())
        }
        async fn wait(
            &self,
            _: &TtrpcContext,
            req: api::WaitRequest,
        ) -> ttrpc::Result<api::WaitResponse> {
            self.log(&format!("wait:{}", req.id));
            let mut r = api::WaitResponse::new();
            r.exit_status = 137;
            Ok(r)
        }
        async fn delete(
            &self,
            _: &TtrpcContext,
            req: api::DeleteRequest,
        ) -> ttrpc::Result<api::DeleteResponse> {
            self.log(&format!("delete:{}", req.id));
            Ok(api::DeleteResponse::new())
        }
        async fn connect(
            &self,
            _: &TtrpcContext,
            req: api::ConnectRequest,
        ) -> ttrpc::Result<api::ConnectResponse> {
            self.log(&format!("connect:{}", req.id));
            let mut r = api::ConnectResponse::new();
            r.shim_pid = 7;
            r.task_pid = 4242;
            Ok(r)
        }
        async fn pids(
            &self,
            _: &TtrpcContext,
            _: api::PidsRequest,
        ) -> ttrpc::Result<api::PidsResponse> {
            Err(ttrpc::Error::RpcStatus(ttrpc::get_status(
                Code::NOT_FOUND,
                "unimplemented in mock",
            )))
        }
    }

    #[tokio::test]
    async fn bootstrap_connect_and_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("shim.sock");
        let mock = Arc::new(MockTask::default());
        let mut server = Server::new()
            .bind(&format!("unix://{}", sock.display()))
            .unwrap()
            .register_service(create_task(mock.clone()));
        server.start().await.unwrap();

        // Fake shim binary: records its cwd and args, prints the address.
        let script = tmp.path().join("containerd-shim-fake-v2");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\npwd > ./start.cwd\necho \"$@\" > ./start.args\necho unix://{}\n",
                sock.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let bundle = Bundle::create(&tmp.path().join("b"), "k8s.io", "ctr1").unwrap();
        // A real generated spec, not a stub, so the bundle the shim sees is the
        // one the CRI would hand it.
        bundle.write_config(&generated_config()).unwrap();
        let p = StartParams {
            binary: script.to_string_lossy().into_owned(),
            namespace: "k8s.io".into(),
            id: "ctr1".into(),
            address: "/run/pelagos-cri/containerd.sock".into(),
            ttrpc_address: "/run/pelagos-cri/events.sock".into(),
            publish_binary: "/bin/true".into(),
            bundle: bundle.dir().to_path_buf(),
        };
        let addr = bootstrap::start(&p, Duration::from_secs(10)).await.unwrap();
        assert_eq!(addr.address, format!("unix://{}", sock.display()));
        // The shim ran inside the bundle directory with the standard flags.
        let cwd = std::fs::read_to_string(bundle.dir().join("start.cwd")).unwrap();
        assert!(cwd.trim().ends_with("k8s.io/ctr1"), "cwd was {cwd}");
        let args = std::fs::read_to_string(bundle.dir().join("start.args")).unwrap();
        assert!(args.contains("-id ctr1") && args.trim().ends_with("start"));

        bundle.persist_shim(&p.binary, &addr).unwrap();
        let rec = bundle.load_shim().unwrap();

        // Reconnect from the persisted record, as re-adoption will.
        let c = ShimClient::connect(&rec.address).await.unwrap();
        let conn = c.connect_task("ctr1").await.unwrap();
        assert_eq!((conn.shim_pid, conn.task_pid), (7, 4242));

        let mut req = api::CreateTaskRequest::new();
        req.id = "ctr1".into();
        req.bundle = bundle.dir().to_string_lossy().into_owned();
        assert_eq!(c.create(req).await.unwrap().pid, 4242);
        assert_eq!(c.start("ctr1").await.unwrap().pid, 4242);
        assert_eq!(c.state("ctr1").await.unwrap().id, "ctr1");
        c.kill("ctr1", 9, false).await.unwrap();
        assert_eq!(c.wait("ctr1").await.unwrap().exit_status, 137);
        c.delete("ctr1").await.unwrap();
        // A shim error propagates as an Err, not a panic.
        assert!(c.pids("ctr1").await.is_err());

        let calls = mock.calls.lock().unwrap().clone();
        assert_eq!(calls[0], "connect:ctr1");
        assert!(calls[1].starts_with("create:ctr1:"));
        assert_eq!(
            &calls[2..],
            [
                "start:ctr1",
                "state:ctr1",
                "kill:ctr1:9",
                "wait:ctr1",
                "delete:ctr1"
            ]
        );
        server.shutdown().await.unwrap();
    }

    /// #585: the shim must get the *sink* in `TTRPC_ADDRESS` while `-address`
    /// stays the containerd-style endpoint, and the recorded path must be one a
    /// shim can actually connect to and publish through.
    #[tokio::test]
    #[serial_test::serial(ttrpc_env)]
    async fn bootstrap_exports_events_sink_as_ttrpc_address() {
        use containerd_shim_protos::api::ForwardRequest;
        use containerd_shim_protos::shim::event::Envelope;
        use containerd_shim_protos::shim_async::{Client, EventsClient};
        use containerd_shim_protos::ttrpc::context;

        use super::events::{placeholder_publish_binary, EventsSink};

        let tmp = tempfile::tempdir().unwrap();
        let (mut sink, mut rx) = EventsSink::start(&tmp.path().join("ev.sock"))
            .await
            .unwrap();

        let script = tmp.path().join("containerd-shim-fake-v2");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s' \"${TTRPC_ADDRESS-UNSET}\" > ./ttrpc.env\necho \"$@\" > ./args\necho unix:///x\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bundle = Bundle::create(&tmp.path().join("b"), "k8s.io", "c").unwrap();
        let mut p = StartParams {
            binary: script.to_string_lossy().into_owned(),
            namespace: "k8s.io".into(),
            id: "c".into(),
            address: "/run/containerd.sock".into(),
            ttrpc_address: sink.address(),
            publish_binary: placeholder_publish_binary(),
            bundle: bundle.dir().to_path_buf(),
        };
        bootstrap::start(&p, Duration::from_secs(10)).await.unwrap();

        let env = std::fs::read_to_string(bundle.dir().join("ttrpc.env")).unwrap();
        assert_eq!(env, sink.address(), "TTRPC_ADDRESS is the events sink");
        let args = std::fs::read_to_string(bundle.dir().join("args")).unwrap();
        assert!(args.contains("-address /run/containerd.sock"), "{args}");
        assert!(
            !args.contains("-publish-binary -"),
            "publish binary non-empty: {args}"
        );

        // Publish through the exact path the shim was handed.
        let c = EventsClient::new(Client::connect(&format!("unix://{env}")).await.unwrap());
        let mut e = Envelope::new();
        e.topic = "/tasks/exit".into();
        e.namespace = "k8s.io".into();
        let mut r = ForwardRequest::new();
        r.envelope = Some(e).into();
        c.forward(context::with_timeout(5_000_000_000), &r)
            .await
            .unwrap();
        assert_eq!(rx.recv().await.unwrap().topic, "/tasks/exit");

        // Empty ttrpc_address: never inherit someone else's sink.
        std::env::set_var("TTRPC_ADDRESS", "/some/other/sink");
        p.ttrpc_address = String::new();
        bootstrap::start(&p, Duration::from_secs(10)).await.unwrap();
        std::env::remove_var("TTRPC_ADDRESS");
        assert_eq!(
            std::fs::read_to_string(bundle.dir().join("ttrpc.env")).unwrap(),
            "UNSET"
        );
        sink.shutdown().await;
    }

    #[tokio::test]
    async fn failing_shim_reports_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("shim");
        std::fs::write(&script, "#!/bin/sh\necho boom >&2\nexit 3\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let p = StartParams {
            binary: script.to_string_lossy().into_owned(),
            namespace: "k8s.io".into(),
            id: "x".into(),
            address: "a".into(),
            ttrpc_address: String::new(),
            publish_binary: "p".into(),
            bundle: tmp.path().to_path_buf(),
        };
        match bootstrap::start(&p, Duration::from_secs(5)).await {
            Err(bootstrap::BootstrapError::Failed(Some(3), e)) => assert!(e.contains("boom")),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
