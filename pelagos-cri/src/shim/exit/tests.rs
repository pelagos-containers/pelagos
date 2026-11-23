use std::sync::Arc;
use std::time::Duration;

use containerd_shim_protos::api;
use containerd_shim_protos::events::task as evt;
use containerd_shim_protos::protobuf::well_known_types::any::Any;
use containerd_shim_protos::protobuf::well_known_types::timestamp::Timestamp;
use containerd_shim_protos::protobuf::Message;
use containerd_shim_protos::shim::event::Envelope;
use containerd_shim_protos::shim_async::{create_task, Client, EventsClient, Task};
use containerd_shim_protos::ttrpc::asynchronous::{Server, TtrpcContext};
use containerd_shim_protos::ttrpc::{self, context};

use super::*;
use crate::shim::events::EventsSink;

fn stamp(secs: i64) -> Option<Timestamp> {
    let mut t = Timestamp::new();
    t.seconds = secs;
    t.nanos = 7;
    Some(t)
}

fn exit_ev(container: &str, process: &str, pid: u32, status: u32) -> ShimEvent {
    let mut m = evt::TaskExit::new();
    m.container_id = container.into();
    m.id = process.into();
    m.pid = pid;
    m.exit_status = status;
    m.exited_at = stamp(1_700_000_000).into();
    ShimEvent {
        topic: "/tasks/exit".into(),
        namespace: "k8s.io".into(),
        timestamp: None,
        type_url: "containerd.events.TaskExit".into(),
        payload: m.write_to_bytes().unwrap(),
    }
}

fn start_ev(container: &str, pid: u32) -> ShimEvent {
    let mut m = evt::TaskStart::new();
    m.container_id = container.into();
    m.pid = pid;
    ShimEvent {
        topic: "/tasks/start".into(),
        namespace: "k8s.io".into(),
        timestamp: None,
        type_url: "containerd.events.TaskStart".into(),
        payload: m.write_to_bytes().unwrap(),
    }
}

fn apply_raw(t: &ExitTracker, ev: &ShimEvent) {
    t.apply(decode(ev).expect("decodes"));
}

#[test]
fn decode_each_event_type() {
    let mut c = evt::TaskCreate::new();
    c.container_id = "c".into();
    c.pid = 11;
    let ev = |url: &str, payload: Vec<u8>| ShimEvent {
        topic: "/tasks/x".into(),
        namespace: "ns".into(),
        timestamp: None,
        type_url: url.into(),
        payload,
    };
    assert_eq!(
        decode(&ev(
            "containerd.events.TaskCreate",
            c.write_to_bytes().unwrap()
        )),
        Some(TaskEvent::Create {
            container_id: "c".into(),
            pid: 11
        })
    );
    assert_eq!(
        decode(&start_ev("c", 12)),
        Some(TaskEvent::Start {
            container_id: "c".into(),
            pid: 12
        })
    );
    // Host-prefixed type URLs decode by their final segment.
    let mut o = evt::TaskOOM::new();
    o.container_id = "c".into();
    assert_eq!(
        decode(&ev(
            "types.containerd.io/containerd.events.TaskOOM",
            o.write_to_bytes().unwrap()
        )),
        Some(TaskEvent::Oom {
            container_id: "c".into()
        })
    );
    let mut ea = evt::TaskExecAdded::new();
    ea.container_id = "c".into();
    ea.exec_id = "e1".into();
    assert_eq!(
        decode(&ev(
            "containerd.events.TaskExecAdded",
            ea.write_to_bytes().unwrap()
        )),
        Some(TaskEvent::ExecAdded {
            container_id: "c".into(),
            exec_id: "e1".into()
        })
    );
    let mut es = evt::TaskExecStarted::new();
    es.container_id = "c".into();
    es.exec_id = "e1".into();
    es.pid = 99;
    assert_eq!(
        decode(&ev(
            "containerd.events.TaskExecStarted",
            es.write_to_bytes().unwrap()
        )),
        Some(TaskEvent::ExecStarted {
            container_id: "c".into(),
            exec_id: "e1".into(),
            pid: 99
        })
    );
    let mut d = evt::TaskDelete::new();
    d.container_id = "c".into();
    d.pid = 12;
    d.exit_status = 3;
    d.exited_at = stamp(5).into();
    assert_eq!(
        decode(&ev(
            "containerd.events.TaskDelete",
            d.write_to_bytes().unwrap()
        )),
        Some(TaskEvent::Delete {
            container_id: "c".into(),
            pid: 12,
            exit_status: 3,
            exited_at: Some((5, 7))
        })
    );
    match decode(&exit_ev("c", "c", 12, 137)) {
        Some(TaskEvent::Exit(x)) => {
            assert_eq!(x.exec_id, None, "id == container_id is the init process");
            assert_eq!((x.pid, x.exit_status), (12, 137));
            assert_eq!(x.exited_at, Some((1_700_000_000, 7)));
        }
        other => panic!("{other:?}"),
    }
    match decode(&exit_ev("c", "e1", 99, 0)) {
        Some(TaskEvent::Exit(x)) => assert_eq!(x.exec_id.as_deref(), Some("e1")),
        other => panic!("{other:?}"),
    }
    // Empty process id also means init.
    match decode(&exit_ev("c", "", 12, 1)) {
        Some(TaskEvent::Exit(x)) => assert_eq!(x.exec_id, None),
        other => panic!("{other:?}"),
    }
}

