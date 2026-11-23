//! Task lifecycle tracking from shim events (#587).
//!
//! The [`events`](super::events) sink delivers raw [`ShimEvent`]s. This module
//! decodes them into typed [`TaskEvent`]s and folds them into per-container
//! state in an [`ExitTracker`], which answers "did container X exit, with what
//! status, when?" for the rest of the CRI.
//!
//! Properties the tracker guarantees (containerd's events contract makes
//! `TaskStart` precede `TaskExit`, but delivery to a late consumer is not
//! ordered with respect to *waiters*):
//!
//! - **No lost wakeups**: [`ExitTracker::wait_exit`] resolves even when the
//!   exit was recorded before the waiter registered.
//! - **Idempotent**: the first exit for a process wins; duplicates (an event
//!   and the `Wait` RPC fallback both reporting it) are ignored and not
//!   re-broadcast.
//! - **Exec vs init**: `TaskExit.id` names the exited *process*. Equal to the
//!   container id means the init process; anything else is an exec.
//! - **Unknown ids are kept**: an exit for a container we never saw `Create`
//!   for (e.g. after a CRI restart) is still recorded.
//! - **Dead shim cannot hang a waiter**: [`ExitTracker::wait_exit_or_poll`]
//!   also issues the `Wait` RPC and gives up with [`WaitError::ShimLost`] if
//!   the shim is gone and no event arrives within a grace period.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use containerd_shim_protos::api;
use containerd_shim_protos::events::task as evt;
use containerd_shim_protos::protobuf::well_known_types::timestamp::Timestamp as WktTimestamp;
use containerd_shim_protos::protobuf::{Message, MessageField};
use tokio::sync::{broadcast, Notify};

use super::client::ShimClient;
use super::events::{EventReceiver, ShimEvent};

/// Event time as `(seconds, nanos)` since the Unix epoch.
pub type Timestamp = (i64, i32);

/// A task lifecycle event decoded from a [`ShimEvent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEvent {
    Create {
        container_id: String,
        pid: u32,
    },
    Start {
        container_id: String,
        pid: u32,
    },
    Exit(ContainerExit),
    Delete {
        container_id: String,
        pid: u32,
        exit_status: u32,
        exited_at: Option<Timestamp>,
    },
    ExecAdded {
        container_id: String,
        exec_id: String,
    },
    ExecStarted {
        container_id: String,
        exec_id: String,
        pid: u32,
    },
    Oom {
        container_id: String,
    },
    /// A well-formed event of a type we don't act on (paused, resumed,
    /// checkpointed, or something newer). Preserved, not dropped.
    Other {
        topic: String,
        type_url: String,
    },
}

/// A process exit: the init process (`exec_id == None`) or an exec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerExit {
    pub container_id: String,
    /// `None` for the container's init process.
    pub exec_id: Option<String>,
    pub pid: u32,
    pub exit_status: u32,
    pub exited_at: Option<Timestamp>,
}

fn ts(t: &MessageField<WktTimestamp>) -> Option<Timestamp> {
    t.as_ref().map(|t| (t.seconds, t.nanos))
}

/// Decode a [`ShimEvent`]. `None` means the payload was malformed for its
/// declared type (logged); unknown types decode to [`TaskEvent::Other`].
pub fn decode(ev: &ShimEvent) -> Option<TaskEvent> {
    // type_url is e.g. `containerd.events.TaskExit`, possibly prefixed with a
    // host (`types.containerd.io/...`); only the final segment identifies it.
    let name = ev
        .type_url
        .rsplit(['.', '/'])
        .next()
        .unwrap_or(ev.type_url.as_str());
    macro_rules! parse {
        ($ty:ty) => {
            match <$ty>::parse_from_bytes(&ev.payload) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("malformed {} payload on {}: {}", name, ev.topic, e);
                    return None;
                }
            }
        };
    }
    Some(match name {
        "TaskCreate" => {
            let m = parse!(evt::TaskCreate);
            TaskEvent::Create {
                container_id: m.container_id,
                pid: m.pid,
            }
        }
        "TaskStart" => {
            let m = parse!(evt::TaskStart);
            TaskEvent::Start {
                container_id: m.container_id,
                pid: m.pid,
            }
        }
        "TaskExit" => {
            let m = parse!(evt::TaskExit);
            let exec_id = if m.id.is_empty() || m.id == m.container_id {
                None
            } else {
                Some(m.id)
            };
            TaskEvent::Exit(ContainerExit {
                container_id: m.container_id,
                exec_id,
                pid: m.pid,
                exit_status: m.exit_status,
                exited_at: ts(&m.exited_at),
            })
        }
        "TaskDelete" => {
            let m = parse!(evt::TaskDelete);
            TaskEvent::Delete {
                container_id: m.container_id,
                pid: m.pid,
                exit_status: m.exit_status,
                exited_at: ts(&m.exited_at),
            }
        }
        "TaskExecAdded" => {
            let m = parse!(evt::TaskExecAdded);
            TaskEvent::ExecAdded {
                container_id: m.container_id,
                exec_id: m.exec_id,
            }
        }
        "TaskExecStarted" => {
            let m = parse!(evt::TaskExecStarted);
            TaskEvent::ExecStarted {
                container_id: m.container_id,
                exec_id: m.exec_id,
                pid: m.pid,
            }
        }
        "TaskOOM" => {
            let m = parse!(evt::TaskOOM);
            TaskEvent::Oom {
                container_id: m.container_id,
            }
        }
        _ => TaskEvent::Other {
            topic: ev.topic.clone(),
            type_url: ev.type_url.clone(),
        },
    })
}

