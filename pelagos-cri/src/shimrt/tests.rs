//! ShimRuntime against a fake shim binary and in-process mock ttrpc Task
//! servers. No root, KVM or real shim.

use std::collections::HashSet;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use containerd_shim_protos::api;
use containerd_shim_protos::shim_async::{create_task, Task};
use containerd_shim_protos::ttrpc::asynchronous::{Server, TtrpcContext};
use containerd_shim_protos::ttrpc::{self, Code};

use super::*;

#[derive(Default)]
pub(crate) struct Mock {
    pub(crate) calls: Mutex<Vec<String>>,
    killed: Mutex<HashSet<String>>,
    /// Set by `Fixture::finish` so pending Waits resolve and shutdown is quick.
    release_all: std::sync::atomic::AtomicBool,
    /// Ids whose Create is rejected.
    reject_create: Mutex<Vec<String>>,
    creates: Mutex<Vec<api::CreateTaskRequest>>,
    execs: Mutex<Vec<api::ExecProcessRequest>>,
    /// Ids whose Connect fails (shim/VM gone).
    reject_connect: Mutex<Vec<String>>,
}

impl Mock {
    fn log(&self, s: String) {
        self.calls.lock().unwrap().push(s);
    }
    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

fn err(msg: &str) -> ttrpc::Error {
    ttrpc::Error::RpcStatus(ttrpc::get_status(Code::INTERNAL, msg))
}

#[async_trait::async_trait]
impl Task for Mock {
    async fn create(
        &self,
        _: &TtrpcContext,
        r: api::CreateTaskRequest,
    ) -> ttrpc::Result<api::CreateTaskResponse> {
        self.log(format!("create:{}", r.id));
        if self.reject_create.lock().unwrap().contains(&r.id) {
            return Err(err("rejected"));
        }
        // The "guest" prints into the stdout FIFO the CRI provided.
        if !r.stdout.is_empty() {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&r.stdout)
                .unwrap();
            f.write_all(b"hello from guest\n").unwrap();
        }
        self.creates.lock().unwrap().push(r);
        let mut resp = api::CreateTaskResponse::new();
        resp.pid = 77;
        Ok(resp)
    }
    async fn stats(
        &self,
        _: &TtrpcContext,
        r: api::StatsRequest,
    ) -> ttrpc::Result<api::StatsResponse> {
        self.log(format!("stats:{}", r.id));
        if r.id.starts_with("nostats") {
            return Err(err("no stats"));
        }
        let mut resp = api::StatsResponse::new();
        resp.stats = Some(crate::shim::stats::tests::v1_payload(5_000_000, 1000, 400)).into();
        Ok(resp)
    }
    async fn connect(
        &self,
        _: &TtrpcContext,
        r: api::ConnectRequest,
    ) -> ttrpc::Result<api::ConnectResponse> {
        self.log(format!("connect:{}", r.id));
        if self.reject_connect.lock().unwrap().contains(&r.id) {
            return Err(err("gone"));
        }
        Ok(api::ConnectResponse::new())
    }
    async fn state(
        &self,
        _: &TtrpcContext,
        r: api::StateRequest,
    ) -> ttrpc::Result<api::StateResponse> {
        self.log(format!("state:{}", r.id));
        if r.id.starts_with("lost") {
            return Err(err("no such task"));
        }
        let create = self
            .creates
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.id == r.id)
            .cloned();
        let mut resp = api::StateResponse::new();
        resp.id = r.id.clone();
        resp.pid = 77;
        if let Some(c) = create {
            resp.stdin = c.stdin;
            resp.stdout = c.stdout;
            resp.stderr = c.stderr;
        }
        if r.id.starts_with("done") {
            resp.status =
                containerd_shim_protos::protobuf::EnumOrUnknown::new(api::Status::STOPPED);
            resp.exit_status = 3;
        } else {
            resp.status =
                containerd_shim_protos::protobuf::EnumOrUnknown::new(api::Status::RUNNING);
        }
        Ok(resp)
    }
    async fn exec(
        &self,
        _: &TtrpcContext,
        r: api::ExecProcessRequest,
    ) -> ttrpc::Result<api::Empty> {
        self.log(format!("exec:{}:{}", r.id, r.exec_id));
        self.execs.lock().unwrap().push(r);
        Ok(api::Empty::new())
    }
    async fn start(
        &self,
        _: &TtrpcContext,
        r: api::StartRequest,
    ) -> ttrpc::Result<api::StartResponse> {
        self.log(format!("start:{}", r.id));
        if !r.exec_id.is_empty() {
            let req = self
                .execs
                .lock()
                .unwrap()
                .iter()
                .find(|e| e.exec_id == r.exec_id)
                .cloned()
                .unwrap();
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&req.stdout)
                .unwrap();
            f.write_all(b"execout").unwrap();
        }
        let mut resp = api::StartResponse::new();
        resp.pid = 77;
        Ok(resp)
    }
    async fn kill(&self, _: &TtrpcContext, r: api::KillRequest) -> ttrpc::Result<api::Empty> {
        self.log(format!("kill:{}:{}", r.id, r.signal));
        self.killed.lock().unwrap().insert(r.id);
        Ok(api::Empty::new())
    }
    async fn wait(
        &self,
        _: &TtrpcContext,
        r: api::WaitRequest,
    ) -> ttrpc::Result<api::WaitResponse> {
        let mut resp = api::WaitResponse::new();
        if !r.exec_id.is_empty() {
            resp.exit_status = 5;
            return Ok(resp);
        }
        if r.id.starts_with("quick") {
            tokio::time::sleep(Duration::from_millis(100)).await;
            resp.exit_status = 0;
            return Ok(resp);
        }
        for _ in 0..500 {
            if self.killed.lock().unwrap().contains(&r.id)
                || self.release_all.load(std::sync::atomic::Ordering::SeqCst)
            {
                resp.exit_status = 137;
                return Ok(resp);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(err("test wait timed out"))
    }
    async fn delete(
        &self,
        _: &TtrpcContext,
        r: api::DeleteRequest,
    ) -> ttrpc::Result<api::DeleteResponse> {
        self.log(format!("delete:{}", r.id));
        Ok(api::DeleteResponse::new())
    }
    async fn shutdown(
        &self,
        _: &TtrpcContext,
        r: api::ShutdownRequest,
    ) -> ttrpc::Result<api::Empty> {
        self.log(format!("shutdown:{}", r.id));
        Ok(api::Empty::new())
    }
}

pub(crate) struct Fixture {
    pub(crate) tmp: tempfile::TempDir,
    pub(crate) rt: Arc<ShimRuntime>,
    cfg: ShimConfig,
    pub(crate) main: Arc<Mock>,
    pub(crate) alt: Arc<Mock>,
    servers: Vec<Server>,
}

async fn serve(path: &Path) -> (Arc<Mock>, Server) {
    let mock = Arc::new(Mock::default());
    let mut s = Server::new()
        .bind(&format!("unix://{}", path.display()))
        .unwrap()
        .register_service(create_task(mock.clone()));
    s.start().await.unwrap();
    (mock, s)
}

fn fake_image(store: &StoreLayout) {
    let hex = format!("{:064x}", 1);
    let d = store.layers.join(&hex);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join(".pelagos_complete"), b"").unwrap();
    let manifest = serde_json::json!({
        "reference": "docker.io/library/app:1", "digest": "sha256:feed",
        "layers": [format!("sha256:{hex}")], "layer_types": [""],
        "config": {"cmd": ["/bin/app"], "env": ["A=1"], "working_dir": "/"},
    });
    let dir = store.images.join("docker.io_library_app_1");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
}

