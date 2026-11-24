//! In-process mock ttrpc Task server: no root, KVM or real shim.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use containerd_shim_protos::api;
use containerd_shim_protos::shim_async::{create_task, Task};
use containerd_shim_protos::ttrpc::asynchronous::{Server, TtrpcContext};
use containerd_shim_protos::ttrpc::{self};
use oci_spec::runtime::{ProcessBuilder, UserBuilder};

use super::*;

#[derive(Clone)]
enum Behavior {
    /// Write the given stdout/stderr, then exit with the code.
    Finish {
        out: &'static [u8],
        err: &'static [u8],
        code: u32,
    },
    /// Never exit until killed.
    Hang,
}

#[derive(Default)]
struct Mock {
    behavior: Mutex<Option<Behavior>>,
    execs: Mutex<HashMap<String, api::ExecProcessRequest>>,
    calls: Mutex<Vec<String>>,
    killed: Mutex<bool>,
    stdin_seen: Mutex<Vec<u8>>,
}

#[async_trait::async_trait]
impl Task for Mock {
    async fn exec(
        &self,
        _: &TtrpcContext,
        r: api::ExecProcessRequest,
    ) -> ttrpc::Result<api::Empty> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("exec:{}:{}", r.id, r.exec_id));
        self.execs.lock().unwrap().insert(r.exec_id.clone(), r);
        Ok(api::Empty::new())
    }
    async fn start(
        &self,
        _: &TtrpcContext,
        r: api::StartRequest,
    ) -> ttrpc::Result<api::StartResponse> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("start:{}", r.exec_id));
        let req = self.execs.lock().unwrap().get(&r.exec_id).cloned().unwrap();
        match self.behavior.lock().unwrap().clone().unwrap() {
            Behavior::Finish { out, err, .. } => {
                // The "guest" writes to the FIFOs the CRI handed over.
                let mut o = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&req.stdout)
                    .unwrap();
                o.write_all(out).unwrap();
                if !req.stderr.is_empty() {
                    let mut e = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&req.stderr)
                        .unwrap();
                    e.write_all(err).unwrap();
                }
                if !req.stdin.is_empty() {
                    // Non-blocking drain of whatever stdin holds at exit time.
                    let path = req.stdin.clone();
                    let seen = &self.stdin_seen;
                    let f = std::fs::OpenOptions::new()
                        .read(true)
                        .custom_flags_nonblock(&path);
                    if let Some(mut f) = f {
                        let mut v = Vec::new();
                        let _ = f.read_to_end(&mut v);
                        seen.lock().unwrap().extend(v);
                    }
                }
            }
            Behavior::Hang => {}
        }
        let mut resp = api::StartResponse::new();
        resp.pid = 99;
        Ok(resp)
    }
    async fn wait(
        &self,
        _: &TtrpcContext,
        r: api::WaitRequest,
    ) -> ttrpc::Result<api::WaitResponse> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("wait:{}", r.exec_id));
        let b = self.behavior.lock().unwrap().clone().unwrap();
        let mut resp = api::WaitResponse::new();
        match b {
            Behavior::Finish { code, .. } => resp.exit_status = code,
            Behavior::Hang => {
                for _ in 0..500 {
                    if *self.killed.lock().unwrap() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                resp.exit_status = 137;
            }
        }
        Ok(resp)
    }
    async fn kill(&self, _: &TtrpcContext, r: api::KillRequest) -> ttrpc::Result<api::Empty> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("kill:{}:{}", r.exec_id, r.signal));
        *self.killed.lock().unwrap() = true;
        Ok(api::Empty::new())
    }
    async fn delete(
        &self,
        _: &TtrpcContext,
        r: api::DeleteRequest,
    ) -> ttrpc::Result<api::DeleteResponse> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("delete:{}", r.exec_id));
        Ok(api::DeleteResponse::new())
    }
    async fn resize_pty(
        &self,
        _: &TtrpcContext,
        r: api::ResizePtyRequest,
    ) -> ttrpc::Result<api::Empty> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("resize:{}:{}x{}", r.exec_id, r.width, r.height));
        Ok(api::Empty::new())
    }
    async fn close_io(
        &self,
        _: &TtrpcContext,
        r: api::CloseIORequest,
    ) -> ttrpc::Result<api::Empty> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("close_io:{}:{}", r.exec_id, r.stdin));
        Ok(api::Empty::new())
    }
}