/// Why a wait did not produce an exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitError {
    /// The shim's `Wait` RPC failed (shim gone) and no exit event arrived
    /// within the grace period.
    ShimLost(String),
}

impl std::fmt::Display for WaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaitError::ShimLost(e) => write!(f, "shim lost before task exit: {e}"),
        }
    }
}

impl std::error::Error for WaitError {}

/// Cap on retained entries; beyond it the oldest *exited* entries are evicted
/// so exits for ids nobody ever forgets cannot grow without bound.
const MAX_ENTRIES: usize = 4096;
/// Broadcast backlog per subscriber before it starts lagging.
const BROADCAST_CAPACITY: usize = 256;

#[derive(Default)]
struct Entry {
    seq: u64,
    pid: Option<u32>,
    started: bool,
    oom: bool,
    exit: Option<ContainerExit>,
    execs: HashMap<String, ContainerExit>,
}

#[derive(Default)]
struct State {
    next_seq: u64,
    entries: HashMap<String, Entry>,
}

struct Inner {
    state: Mutex<State>,
    notify: Notify,
    tx: broadcast::Sender<ContainerExit>,
}

/// Folds shim events into per-container state. Cheap to clone.
#[derive(Clone)]
pub struct ExitTracker {
    inner: Arc<Inner>,
}