pub(crate) async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a.sock");
    let b = tmp.path().join("b.sock");
    let (main, s1) = serve(&a).await;
    let (alt, s2) = serve(&b).await;
    // Fake shim binary: containers whose id starts with "rej" bootstrap to the
    // alternate server, everything else (sandboxes) to the main one.
    let script = tmp.path().join("containerd-shim-fake-v2");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncase \"$*\" in *'-id rej'*) echo unix://{} ;; *) echo unix://{} ;; esac\n",
            b.display(),
            a.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let root = tmp.path().join("rt");
    let store = StoreLayout::under(&tmp.path().join("store"));
    fake_image(&store);
    let cfg = ShimConfig {
        handlers: [("fake".to_string(), script.to_string_lossy().into_owned())].into(),
        root,
        store,
    };
    let rt = Arc::new(ShimRuntime::start(cfg.clone()).await.unwrap());
    Fixture {
        tmp,
        rt,
        cfg,
        main,
        alt,
        servers: vec![s1, s2],
    }
}

pub(crate) fn sandbox(id: &str) -> CriSandbox {
    serde_json::from_value(serde_json::json!({
        "id": id, "name": "pod", "namespace": "default", "uid": "u1",
        "attempt": 0, "labels": {}, "annotations": {}, "created_at_ns": 0,
        "state": "Running", "runtime_handler": "fake",
        "dns_servers": ["10.0.0.10"], "dns_searches": ["default.svc"],
    }))
    .unwrap()
}