trait NonBlock {
    fn custom_flags_nonblock(&mut self, path: &str) -> Option<std::fs::File>;
}
impl NonBlock for std::fs::OpenOptions {
    fn custom_flags_nonblock(&mut self, path: &str) -> Option<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        self.custom_flags(libc::O_NONBLOCK).open(path).ok()
    }
}

async fn setup(b: Behavior) -> (tempfile::TempDir, Arc<Mock>, Server, ShimClient) {
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("s.sock");
    let mock = Arc::new(Mock::default());
    *mock.behavior.lock().unwrap() = Some(b);
    let mut server = Server::new()
        .bind(&format!("unix://{}", sock.display()))
        .unwrap()
        .register_service(create_task(mock.clone()));
    server.start().await.unwrap();
    let client = ShimClient::connect(&sock.to_string_lossy()).await.unwrap();
    (tmp, mock, server, client)
}

fn base() -> Process {
    ProcessBuilder::default()
        .args(vec!["/pause".to_string()])
        .env(vec!["PATH=/usr/bin".to_string(), "FOO=orig".to_string()])
        .cwd("/")
        .user(
            UserBuilder::default()
                .uid(1000u32)
                .gid(1000u32)
                .build()
                .unwrap(),
        )
        .no_new_privileges(true)
        .build()
        .unwrap()
}

#[test]
fn spec_derives_from_container_process() {
    let req = ExecRequest {
        args: vec!["ls".into(), "-l".into()],
        env: vec!["FOO=new".into(), "BAR=1".into()],
        cwd: Some("/tmp".into()),
        tty: true,
        stdin: false,
    };
    let any = exec_spec(&base(), &req).unwrap();
    assert_eq!(any.type_url, PROCESS_TYPE_URL);
    let p: Process = serde_json::from_slice(&any.value).unwrap();
    assert_eq!(p.args().clone().unwrap(), ["ls", "-l"]);
    assert_eq!(p.terminal(), Some(true));
    assert_eq!(p.cwd().to_str(), Some("/tmp"));
    let env = p.env().clone().unwrap();
    assert!(env.contains(&"PATH=/usr/bin".to_string()));
    assert!(env.contains(&"FOO=new".to_string()) && !env.contains(&"FOO=orig".to_string()));
    assert!(env.contains(&"BAR=1".to_string()));
    // Security-relevant settings are inherited, not reset.
    assert_eq!(p.user().uid(), 1000);
    assert_eq!(p.no_new_privileges(), Some(true));
}

#[test]
fn spec_rejects_empty_command() {
    assert!(matches!(
        exec_spec(&base(), &ExecRequest::default()),
        Err(ExecError::EmptyCommand)
    ));
}

#[test]
fn base_process_reads_generated_bundle_config() {
    let tmp = tempfile::tempdir().unwrap();
    let spec = oci_spec::runtime::SpecBuilder::default()
        .process(base())
        .build()
        .unwrap();
    std::fs::write(
        tmp.path().join("config.json"),
        serde_json::to_vec(&spec).unwrap(),
    )
    .unwrap();
    assert_eq!(base_process(tmp.path()).unwrap().user().uid(), 1000);
    assert!(matches!(
        base_process(&tmp.path().join("nope")),
        Err(ExecError::Config(_))
    ));
}

