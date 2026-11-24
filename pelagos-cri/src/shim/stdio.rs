//! Stdio for shim-backed tasks (epic #574, #597).
//!
//! The shim model hands a task three FIFOs (stdin/stdout/stderr) whose paths
//! travel in `CreateTaskRequest` / `ExecProcessRequest`. The shim opens its
//! ends; we own the other ends. This module provides:
//!
//! - [`Stdio`]: create/remove the FIFO set and fill the request fields.
//! - [`CriLogWriter`]: the kubelet's CRI log format
//!   (`<rfc3339nano> <stream> F|P <line>`), with partial-line handling.
//! - [`Pump`]: reads stdout/stderr FIFOs, feeds a [`Sink`] (CRI log file or an
//!   in-memory capture for ExecSync) and fans the bytes out on a broadcast
//!   channel so `Attach` can follow a running container (a FIFO has exactly
//!   one reader, so attach cannot read it directly).
//! - [`StdinWriter`]: shared writer for the stdin FIFO.
//!
//! Read ends are opened read-write: that holds a writer open so the pump
//! never sees EOF before the shim attaches (or after it detaches), and lets
//! the open succeed in any order. Termination is therefore explicit:
//! [`Pump::finish`] drains what is buffered and stops, called after the task
//! exits.

use std::ffi::CString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

/// Which output stream a chunk belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    pub fn as_str(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        }
    }
    fn idx(self) -> usize {
        match self {
            Stream::Stdout => 0,
            Stream::Stderr => 1,
        }
    }
}

/// The FIFO paths for one task or exec process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stdio {
    pub stdin: Option<PathBuf>,
    pub stdout: PathBuf,
    /// `None` for a terminal: a pty has a single merged output stream.
    pub stderr: Option<PathBuf>,
    pub terminal: bool,
}

fn mkfifo(path: &Path) -> io::Result<()> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in fifo path"))?;
    // SAFETY: c is a valid NUL-terminated path.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Stdio {
    /// Create `<dir>/<name>-{stdin,stdout,stderr}` FIFOs.
    pub fn create(dir: &Path, name: &str, stdin: bool, terminal: bool) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let mk = |suffix: &str| -> io::Result<PathBuf> {
            let p = dir.join(format!("{name}-{suffix}"));
            let _ = std::fs::remove_file(&p);
            mkfifo(&p)?;
            Ok(p)
        };
        let s = Stdio {
            stdin: if stdin { Some(mk("stdin")?) } else { None },
            stdout: mk("stdout")?,
            stderr: if terminal { None } else { Some(mk("stderr")?) },
            terminal,
        };
        Ok(s)
    }

    fn strings(&self) -> (String, String, String) {
        let f = |p: &Option<PathBuf>| {
            p.as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        (
            f(&self.stdin),
            self.stdout.to_string_lossy().into_owned(),
            f(&self.stderr),
        )
    }

    pub fn fill_create(&self, req: &mut containerd_shim_protos::api::CreateTaskRequest) {
        (req.stdin, req.stdout, req.stderr) = self.strings();
        req.terminal = self.terminal;
    }

    pub fn fill_exec(&self, req: &mut containerd_shim_protos::api::ExecProcessRequest) {
        (req.stdin, req.stdout, req.stderr) = self.strings();
        req.terminal = self.terminal;
    }

    /// Unlink the FIFOs (best effort).
    pub fn remove(&self) {
        for p in [
            Some(&self.stdout),
            self.stdin.as_ref(),
            self.stderr.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn cri_now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.9fZ")
        .to_string()
}

/// Longest unterminated line buffered before it is emitted as a `P` record.
pub const MAX_PARTIAL: usize = 16 * 1024;

/// Writes the CRI log format to a file, framing raw bytes into lines.
pub struct CriLogWriter {
    path: PathBuf,
    file: std::fs::File,
    partial: [Vec<u8>; 2],
}

impl CriLogWriter {
    /// Open (creating parents and the file up front so the kubelet can open it
    /// before the container prints anything) in append mode.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            partial: [Vec::new(), Vec::new()],
        })
    }

    /// Reopen the log path (creating it) after the kubelet rotated the file
    /// away (`ReopenContainerLog`). Partial lines carry over.
    pub fn reopen(&mut self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        Ok(())
    }

    fn record(out: &mut Vec<u8>, stream: Stream, tag: char, body: &[u8]) {
        out.extend_from_slice(format!("{} {} {} ", cri_now(), stream.as_str(), tag).as_bytes());
        out.extend_from_slice(body);
        out.push(b'\n');
    }

    pub fn write(&mut self, stream: Stream, data: &[u8]) -> io::Result<()> {
        let buf = &mut self.partial[stream.idx()];
        buf.extend_from_slice(data);
        let mut out = Vec::new();
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            Self::record(&mut out, stream, 'F', &line[..pos]);
        }
        while buf.len() >= MAX_PARTIAL {
            let chunk: Vec<u8> = buf.drain(..MAX_PARTIAL).collect();
            Self::record(&mut out, stream, 'P', &chunk);
        }
        if out.is_empty() {
            return Ok(());
        }
        self.file.write_all(&out)
    }

    /// Emit any unterminated trailing line as a complete one (as the native
    /// relay does when the container is gone).
    pub fn finish(&mut self) -> io::Result<()> {
        let mut out = Vec::new();
        for s in [Stream::Stdout, Stream::Stderr] {
            let rest = std::mem::take(&mut self.partial[s.idx()]);
            if !rest.is_empty() {
                Self::record(&mut out, s, 'F', &rest);
            }
        }
        if out.is_empty() {
            return Ok(());
        }
        self.file.write_all(&out)
    }
}