pub(crate) fn container(id: &str, sandbox_id: &str) -> CriContainer {
    serde_json::from_value(serde_json::json!({
        "id": id, "sandbox_id": sandbox_id, "pelagos_name": format!("pcri-{id}"),
        "name": "app", "image": "docker.io/library/app:1", "entrypoint": [], "args": [],
        "envs": [["B", "2"]], "working_dir": "", "mounts": [], "labels": {},
        "annotations": {}, "created_at_ns": 0, "started_at_ns": 0, "finished_at_ns": 0,
        "state": "Created", "exit_code": 0,
    }))
    .unwrap()
}

impl Fixture {
    pub(crate) fn reject_connect(&self, id: &str) {
        self.main.reject_connect.lock().unwrap().push(id.into());
    }

    pub(crate) async fn finish(mut self) {
        self.main
            .release_all
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.alt
            .release_all
            .store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;
        for s in &mut self.servers {
            let _ = s.shutdown().await;
        }
        self.rt.shutdown().await;
        drop(self.tmp);
    }
    fn log_path(&self, name: &str) -> std::path::PathBuf {
        self.tmp.path().join("logs").join(name)
    }
}

async fn eventually(what: &str, f: impl Fn() -> bool) {
    for _ in 0..300 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn sandbox_and_container_full_lifecycle() {
    let f = fixture().await;
    let sb = sandbox("sb1");
    f.rt.start_sandbox("fake", &sb, "/run/netns/pcri-sb1", Some("podhost"), None)
        .await
        .unwrap();
    // The sandbox bundle carries the generated spec with the pod netns.
    let cfg =
        std::fs::read_to_string(f.tmp.path().join("rt/bundles/k8s.io/sb1/config.json")).unwrap();
    assert!(
        cfg.contains("/run/netns/pcri-sb1") && cfg.contains("podhost"),
        "{cfg}"
    );
    assert_eq!(f.main.calls(), ["create:sb1", "start:sb1"]);

    let c = container("c1", "sb1");
    let log = f.log_path("c1.log");
    let pid =
        f.rt.start_container(&sb, &c, log.to_str().unwrap(), "/run/netns/pcri-sb1")
            .await
            .unwrap();
    assert_eq!(pid, 77);
    assert!(f.rt.is_running("c1"));
    assert_eq!(f.rt.container_ids_of("sb1"), ["c1"]);

    // The shim got the overlay descriptor, the bundle and the FIFOs.
    let req = f
        .main
        .creates
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.id == "c1")
        .cloned()
        .unwrap();
    assert_eq!(req.rootfs.len(), 1);
    assert_eq!(req.rootfs[0].type_, "overlay");
    assert!(req.bundle.ends_with("k8s.io/c1") && !req.stdout.is_empty() && !req.stderr.is_empty());
    // The container spec: image defaults + CRI env, sandbox identity, pod DNS mount.
    let cfg = std::fs::read_to_string(format!("{}/config.json", req.bundle)).unwrap();
    assert!(
        cfg.contains("/bin/app") && cfg.contains("A=1") && cfg.contains("B=2"),
        "{cfg}"
    );
    assert!(
        cfg.contains("io.kubernetes.cri.sandbox-id") && cfg.contains("/etc/resolv.conf"),
        "{cfg}"
    );
    let resolv =
        std::fs::read_to_string(f.tmp.path().join("rt/bundles/k8s.io/sb1/resolv.conf")).unwrap();
    assert!(resolv.contains("nameserver 10.0.0.10") && resolv.contains("search default.svc"));

    // Stop: SIGTERM, exit reported, log drained into CRI format.
    let x =
        f.rt.stop_container("c1", 15, Duration::from_secs(10))
            .await
            .unwrap();
    assert_eq!(x.exit_status, 137);
    assert!(!f.rt.is_running("c1"));
    let l = std::fs::read_to_string(&log).unwrap();
    assert!(l.contains(" stdout F hello from guest"), "{l}");
    assert!(f.main.calls().contains(&"kill:c1:15".to_string()));

    f.rt.remove_container("c1").await;
    assert!(f.main.calls().contains(&"delete:c1".to_string()));
    assert!(
        !f.tmp.path().join("rt/bundles/k8s.io/c1").exists(),
        "bundle removed"
    );
    assert!(f.rt.container_ids_of("sb1").is_empty());
    f.rt.remove_container("c1").await; // idempotent

    f.rt.stop_sandbox("sb1").await;
    let calls = f.main.calls();
    for want in ["kill:sb1:9", "delete:sb1", "shutdown:sb1"] {
        assert!(calls.contains(&want.to_string()), "{want} in {calls:?}");
    }
    f.rt.remove_sandbox("sb1");
    assert!(!f.tmp.path().join("rt/bundles/k8s.io/sb1").exists());
    f.finish().await;
}