#[test]
fn unknown_type_preserved_and_malformed_skipped() {
    let unknown = ShimEvent {
        topic: "/tasks/paused".into(),
        namespace: "ns".into(),
        timestamp: None,
        type_url: "containerd.events.TaskPaused".into(),
        payload: vec![],
    };
    assert_eq!(
        decode(&unknown),
        Some(TaskEvent::Other {
            topic: "/tasks/paused".into(),
            type_url: "containerd.events.TaskPaused".into()
        })
    );
    let bad = ShimEvent {
        type_url: "containerd.events.TaskExit".into(),
        payload: vec![0xff, 0xff, 0xff],
        ..unknown
    };
    assert_eq!(
        decode(&bad),
        None,
        "malformed payload is skipped, not a panic"
    );
}

#[tokio::test]
async fn exit_recorded_before_waiter_resolves_immediately() {
    let t = ExitTracker::new();
    apply_raw(&t, &start_ev("c", 12));
    apply_raw(&t, &exit_ev("c", "c", 12, 5));
    let x = tokio::time::timeout(Duration::from_secs(2), t.wait_exit("c"))
        .await
        .expect("must not hang when the exit preceded the waiter");
    assert_eq!(x.exit_status, 5);
    assert_eq!(t.started_pid("c"), Some(12));
}