/// Where pumped bytes go.
pub enum Sink {
    /// Broadcast only.
    None,
    Log(CriLogWriter),
    /// In-memory capture (ExecSync); each stream is capped at `limit` bytes.
    Capture {
        limit: usize,
    },
}

/// Output captured by a [`Sink::Capture`] pump.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Captured {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

enum SinkState {
    None,
    Log(CriLogWriter),
    Capture { limit: usize, cap: Captured },
}

/// One chunk of output, tagged with its stream.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub stream: Stream,
    pub data: Bytes,
}

/// Broadcast capacity in chunks. A slow attach client lags (and is told so by
/// `RecvError::Lagged`) rather than back-pressuring the container's logs.
const BROADCAST_CAP: usize = 1024;

/// How long a drain waits for more buffered bytes before concluding the FIFO
/// is empty.
const DRAIN_IDLE: std::time::Duration = std::time::Duration::from_millis(25);

/// Reads a task's stdout/stderr FIFOs until [`Pump::finish`].
pub struct Pump {
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    sink: Arc<Mutex<SinkState>>,
    tx: broadcast::Sender<Chunk>,
}

impl Pump {
    /// Must be called inside a tokio runtime. Opens the read ends before
    /// returning, so it is safe to start the task right after.
    pub fn spawn(stdio: &Stdio, sink: Sink) -> io::Result<Self> {
        let state = match sink {
            Sink::None => SinkState::None,
            Sink::Log(w) => SinkState::Log(w),
            Sink::Capture { limit } => SinkState::Capture {
                limit,
                cap: Captured::default(),
            },
        };
        let sink = Arc::new(Mutex::new(state));
        let (tx, _) = broadcast::channel(BROADCAST_CAP);
        let (stop, stop_rx) = watch::channel(false);
        let open = |p: &Path| pipe::OpenOptions::new().read_write(true).open_receiver(p);
        let mut tasks = Vec::new();
        let mut streams = vec![(Stream::Stdout, open(&stdio.stdout)?)];
        if let Some(e) = &stdio.stderr {
            streams.push((Stream::Stderr, open(e)?));
        }
        for (stream, rx) in streams {
            tasks.push(tokio::spawn(pump_one(
                rx,
                stream,
                sink.clone(),
                tx.clone(),
                stop_rx.clone(),
            )));
        }
        Ok(Self {
            stop,
            tasks,
            sink,
            tx,
        })
    }