#[tokio::test]
async fn natural_exit_is_reported_and_log_flushed_without_stop() {
    let f = fixture().await;
    let sb = sandbox("sb2");
    f.rt.start_sandbox("fake", &sb, "/run/netns/x", None, None)
        .await
        .unwrap();
    let c = container("quick1", "sb2");
    let log = f.log_path("q.log");
    f.rt.start_container(&sb, &c, log.to_str().unwrap(), "/run/netns/x")
        .await
        .unwrap();
    eventually("exit via Wait RPC", || {
        f.rt.container_exit("quick1").is_some()
    })
    .await;
    f.rt.flush_logs("quick1").await;
    assert_eq!(f.rt.container_exit("quick1").unwrap().exit_status, 0);
    assert!(std::fs::read_to_string(&log)
        .unwrap()
        .contains("hello from guest"));
    f.finish().await;
}

#[tokio::test]
async fn container_falls_back_to_shim_start_when_sandbox_shim_rejects_create() {
    let f = fixture().await;
    f.main.reject_create.lock().unwrap().push("rej1".into());
    let sb = sandbox("sb3");
    f.rt.start_sandbox("fake", &sb, "/run/netns/x", None, None)
        .await
        .unwrap();
    let c = container("rej1", "sb3");
    let log = f.log_path("r.log");
    f.rt.start_container(&sb, &c, log.to_str().unwrap(), "/run/netns/x")
        .await
        .unwrap();
    // Rejected by the sandbox shim, created and started via the bootstrapped one.
    assert!(f.main.calls().contains(&"create:rej1".to_string()));
    assert!(!f.main.calls().contains(&"start:rej1".to_string()));
    assert_eq!(f.alt.calls(), ["create:rej1", "start:rej1"]);
    // The task is driven through the shim that owns it.
    let x =
        f.rt.stop_container("rej1", 15, Duration::from_secs(10))
            .await
            .unwrap();
    assert_eq!(x.exit_status, 137);
    assert!(f.alt.calls().contains(&"kill:rej1:15".to_string()));
    assert!(!f.main.calls().contains(&"kill:rej1:15".to_string()));
    // The fallback also left the container's shim address on disk.
    f.rt.remove_container("rej1").await;
    f.finish().await;
}

