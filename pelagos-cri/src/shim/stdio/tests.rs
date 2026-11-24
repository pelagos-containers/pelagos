use std::io::Write;
use std::os::unix::fs::FileTypeExt;
use std::time::Duration;

use super::*;

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// `<ts> <stream> <tag> <body>` -> (stream, tag, body)
fn parse(l: &str) -> (&str, &str, &str) {
    let mut it = l.splitn(4, ' ');
    let ts = it.next().unwrap();
    assert!(ts.ends_with('Z') && ts.contains('T'), "timestamp: {ts}");
    (
        it.next().unwrap(),
        it.next().unwrap(),
        it.next().unwrap_or(""),
    )
}

#[test]
fn log_writer_frames_lines_and_joins_partials() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("deep/dir/ctr.log");
    let mut w = CriLogWriter::open(&p).unwrap();
    assert!(p.exists(), "file exists before any output");
    w.write(Stream::Stdout, b"hello\nwor").unwrap();
    w.write(Stream::Stderr, b"oops\n").unwrap();
    w.write(Stream::Stdout, b"ld\n\ntail").unwrap();
    // The unterminated "tail" is held back until finish.
    let l = lines(&p);
    assert_eq!(l.len(), 4, "{l:?}");
    assert_eq!(parse(&l[0]), ("stdout", "F", "hello"));
    assert_eq!(parse(&l[1]), ("stderr", "F", "oops"));
    assert_eq!(parse(&l[2]), ("stdout", "F", "world"));
    assert_eq!(parse(&l[3]), ("stdout", "F", ""), "empty line preserved");
    w.finish().unwrap();
    let l = lines(&p);
    assert_eq!(parse(&l[4]), ("stdout", "F", "tail"));
    w.finish().unwrap();
    assert_eq!(lines(&p).len(), 5, "finish is idempotent");
}

#[test]
fn log_writer_splits_overlong_lines_as_partial_records() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("c.log");
    let mut w = CriLogWriter::open(&p).unwrap();
    let big = vec![b'x'; MAX_PARTIAL * 2 + 10];
    w.write(Stream::Stdout, &big).unwrap();
    w.write(Stream::Stdout, b"\n").unwrap();
    let l = lines(&p);
    let tags: Vec<_> = l.iter().map(|x| parse(x).1).collect();
    assert_eq!(tags, ["P", "P", "F"], "two P records then the F terminator");
    assert_eq!(parse(&l[2]).2.len(), 10);
}

#[test]
fn log_writer_appends_across_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("c.log");
    CriLogWriter::open(&p)
        .unwrap()
        .write(Stream::Stdout, b"a\n")
        .unwrap();
    CriLogWriter::open(&p)
        .unwrap()
        .write(Stream::Stdout, b"b\n")
        .unwrap();
    assert_eq!(lines(&p).len(), 2, "re-adoption must not truncate the log");
}

#[test]
fn stdio_create_fills_requests_and_cleans_up() {
    let tmp = tempfile::tempdir().unwrap();
    let s = Stdio::create(tmp.path(), "c1", true, false).unwrap();
    for p in [
        &s.stdout,
        s.stdin.as_ref().unwrap(),
        s.stderr.as_ref().unwrap(),
    ] {
        assert!(std::fs::metadata(p).unwrap().file_type().is_fifo(), "{p:?}");
    }
    let mut req = containerd_shim_protos::api::CreateTaskRequest::new();
    s.fill_create(&mut req);
    assert_eq!(req.stdout, s.stdout.to_string_lossy());
    assert!(!req.stdin.is_empty() && !req.stderr.is_empty() && !req.terminal);

    let t = Stdio::create(tmp.path(), "c2", false, true).unwrap();
    let mut ex = containerd_shim_protos::api::ExecProcessRequest::new();
    t.fill_exec(&mut ex);
    assert!(ex.terminal && ex.stdin.is_empty() && ex.stderr.is_empty());
    assert!(!ex.stdout.is_empty());

    s.remove();
    assert!(!s.stdout.exists() && !s.stdin.as_ref().unwrap().exists());
    // Recreating over stale FIFOs (crash leftovers) works.
    std::fs::write(tmp.path().join("c3-stdout"), b"stale").unwrap();
    Stdio::create(tmp.path(), "c3", false, true).unwrap();
}

fn write_fifo(p: &Path, data: &[u8]) {
    // Opening for write succeeds because the pump holds the read side open.
    let mut f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
    f.write_all(data).unwrap();
}