    /// Reopen a [`Sink::Log`] file (log rotation). No-op for other sinks.
    pub fn reopen_log(&self) -> io::Result<()> {
        match &mut *self.sink.lock().unwrap() {
            SinkState::Log(w) => w.reopen(),
            _ => Ok(()),
        }
    }

    /// Follow live output (attach). Only chunks pumped after this call are seen.
    pub fn subscribe(&self) -> broadcast::Receiver<Chunk> {
        self.tx.subscribe()
    }

    /// Drain buffered output, stop the readers, flush the sink. Subscribers see
    /// the channel close. Returns the capture for [`Sink::Capture`].
    pub async fn finish(self) -> Option<Captured> {
        let _ = self.stop.send(true);
        for t in self.tasks {
            let _ = t.await;
        }
        let mut g = self.sink.lock().unwrap();
        match &mut *g {
            SinkState::Log(w) => {
                if let Err(e) = w.finish() {
                    log::warn!("shim stdio: flushing CRI log: {e}");
                }
                None
            }
            SinkState::Capture { cap, .. } => Some(std::mem::take(cap)),
            SinkState::None => None,
        }
    }
}

fn deliver(stream: Stream, data: &[u8], sink: &Mutex<SinkState>, tx: &broadcast::Sender<Chunk>) {
    match &mut *sink.lock().unwrap() {
        SinkState::None => {}
        SinkState::Log(w) => {
            if let Err(e) = w.write(stream, data) {
                log::warn!("shim stdio: writing CRI log: {e}");
            }
        }
        SinkState::Capture { limit, cap } => {
            let dst = match stream {
                Stream::Stdout => &mut cap.stdout,
                Stream::Stderr => &mut cap.stderr,
            };
            let room = limit.saturating_sub(dst.len());
            if data.len() > room {
                cap.truncated = true;
            }
            let take = data.len().min(room);
            dst.extend_from_slice(&data[..take]);
        }
    }
    // No subscribers is the normal case; ignore the error.
    let _ = tx.send(Chunk {
        stream,
        data: Bytes::copy_from_slice(data),
    });
}

async fn pump_one(
    mut rx: pipe::Receiver,
    stream: Stream,
    sink: Arc<Mutex<SinkState>>,
    tx: broadcast::Sender<Chunk>,
    mut stop: watch::Receiver<bool>,
) {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        tokio::select! {
            r = rx.read(&mut buf) => match r {
                Ok(0) | Err(_) => break,
                Ok(n) => deliver(stream, &buf[..n], &sink, &tx),
            },
            _ = stop.changed() => {
                // try_read would miss bytes whose readiness the reactor has
                // not delivered yet; a short-deadline read waits for them.
                while let Ok(Ok(n)) = tokio::time::timeout(DRAIN_IDLE, rx.read(&mut buf)).await {
                    if n == 0 { break; }
                    deliver(stream, &buf[..n], &sink, &tx);
                }
                break;
            }
        }
    }
}

/// Shared writer for a task's stdin FIFO.
#[derive(Clone)]
pub struct StdinWriter {
    inner: Arc<tokio::sync::Mutex<Option<pipe::Sender>>>,
}

impl StdinWriter {
    /// Open read-write so it succeeds before the shim has opened its end.
    pub fn open(path: &Path) -> io::Result<Self> {
        let tx = pipe::OpenOptions::new()
            .read_write(true)
            .open_sender(path)?;
        Ok(Self {
            inner: Arc::new(tokio::sync::Mutex::new(Some(tx))),
        })
    }

    pub async fn write_all(&self, data: &[u8]) -> io::Result<()> {
        match self.inner.lock().await.as_mut() {
            Some(tx) => tx.write_all(data).await,
            None => Err(io::Error::from(io::ErrorKind::BrokenPipe)),
        }
    }

    /// Close our end; the shim's reader then sees EOF.
    pub async fn close(&self) {
        self.inner.lock().await.take();
    }
}

#[cfg(test)]
mod tests;