#[tokio::test]
async fn failed_container_start_leaves_nothing_behind() {
    let f = fixture().await;
    // Rejected by BOTH shims.
    f.main.reject_create.lock().unwrap().push("rej2".into());
    f.alt.reject_create.lock().unwrap().push("rej2".into());
    let sb = sandbox("sb4");
    f.rt.start_sandbox("fake", &sb, "/run/netns/x", None, None)
        .await
        .unwrap();
    let c = container("rej2", "sb4");
    let log = f.log_path("f.log");
    let e =
        f.rt.start_container(&sb, &c, log.to_str().unwrap(), "/run/netns/x")
            .await;
    assert!(matches!(e, Err(ShimError::Rpc(_))), "{e:?}");
    assert!(!f.rt.is_running("rej2"));
    assert!(f.rt.container_ids_of("sb4").is_empty());
    assert!(
        !f.tmp.path().join("rt/bundles/k8s.io/rej2").exists(),
        "bundle removed"
    );
    let scratch = f.tmp.path().join("store/scratch");
    let leftover = std::fs::read_dir(&scratch).map(|d| d.count()).unwrap_or(0);
    assert_eq!(leftover, 0, "overlay scratch removed");
    f.finish().await;
}

#[tokio::test]
async fn failed_sandbox_start_shuts_the_shim_down_and_cleans_up() {
    let f = fixture().await;
    f.main.reject_create.lock().unwrap().push("sb5".into());
    let e =
        f.rt.start_sandbox("fake", &sandbox("sb5"), "/run/netns/x", None, None)
            .await;
    assert!(matches!(e, Err(ShimError::Rpc(_))), "{e:?}");
    assert!(
        f.main.calls().contains(&"shutdown:sb5".to_string()),
        "no half-started VM left"
    );
    assert!(!f.tmp.path().join("rt/bundles/k8s.io/sb5").exists());
    // And the pod is unknown afterwards.
    let c = container("c9", "sb5");
    let e =
        f.rt.start_container(&sandbox("sb5"), &c, "/dev/null", "/x")
            .await;
    assert!(matches!(e, Err(ShimError::UnknownSandbox(_))));
    f.finish().await;
}

#[tokio::test]
async fn unknown_handler_and_unknown_container_errors() {
    let f = fixture().await;
    let e =
        f.rt.start_sandbox("kata", &sandbox("s"), "/x", None, None)
            .await;
    assert!(matches!(e, Err(ShimError::UnknownHandler(_))));
    assert!(f.rt.handles("fake") && !f.rt.handles("kata") && !f.rt.handles(""));
    assert_eq!(f.rt.handler_names(), ["fake"]);
    assert!(f
        .rt
        .stop_container("nope", 15, Duration::from_secs(1))
        .await
        .is_none());
    assert!(matches!(
        f.rt.exec_sync("nope", vec!["x".into()], None).await,
        Err(ShimError::UnknownContainer(_))
    ));
    f.rt.stop_sandbox("nope").await; // idempotent
    f.finish().await;
}