impl Default for ExitTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ExitTracker {
    /// A tracker with no event source; feed it via [`Self::apply`].
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                notify: Notify::new(),
                tx,
            }),
        }
    }

    /// A tracker fed by a dispatcher task draining `rx` (the receiver from
    /// [`EventsSink::start`](super::events::EventsSink::start)). The task
    /// ends when the sink's channel closes.
    pub fn spawn(mut rx: EventReceiver) -> Self {
        let t = Self::new();
        let me = t.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                match decode(&ev) {
                    Some(te) => me.apply(te),
                    None => log::debug!("dropped undecodable shim event {}", ev.topic),
                }
            }
            log::debug!("shim event channel closed; exit tracker dispatcher done");
        });
        t
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Fold one decoded event into the tracked state.
    pub fn apply(&self, ev: TaskEvent) {
        match ev {
            TaskEvent::Create { container_id, pid } => {
                let mut st = self.lock();
                let e = entry(&mut st, &container_id);
                e.pid = Some(pid);
            }
            TaskEvent::Start { container_id, pid } => {
                let mut st = self.lock();
                let e = entry(&mut st, &container_id);
                e.pid = Some(pid);
                e.started = true;
            }
            TaskEvent::Oom { container_id } => {
                entry(&mut self.lock(), &container_id).oom = true;
            }
            TaskEvent::Exit(x) => {
                self.record_exit(x);
            }
            TaskEvent::Delete { container_id, .. } => self.forget(&container_id),
            TaskEvent::ExecAdded { .. }
            | TaskEvent::ExecStarted { .. }
            | TaskEvent::Other { .. } => {}
        }
    }

    /// Record an exit. Returns `true` if it was new (and so broadcast).
    fn record_exit(&self, x: ContainerExit) -> bool {
        let fresh = {
            let mut st = self.lock();
            let e = entry(&mut st, &x.container_id);
            let slot = match &x.exec_id {
                None => {
                    if e.exit.is_some() {
                        false
                    } else {
                        e.exit = Some(x.clone());
                        true
                    }
                }
                Some(id) => {
                    if e.execs.contains_key(id) {
                        false
                    } else {
                        e.execs.insert(id.clone(), x.clone());
                        true
                    }
                }
            };
            if slot {
                evict(&mut st);
            }
            slot
        };
        if fresh {
            // No subscribers is fine.
            let _ = self.inner.tx.send(x);
            self.inner.notify.notify_waiters();
        } else {
            log::debug!("duplicate exit for {} ignored", x.container_id);
        }
        fresh
    }

    /// Drop all state for a container (call on `TaskDelete` / removal).
    pub fn forget(&self, container_id: &str) {
        self.lock().entries.remove(container_id);
    }

    /// The recorded init-process exit, if any.
    pub fn exit_of(&self, container_id: &str) -> Option<ContainerExit> {
        self.lock()
            .entries
            .get(container_id)
            .and_then(|e| e.exit.clone())
    }

    /// The recorded exit of one exec process, if any.
    pub fn exec_exit_of(&self, container_id: &str, exec_id: &str) -> Option<ContainerExit> {
        self.lock()
            .entries
            .get(container_id)
            .and_then(|e| e.execs.get(exec_id).cloned())
    }

    /// Whether `TaskStart` was seen and the pid it reported.
    pub fn started_pid(&self, container_id: &str) -> Option<u32> {
        self.lock()
            .entries
            .get(container_id)
            .filter(|e| e.started)
            .and_then(|e| e.pid)
    }

    /// Whether a `TaskOOM` was seen for the container.
    pub fn oom_seen(&self, container_id: &str) -> bool {
        self.lock()
            .entries
            .get(container_id)
            .map(|e| e.oom)
            .unwrap_or(false)
    }

    /// Subscribe to new exits (init and exec) as they are recorded.
    pub fn subscribe(&self) -> broadcast::Receiver<ContainerExit> {
        self.inner.tx.subscribe()
    }

    /// Resolve with the container's init-process exit, immediately if it was
    /// already recorded.
    pub async fn wait_exit(&self, container_id: &str) -> ContainerExit {
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            // Register before checking so an exit recorded in between wakes us.
            notified.as_mut().enable();
            if let Some(x) = self.exit_of(container_id) {
                return x;
            }
            notified.await;
        }
    }

    /// Like [`Self::wait_exit`], but also asks the shim via the `Wait` RPC so
    /// a shim that never publishes (or died) cannot hang the caller. If the
    /// RPC fails, an exit event still has `grace` to arrive before this
    /// returns [`WaitError::ShimLost`].
    pub async fn wait_exit_or_poll(
        &self,
        container_id: &str,
        client: &ShimClient,
        grace: Duration,
    ) -> Result<ContainerExit, WaitError> {
        enum First {
            Event(ContainerExit),
            Rpc(Result<api::WaitResponse, containerd_shim_protos::ttrpc::Error>),
        }
        let events = self.wait_exit(container_id);
        let rpc = client.wait(container_id);
        tokio::pin!(events);
        let first = tokio::select! {
            x = &mut events => First::Event(x),
            r = rpc => First::Rpc(r),
        };
        match first {
            First::Event(x) => Ok(x),
            First::Rpc(Ok(resp)) => {
                // Prefer an event that raced us so pid/timestamp match.
                if let Some(x) = self.exit_of(container_id) {
                    return Ok(x);
                }
                let pid = self
                    .lock()
                    .entries
                    .get(container_id)
                    .and_then(|e| e.pid)
                    .unwrap_or(0);
                self.record_exit(ContainerExit {
                    container_id: container_id.to_string(),
                    exec_id: None,
                    pid,
                    exit_status: resp.exit_status,
                    exited_at: ts(&resp.exited_at),
                });
                Ok(self.exit_of(container_id).expect("exit just recorded"))
            }
            First::Rpc(Err(e)) => match tokio::time::timeout(grace, &mut events).await {
                Ok(x) => Ok(x),
                Err(_) => Err(WaitError::ShimLost(e.to_string())),
            },
        }
    }
}

fn entry<'a>(st: &'a mut State, id: &str) -> &'a mut Entry {
    if !st.entries.contains_key(id) {
        st.next_seq += 1;
        let seq = st.next_seq;
        st.entries.insert(
            id.to_string(),
            Entry {
                seq,
                ..Entry::default()
            },
        );
    }
    st.entries.get_mut(id).expect("just inserted")
}

fn evict(st: &mut State) {
    while st.entries.len() > MAX_ENTRIES {
        let victim = st
            .entries
            .iter()
            .filter(|(_, e)| e.exit.is_some())
            .min_by_key(|(_, e)| e.seq)
            .map(|(k, _)| k.clone());
        match victim {
            Some(k) => {
                st.entries.remove(&k);
            }
            None => break,
        }
    }
}

#[cfg(test)]
mod tests;