#[tokio::test]
async fn exec_sync_captures_output_and_exit_code() {
    let (tmp, mock, mut server, client) = setup(Behavior::Finish {
        out: b"hello\n",
        err: b"boom\n",
        code: 7,
    })
    .await;
    let dir = tmp.path().join("io");
    let r = ExecRequest {
        args: vec!["sh".into()],
        ..Default::default()
    };
    let o = exec_sync(
        &client,
        "c1",
        "e1",
        &base(),
        &r,
        &dir,
        ExecLimits {
            timeout: Some(Duration::from_secs(10)),
            capture_limit: 1024,
        },
    )
    .await
    .unwrap();
    assert_eq!(o.exit_code, 7);
    assert!(!o.timed_out);
    assert_eq!(o.captured.stdout, b"hello\n");
    assert_eq!(o.captured.stderr, b"boom\n");
    let calls = mock.calls.lock().unwrap().clone();
    assert_eq!(calls, ["exec:c1:e1", "start:e1", "wait:e1", "delete:e1"]);
    // The process spec the shim received is the derived one.
    let req = mock.execs.lock().unwrap().get("e1").cloned().unwrap();
    assert_eq!(req.spec.type_url, PROCESS_TYPE_URL);
    // FIFOs are cleaned up.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "fifos removed");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn exec_sync_timeout_kills_and_flags() {
    let (tmp, mock, mut server, client) = setup(Behavior::Hang).await;
    let r = ExecRequest {
        args: vec!["sleep".into(), "99".into()],
        ..Default::default()
    };
    let o = exec_sync(
        &client,
        "c1",
        "e2",
        &base(),
        &r,
        &tmp.path().join("io"),
        ExecLimits {
            timeout: Some(Duration::from_millis(200)),
            capture_limit: 1024,
        },
    )
    .await
    .unwrap();
    assert!(o.timed_out);
    assert_eq!(o.exit_code, TIMEOUT_EXIT_CODE);
    let calls = mock.calls.lock().unwrap().clone();
    assert!(calls.contains(&"kill:e2:9".to_string()), "{calls:?}");
    assert!(calls.contains(&"delete:e2".to_string()), "{calls:?}");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn exec_sync_shim_error_still_cleans_up() {
    // No behavior set: Mock::start unwraps -> the server errors the RPC.
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("s.sock");
    let mock = Arc::new(Mock::default());
    *mock.behavior.lock().unwrap() = Some(Behavior::Hang);
    let mut server = Server::new()
        .bind(&format!("unix://{}", sock.display()))
        .unwrap()
        .register_service(create_task(mock.clone()));
    server.start().await.unwrap();
    let client = ShimClient::connect(&sock.to_string_lossy()).await.unwrap();
    server.shutdown().await.unwrap();
    let dir = tmp.path().join("io");
    let r = ExecRequest {
        args: vec!["x".into()],
        ..Default::default()
    };
    let res = exec_sync(
        &client,
        "c",
        "e3",
        &base(),
        &r,
        &dir,
        ExecLimits {
            timeout: None,
            capture_limit: 16,
        },
    )
    .await;
    assert!(matches!(res, Err(ExecError::Shim(_))), "{res:?}");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "fifos removed on error"
    );
}

#[tokio::test]
async fn streaming_session_resize_close_and_finish() {
    let (tmp, mock, mut server, client) = setup(Behavior::Finish {
        out: b"live\n",
        err: b"",
        code: 0,
    })
    .await;
    let dir = tmp.path().join("io");
    let r = ExecRequest {
        args: vec!["sh".into()],
        stdin: true,
        tty: true,
        ..Default::default()
    };
    let mut rx = None;
    let s = ExecSession::start_with(&client, "c1", "e4", &base(), &r, &dir, |p| {
        rx = Some(p.subscribe())
    })
    .await
    .unwrap();
    // Subscribed before start: the first output is not missed.
    let chunk = tokio::time::timeout(Duration::from_secs(5), rx.as_mut().unwrap().recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&chunk.data[..], b"live\n");
    // Input path is wired.
    s.stdin.as_ref().unwrap().write_all(b"x").await.unwrap();
    s.resize(&client, 120, 40).await.unwrap();
    s.close_stdin(&client).await;
    assert_eq!(s.wait(&client).await.unwrap(), 0);
    s.finish(&client).await;
    let calls = mock.calls.lock().unwrap().clone();
    assert!(calls.contains(&"resize:e4:120x40".to_string()), "{calls:?}");
    assert!(calls.contains(&"close_io:e4:true".to_string()), "{calls:?}");
    assert!(calls.contains(&"delete:e4".to_string()), "{calls:?}");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    server.shutdown().await.unwrap();
}