#[test]
fn resolv_conf_renders_servers_searches_and_options() {
    let mut sb = sandbox("s");
    sb.dns_options = vec!["ndots:5".into()];
    assert_eq!(
        resolv_conf(&sb),
        "nameserver 10.0.0.10\nsearch default.svc\noptions ndots:5\n"
    );
}

#[tokio::test]
async fn exec_sync_runs_in_the_container_and_captures_output() {
    let f = fixture().await;
    let sb = sandbox("sb6");
    f.rt.start_sandbox("fake", &sb, "/run/netns/x", None, None)
        .await
        .unwrap();
    f.rt.start_container(
        &sb,
        &container("c6", "sb6"),
        f.log_path("e.log").to_str().unwrap(),
        "/x",
    )
    .await
    .unwrap();
    let o =
        f.rt.exec_sync(
            "c6",
            vec!["echo".into(), "hi".into()],
            Some(Duration::from_secs(10)),
        )
        .await
        .unwrap();
    assert_eq!(o.exit_code, 5);
    assert_eq!(o.captured.stdout, b"execout");
    let spec = f.main.execs.lock().unwrap()[0].spec.clone().unwrap();
    let p: oci_spec::runtime::Process = serde_json::from_slice(&spec.value).unwrap();
    assert_eq!(p.args().clone().unwrap(), ["echo", "hi"]);
    f.finish().await;
}

/// A "restarted CRI": a fresh runtime over the same root/store, as after a
/// process restart. The old runtime's pumps are closed first (its process is
/// gone in reality).
pub(crate) async fn restart(f: &Fixture, ids: &[&str]) -> Arc<ShimRuntime> {
    for id in ids {
        f.rt.flush_logs(id).await;
    }
    Arc::new(ShimRuntime::start(f.cfg.clone()).await.unwrap())
}

#[tokio::test]
async fn readopt_reattaches_running_exited_and_lost_tasks() {
    let f = fixture().await;
    let sb = sandbox("rsb");
    f.rt.start_sandbox("fake", &sb, "/run/netns/x", None, None)
        .await
        .unwrap();
    let mut specs = Vec::new();
    for id in ["live1", "done1", "lost1"] {
        let c = container(id, "rsb");
        let log = f.log_path(&format!("{id}.log"));
        f.rt.start_container(&sb, &c, log.to_str().unwrap(), "/x")
            .await
            .unwrap();
        specs.push((c, log.to_string_lossy().into_owned()));
    }
    let rt2 = restart(&f, &["live1", "done1", "lost1"]).await;
    assert!(
        rt2.container_ids_of("rsb").is_empty(),
        "fresh runtime knows nothing yet"
    );

    let report = rt2.readopt(std::slice::from_ref(&sb), &specs).await;
    assert!(report.sandboxes_lost.is_empty());
    let by: std::collections::HashMap<_, _> = report.containers.iter().cloned().collect();
    assert_eq!(by["live1"], Readopted::Running);
    assert_eq!(by["done1"], Readopted::Exited(3));
    assert_eq!(by["lost1"], Readopted::Lost);
    assert_eq!(rt2.container_exit("done1").unwrap().exit_status, 3);
    assert_eq!(rt2.container_exit("lost1").unwrap().exit_status, 137);
    assert!(rt2.container_exit("live1").is_none());

    // The running task is fully usable again: output flows to the CRI log
    // (appended after what was logged before the restart) ...
    let req = f
        .main
        .creates
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.id == "live1")
        .cloned()
        .unwrap();
    {
        let mut w = std::fs::OpenOptions::new()
            .write(true)
            .open(&req.stdout)
            .unwrap();
        w.write_all(b"after restart\n").unwrap();
    }
    // ... and stop goes through the re-attached client, then cleans up.
    let x = rt2
        .stop_container("live1", 15, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(x.exit_status, 137);
    let log = std::fs::read_to_string(f.log_path("live1.log")).unwrap();
    assert!(
        log.contains("hello from guest") && log.contains("after restart"),
        "{log}"
    );
    rt2.remove_container("live1").await;
    rt2.remove_container("done1").await;
    assert!(!f.tmp.path().join("rt/bundles/k8s.io/live1").exists());
    // The sandbox shim was re-attached too.
    rt2.stop_sandbox("rsb").await;
    assert!(f.main.calls().contains(&"shutdown:rsb".to_string()));
    rt2.shutdown().await;
    f.finish().await;
}