#[tokio::test]
async fn waiter_before_exit_is_woken() {
    let t = ExitTracker::new();
    let t2 = t.clone();
    let w = tokio::spawn(async move { t2.wait_exit("c").await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!w.is_finished());
    apply_raw(&t, &exit_ev("c", "c", 12, 9));
    let x = tokio::time::timeout(Duration::from_secs(2), w)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(x.exit_status, 9);
}

#[tokio::test]
async fn duplicate_exit_is_idempotent_and_not_rebroadcast() {
    let t = ExitTracker::new();
    let mut sub = t.subscribe();
    apply_raw(&t, &exit_ev("c", "c", 12, 1));
    apply_raw(&t, &exit_ev("c", "c", 12, 2));
    assert_eq!(sub.recv().await.unwrap().exit_status, 1);
    assert!(sub.try_recv().is_err(), "duplicate must not be broadcast");
    assert_eq!(t.exit_of("c").unwrap().exit_status, 1, "first exit wins");
}

#[tokio::test]
async fn exec_exit_tracked_separately_from_init() {
    let t = ExitTracker::new();
    apply_raw(&t, &exit_ev("c", "e1", 99, 3));
    assert!(t.exit_of("c").is_none(), "exec exit is not the init exit");
    assert_eq!(t.exec_exit_of("c", "e1").unwrap().exit_status, 3);
    apply_raw(&t, &exit_ev("c", "c", 12, 0));
    assert_eq!(t.exit_of("c").unwrap().exit_status, 0);
    assert_eq!(t.exec_exit_of("c", "e1").unwrap().exit_status, 3);
}

#[tokio::test]
async fn exit_for_unknown_id_retained_and_forget_clears() {
    let t = ExitTracker::new();
    apply_raw(&t, &exit_ev("never-created", "never-created", 1, 4));
    assert_eq!(t.exit_of("never-created").unwrap().exit_status, 4);
    t.forget("never-created");
    assert!(t.exit_of("never-created").is_none());

    // TaskDelete forgets too.
    apply_raw(&t, &exit_ev("c", "c", 1, 0));
    let mut d = evt::TaskDelete::new();
    d.container_id = "c".into();
    t.apply(
        decode(&ShimEvent {
            topic: "/tasks/delete".into(),
            namespace: "ns".into(),
            timestamp: None,
            type_url: "containerd.events.TaskDelete".into(),
            payload: d.write_to_bytes().unwrap(),
        })
        .unwrap(),
    );
    assert!(t.exit_of("c").is_none());
}

#[tokio::test]
async fn oldest_exited_entries_evicted_past_cap() {
    let t = ExitTracker::new();
    for i in 0..(MAX_ENTRIES + 10) {
        t.apply(TaskEvent::Exit(ContainerExit {
            container_id: format!("c{i}"),
            exec_id: None,
            pid: 1,
            exit_status: 0,
            exited_at: None,
        }));
    }
    assert!(t.exit_of("c0").is_none(), "oldest evicted");
    assert!(t.exit_of(&format!("c{}", MAX_ENTRIES + 9)).is_some());
    assert!(t.lock().entries.len() <= MAX_ENTRIES);
}

fn envelope_req(topic: &str, type_url: &str, payload: Vec<u8>) -> api::ForwardRequest {
    let mut env = Envelope::new();
    env.topic = topic.into();
    env.namespace = "k8s.io".into();
    let mut any = Any::new();
    any.type_url = type_url.into();
    any.value = payload;
    env.event = Some(any).into();
    let mut r = api::ForwardRequest::new();
    r.envelope = Some(env).into();
    r
}

/// Real sink + real ttrpc Events client + tracker dispatcher: a shim that
/// publishes Start then Exit resolves a waiter with the right status.
#[tokio::test]
async fn end_to_end_through_the_events_sink() {
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("ev/events.sock");
    let (mut sink, rx) = EventsSink::start(&sock).await.unwrap();
    let tracker = ExitTracker::spawn(rx);
    let mut sub = tracker.subscribe();

    let c = EventsClient::new(
        Client::connect(&format!("unix://{}", sock.display()))
            .await
            .unwrap(),
    );
    let fwd = |ev: ShimEvent| {
        let c = &c;
        async move {
            c.forward(
                context::with_timeout(5_000_000_000),
                &envelope_req(&ev.topic, &ev.type_url, ev.payload),
            )
            .await
            .unwrap();
        }
    };
    fwd(start_ev("ctr", 321)).await;
    fwd(exit_ev("ctr", "ctr", 321, 137)).await;
    // An undecodable event in the stream must not stall later ones.
    fwd(ShimEvent {
        topic: "/tasks/exit".into(),
        namespace: "k8s.io".into(),
        timestamp: None,
        type_url: "containerd.events.TaskExit".into(),
        payload: vec![0xff, 0xff],
    })
    .await;
    fwd(exit_ev("other", "other", 5, 0)).await;

    let x = tokio::time::timeout(Duration::from_secs(5), tracker.wait_exit("ctr"))
        .await
        .unwrap();
    assert_eq!((x.pid, x.exit_status), (321, 137));
    assert_eq!(tracker.started_pid("ctr"), Some(321));
    assert_eq!(sub.recv().await.unwrap().container_id, "ctr");
    assert_eq!(sub.recv().await.unwrap().container_id, "other");
    sink.shutdown().await;
}

struct WaitMock {
    reply: Result<u32, ()>,
}

#[async_trait::async_trait]
impl Task for WaitMock {
    async fn wait(
        &self,
        _: &TtrpcContext,
        _: api::WaitRequest,
    ) -> ttrpc::Result<api::WaitResponse> {
        match self.reply {
            Ok(status) => {
                let mut r = api::WaitResponse::new();
                r.exit_status = status;
                r.exited_at = stamp(42).into();
                Ok(r)
            }
            Err(()) => Err(ttrpc::Error::RpcStatus(ttrpc::get_status(
                ttrpc::Code::UNAVAILABLE,
                "shim going away",
            ))),
        }
    }
}

async fn mock_shim(
    dir: &std::path::Path,
    reply: Result<u32, ()>,
) -> (Server, crate::shim::client::ShimClient) {
    let sock = dir.join("shim.sock");
    let mut server = Server::new()
        .bind(&format!("unix://{}", sock.display()))
        .unwrap()
        .register_service(create_task(Arc::new(WaitMock { reply })));
    server.start().await.unwrap();
    let client = crate::shim::client::ShimClient::connect(&sock.to_string_lossy())
        .await
        .unwrap();
    (server, client)
}

/// The sink is silent; the `Wait` RPC alone resolves the waiter and the exit
/// is then visible to everyone else (state + broadcast).
#[tokio::test]
async fn fallback_wait_rpc_resolves_without_events() {
    let tmp = tempfile::tempdir().unwrap();
    let (_srv, client) = mock_shim(tmp.path(), Ok(7)).await;
    let t = ExitTracker::new();
    let mut sub = t.subscribe();
    let x = t
        .wait_exit_or_poll("c", &client, Duration::from_millis(200))
        .await
        .unwrap();
    assert_eq!(x.exit_status, 7);
    assert_eq!(x.exited_at, Some((42, 7)));
    assert_eq!(t.exit_of("c").unwrap().exit_status, 7);
    assert_eq!(sub.recv().await.unwrap().exit_status, 7);
}

/// Wait RPC fails and no event arrives: the waiter gets ShimLost after the
/// grace period instead of hanging.
#[tokio::test]
async fn dead_shim_returns_shim_lost() {
    let tmp = tempfile::tempdir().unwrap();
    let (_srv, client) = mock_shim(tmp.path(), Err(())).await;
    let t = ExitTracker::new();
    let r = tokio::time::timeout(
        Duration::from_secs(5),
        t.wait_exit_or_poll("c", &client, Duration::from_millis(150)),
    )
    .await
    .expect("must not hang");
    assert!(matches!(r, Err(WaitError::ShimLost(_))), "{r:?}");
}

/// Wait RPC fails but the exit event does arrive within the grace period:
/// the event wins.
#[tokio::test]
async fn failed_rpc_still_honours_late_event() {
    let tmp = tempfile::tempdir().unwrap();
    let (_srv, client) = mock_shim(tmp.path(), Err(())).await;
    let t = ExitTracker::new();
    let t2 = t.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        apply_raw(&t2, &exit_ev("c", "c", 5, 11));
    });
    let x = t
        .wait_exit_or_poll("c", &client, Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(x.exit_status, 11);
}