#[tokio::test]
async fn pump_writes_cri_log_and_fans_out_to_attach() {
    let tmp = tempfile::tempdir().unwrap();
    let s = Stdio::create(tmp.path(), "c", false, false).unwrap();
    let log = tmp.path().join("pod/ctr_0.log");
    let pump = Pump::spawn(&s, Sink::Log(CriLogWriter::open(&log).unwrap())).unwrap();
    let mut attach = pump.subscribe();

    write_fifo(&s.stdout, b"out1\nout2");
    write_fifo(s.stderr.as_ref().unwrap(), b"err1\n");

    // Attach sees live bytes.
    let mut seen = Vec::new();
    while seen.len() < 2 {
        let c = tokio::time::timeout(Duration::from_secs(5), attach.recv())
            .await
            .expect("attach chunk")
            .unwrap();
        seen.push((c.stream, c.data));
    }
    assert!(seen
        .iter()
        .any(|(s, d)| *s == Stream::Stderr && &d[..] == b"err1\n"));

    // Bytes written right before finish() must not be lost.
    write_fifo(&s.stdout, b"\nlast");
    assert!(pump.finish().await.is_none());
    let l = lines(&log);
    let got: Vec<_> = l.iter().map(|x| parse(x)).collect();
    assert!(got.contains(&("stdout", "F", "out1")), "{l:?}");
    assert!(got.contains(&("stdout", "F", "out2")), "{l:?}");
    assert!(got.contains(&("stderr", "F", "err1")), "{l:?}");
    assert!(
        got.contains(&("stdout", "F", "last")),
        "trailing partial flushed on finish: {l:?}"
    );
    // After finish the broadcast closes instead of hanging attach clients.
    loop {
        match tokio::time::timeout(Duration::from_secs(5), attach.recv()).await {
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Ok(_) => continue,
            Err(_) => panic!("attach receiver never closed"),
        }
    }
}

#[tokio::test]
async fn pump_capture_is_bounded_and_per_stream() {
    let tmp = tempfile::tempdir().unwrap();
    let s = Stdio::create(tmp.path(), "c", false, false).unwrap();
    let pump = Pump::spawn(&s, Sink::Capture { limit: 8 }).unwrap();
    write_fifo(&s.stdout, b"0123456789");
    write_fifo(s.stderr.as_ref().unwrap(), b"ab");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let cap = pump.finish().await.unwrap();
    assert_eq!(cap.stdout, b"01234567");
    assert_eq!(cap.stderr, b"ab");
    assert!(cap.truncated);
}

#[tokio::test]
async fn terminal_pump_has_single_stream() {
    let tmp = tempfile::tempdir().unwrap();
    let s = Stdio::create(tmp.path(), "c", true, true).unwrap();
    let pump = Pump::spawn(&s, Sink::Capture { limit: 64 }).unwrap();
    write_fifo(&s.stdout, b"pty out");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let cap = pump.finish().await.unwrap();
    assert_eq!(cap.stdout, b"pty out");
    assert!(cap.stderr.is_empty());
}

#[tokio::test]
async fn stdin_writer_reaches_reader_and_close_gives_eof() {
    let tmp = tempfile::tempdir().unwrap();
    let s = Stdio::create(tmp.path(), "c", true, false).unwrap();
    let path = s.stdin.clone().unwrap();
    // Writer first (as in a real run the CRI holds its end before the shim
    // reads); then the "shim" reader opens, which no longer blocks. Data
    // written with no reader open would be lost, so wait for it.
    let w = StdinWriter::open(s.stdin.as_ref().unwrap()).unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut f = std::fs::File::open(&path).unwrap();
        ready_tx.send(()).unwrap();
        let mut v = Vec::new();
        f.read_to_end(&mut v).unwrap(); // returns only at EOF
        v
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let w2 = w.clone();
    w.write_all(b"abc").await.unwrap();
    w2.write_all(b"def").await.unwrap();
    w.close().await;
    let got = tokio::task::spawn_blocking(move || reader.join().unwrap())
        .await
        .unwrap();
    assert_eq!(got, b"abcdef", "all writes delivered, then EOF after close");
    assert!(
        w2.write_all(b"x").await.is_err(),
        "closed is closed for clones"
    );
}

#[test]
fn log_writer_reopen_follows_a_rotated_file() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("c.log");
    let mut w = CriLogWriter::open(&p).unwrap();
    w.write(Stream::Stdout, b"before\nhal").unwrap();
    // kubelet rotates: rename away, then asks us to reopen.
    let rotated = tmp.path().join("c.log.20260101");
    std::fs::rename(&p, &rotated).unwrap();
    w.write(Stream::Stdout, b"f\n").unwrap();
    assert_eq!(
        lines(&rotated).len(),
        2,
        "still writing to the renamed inode"
    );
    w.reopen().unwrap();
    assert!(p.exists(), "fresh file at the original path");
    w.write(Stream::Stdout, b"after\n").unwrap();
    let l = lines(&p);
    assert_eq!(l.len(), 1);
    assert_eq!(parse(&l[0]), ("stdout", "F", "after"));
}