#[tokio::test]
async fn readopt_reports_lost_sandbox_and_leaves_its_bundle() {
    let f = fixture().await;
    let sb = sandbox("gonesb");
    f.rt.start_sandbox("fake", &sb, "/run/netns/x", None, None)
        .await
        .unwrap();
    let c = container("c1", "gonesb");
    f.rt.start_container(&sb, &c, f.log_path("g.log").to_str().unwrap(), "/x")
        .await
        .unwrap();
    f.main.reject_connect.lock().unwrap().push("gonesb".into());
    let rt2 = restart(&f, &["c1"]).await;
    let report = rt2.readopt(&[sb], &[(c, "/dev/null".into())]).await;
    assert_eq!(report.sandboxes_lost, ["gonesb"]);
    assert_eq!(report.containers, [("c1".to_string(), Readopted::Lost)]);
    assert_eq!(rt2.container_exit("c1").unwrap().exit_status, 137);
    assert!(
        f.tmp.path().join("rt/bundles/k8s.io/gonesb").exists(),
        "a dead shim's bundle may hold mounts: never deleted blindly"
    );
    rt2.shutdown().await;
    f.finish().await;
}

#[test]
fn reattach_rootfs_derives_paths_without_creating_anything() {
    let t = tempfile::tempdir().unwrap();
    let store = StoreLayout::under(t.path());
    let p = rootfs::reattach_rootfs(&store, "abcdef0123456789", vec![]);
    assert_eq!(
        p.scratch_dir(),
        store.scratch.join("cri-shim-abcdef0123456789")
    );
    assert_eq!(p.upper_dir(), p.scratch_dir().join("upper"));
    assert!(!p.uses_aliases() && !store.scratch.exists());
    std::fs::create_dir_all(store.alias_root.join("abcdef012345")).unwrap();
    assert!(rootfs::reattach_rootfs(&store, "abcdef0123456789", vec![]).uses_aliases());
}

#[tokio::test]
async fn usage_maps_shim_stats_and_degrades_to_none() {
    let f = fixture().await;
    let sb = sandbox("stsb");
    f.rt.start_sandbox("fake", &sb, "/run/netns/x", None, None)
        .await
        .unwrap();
    for id in ["st1", "nostats1"] {
        f.rt.start_container(
            &sb,
            &container(id, "stsb"),
            f.log_path("s.log").to_str().unwrap(),
            "/x",
        )
        .await
        .unwrap();
    }
    let u = f.rt.usage("st1").await.unwrap();
    assert_eq!(
        (u.cpu_nanos, u.mem_usage, u.mem_working_set),
        (5_000_000, 1000, 600)
    );
    assert!(
        f.rt.usage("nostats1").await.is_none(),
        "shim error -> None, not a failure"
    );
    assert!(f.rt.usage("unknown").await.is_none());
    f.rt.stop_container("st1", 15, Duration::from_secs(10))
        .await;
    assert!(
        f.rt.usage("st1").await.is_none(),
        "exited containers report no usage"
    );
    f.finish().await;
}

#[tokio::test]
async fn streaming_exec_session_gets_live_output_and_exit() {
    let f = fixture().await;
    let sb = sandbox("xsb");
    f.rt.start_sandbox("fake", &sb, "/run/netns/x", None, None)
        .await
        .unwrap();
    f.rt.start_container(
        &sb,
        &container("xc1", "xsb"),
        f.log_path("x.log").to_str().unwrap(),
        "/x",
    )
    .await
    .unwrap();
    let mut es =
        f.rt.start_exec(
            "xc1",
            exec::ExecRequest {
                args: vec!["sh".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // Subscribed before the process ran: the guest's first output is seen.
    let chunk = tokio::time::timeout(Duration::from_secs(5), es.output.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&chunk.data[..], b"execout");
    assert_eq!(es.session.wait(&es.client).await.unwrap(), 5);
    let client = es.client.clone();
    es.session.finish(&client).await;
    // The pump closing ends relays: the channel reports closed, not hangs.
    loop {
        match tokio::time::timeout(Duration::from_secs(5), es.output.recv()).await {
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
            Ok(_) => continue,
            Err(_) => panic!("exec output channel never closed"),
        }
    }
    assert!(matches!(
        f.rt.start_exec("nope", exec::ExecRequest::default()).await,
        Err(ShimError::UnknownContainer(_))
    ));
    f.finish().await;
}

#[tokio::test]
async fn attach_follows_live_output_and_exposes_stdin() {
    let f = fixture().await;
    let sb = sandbox("asb");
    f.rt.start_sandbox("fake", &sb, "/run/netns/x", None, None)
        .await
        .unwrap();
    let mut c = container("ac1", "asb");
    c.stdin = true;
    f.rt.start_container(&sb, &c, f.log_path("a.log").to_str().unwrap(), "/x")
        .await
        .unwrap();
    let mut at = f.rt.attach("ac1").await.unwrap();
    assert!(
        at.stdin.is_some(),
        "container created with stdin has an attachable stdin"
    );
    let req = f
        .main
        .creates
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.id == "ac1")
        .cloned()
        .unwrap();
    {
        let mut w = std::fs::OpenOptions::new()
            .write(true)
            .open(&req.stdout)
            .unwrap();
        w.write_all(b"live line\n").unwrap();
    }
    let chunk = tokio::time::timeout(Duration::from_secs(5), at.output.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&chunk.data[..], b"live line\n");
    // No stdin when the container was not created with it.
    f.rt.start_container(
        &sb,
        &container("ac2", "asb"),
        f.log_path("a2.log").to_str().unwrap(),
        "/x",
    )
    .await
    .unwrap();
    assert!(f.rt.attach("ac2").await.unwrap().stdin.is_none());
    assert!(matches!(
        f.rt.attach("nope").await,
        Err(ShimError::UnknownContainer(_))
    ));
    f.finish().await;
}

#[tokio::test]
async fn reopen_log_makes_the_pump_follow_a_rotated_file() {
    let f = fixture().await;
    let sb = sandbox("rsb9");
    f.rt.start_sandbox("fake", &sb, "/run/netns/x", None, None)
        .await
        .unwrap();
    let log = f.log_path("rot.log");
    f.rt.start_container(&sb, &container("rc9", "rsb9"), log.to_str().unwrap(), "/x")
        .await
        .unwrap();
    let req = f
        .main
        .creates
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.id == "rc9")
        .cloned()
        .unwrap();
    eventually("first line logged", || {
        std::fs::read_to_string(&log)
            .map(|s| s.contains("hello from guest"))
            .unwrap_or(false)
    })
    .await;
    let rotated = f.log_path("rot.log.1");
    std::fs::rename(&log, &rotated).unwrap();
    f.rt.reopen_log("rc9").await;
    {
        let mut w = std::fs::OpenOptions::new()
            .write(true)
            .open(&req.stdout)
            .unwrap();
        w.write_all(b"post rotation\n").unwrap();
    }
    eventually("post-rotation line in the new file", || {
        std::fs::read_to_string(&log)
            .map(|s| s.contains("post rotation"))
            .unwrap_or(false)
    })
    .await;
    assert!(!std::fs::read_to_string(&rotated)
        .unwrap()
        .contains("post rotation"));
    f.finish().await;
}
