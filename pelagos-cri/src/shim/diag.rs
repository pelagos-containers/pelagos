//! Diagnostics for a sandbox shim that keeps rejecting container `Create` (#659).
//!
//! A Kata guest agent answers a failed create with a bare `No such file or directory` wrapped
//! in several levels of escaped error text and a stack backtrace: the CRI log was one
//! unreadable line, repeated 2,242 times, and named neither the failing path nor the cause.
//! This module (1) boils such an error down to its cause, (2) checks every host bind-mount
//! source of the container against the host and names the missing ones, and (3) writes a
//! bounded evidence bundle before anything is recycled, so the cause is not destroyed by the
//! recovery (#660).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Consecutive Create failures on one sandbox at which the condition is escalated (logged at
/// `error` level with a diagnostics bundle): 3, 10, 30, then every 100.
pub fn is_escalation_point(n: u32) -> bool {
    matches!(n, 3 | 10 | 30) || (n >= 100 && n % 100 == 0)
}

/// One step of a cause chain: without the `Others(` wrapper the shim's error type leaves
/// behind, and cut to a readable length (the full text is in the bundle).
fn tidy(step: &str) -> String {
    let s = step
        .trim()
        .trim_end_matches("Others(")
        .trim_end_matches([':', ' ']);
    if s.chars().count() > 90 {
        format!("{}...", s.chars().take(90).collect::<String>())
    } else {
        s.to_string()
    }
}

/// The cause of a shim error, without the backtrace or the escaping. Best effort: the full
/// text is still written to the bundle.
pub fn root_cause(err: &str) -> String {
    // Keep the "Caused by" chain headings (e.g. "agent create container") ...
    let mut chain: Vec<String> = Vec::new();
    // Only before the first backtrace: its frames look like "11: __libc_start_main".
    let head = &err[..err.find("Stack backtrace").unwrap_or(err.len())];
    for line in head.split("\\n").flat_map(|l| l.split('\n')) {
        let l = line.trim_matches(|c: char| c == '\\' || c.is_whitespace());
        // "0: agent create container"
        if let Some((idx, rest)) = l.split_once(": ") {
            if idx.chars().all(|c| c.is_ascii_digit())
                && !idx.is_empty()
                && !rest.starts_with("rpc status")
                && !rest.contains("<unknown>")
                && !rest.contains("::")
            {
                chain.push(tidy(rest));
            }
        }
    }
    // ... and the innermost `message: "..."` (the guest's own words).
    let mut inner = String::new();
    if let Some(pos) = err.rfind("message: ") {
        let tail = &err[pos + "message: ".len()..];
        let tail = tail.trim_start_matches(['\\', '"']);
        let end = ["Stack backtrace", "\\n", "\n", "\\\"", "\"", ", details"]
            .iter()
            .filter_map(|m| tail.find(m))
            .min()
            .unwrap_or(tail.len().min(200));
        inner = tidy(tail[..end].trim_end_matches(['\\', '"']));
    }
    match (chain.is_empty(), inner.is_empty()) {
        (false, false) => format!("{}: {}", chain.join(" > "), inner),
        (true, false) => inner,
        (false, true) => chain.join(" > "),
        (true, true) => err.chars().take(300).collect(),
    }
}

/// Bind-mount sources of an OCI `config.json` that do not exist on the host, as
/// `"<destination> <- <source>"`. `exists` is injected so the logic is testable.
pub fn missing_mount_sources(config_json: &[u8], exists: impl Fn(&Path) -> bool) -> Vec<String> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(config_json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for m in v["mounts"].as_array().into_iter().flatten() {
        let is_bind = m["type"].as_str() == Some("bind")
            || m["options"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|o| matches!(o.as_str(), Some("bind") | Some("rbind")));
        if !is_bind {
            continue;
        }
        if let (Some(dest), Some(src)) = (m["destination"].as_str(), m["source"].as_str()) {
            if !exists(Path::new(src)) {
                out.push(format!("{dest} <- {src}"));
            }
        }
    }
    out
}

/// A depth- and size-limited recursive listing (`ls -laR` without the shell).
fn listing(root: &Path, max_depth: usize, max_entries: usize) -> String {
    fn walk(dir: &Path, depth: usize, max_depth: usize, left: &mut usize, out: &mut String) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            let _ = writeln!(out, "{}: unreadable", dir.display());
            return;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if *left == 0 {
                let _ = writeln!(out, "... truncated");
                return;
            }
            *left -= 1;
            let p = e.path();
            let md = std::fs::symlink_metadata(&p);
            let (kind, size) = match &md {
                Ok(m) if m.file_type().is_symlink() => (
                    "l",
                    std::fs::read_link(&p)
                        .map(|t| t.display().to_string())
                        .unwrap_or_default(),
                ),
                Ok(m) if m.is_dir() => ("d", String::new()),
                Ok(m) => ("-", m.len().to_string()),
                Err(_) => ("?", String::new()),
            };
            let _ = writeln!(out, "{kind} {} {size}", p.display());
            if kind == "d" && depth < max_depth {
                walk(&p, depth + 1, max_depth, left, out);
            }
        }
    }
    let mut out = String::new();
    let mut left = max_entries;
    if !root.exists() {
        let _ = writeln!(out, "{}: does not exist", root.display());
    } else {
        walk(root, 1, max_depth, &mut left, &mut out);
    }
    out
}

/// Last lines of the journal that mention `needle` (the shim logs under identifier `kata`).
async fn journal_tail(needle: &str) -> String {
    let run = tokio::process::Command::new("journalctl")
        .args(["-b", "-n", "3000", "--no-pager", "-o", "short-iso"])
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(10), run).await {
        Ok(Ok(o)) => {
            let text = String::from_utf8_lossy(&o.stdout);
            let lines: Vec<&str> = text.lines().filter(|l| l.contains(needle)).collect();
            let from = lines.len().saturating_sub(300);
            lines[from..].join("\n")
        }
        Ok(Err(e)) => format!("journalctl failed: {e}"),
        Err(_) => "journalctl timed out".to_string(),
    }
}

/// Write an evidence bundle for repeated Create failures. Returns its directory. At most
/// `KEEP` bundles are kept per sandbox. Never fails the caller: errors are logged.
pub async fn capture(
    diag_root: &Path,
    sandbox_id: &str,
    container_id: &str,
    bundle_dir: &Path,
    full_error: &str,
    failures: u32,
) -> Option<PathBuf> {
    const KEEP: usize = 5;
    let sb_dir = diag_root.join(sandbox_id);
    let dir = sb_dir.join(format!("{:05}-{container_id:.12}", failures));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!(
            "sandbox {sandbox_id}: diagnostics dir {}: {e}",
            dir.display()
        );
        return None;
    }
    let config = std::fs::read(bundle_dir.join("config.json")).unwrap_or_default();
    let missing = missing_mount_sources(&config, |p| p.exists());
    let mut summary = format!(
        "sandbox {sandbox_id}\ncontainer {container_id}\nconsecutive Create failures: {failures}\n\
         cause: {}\n\nbind-mount sources missing on the host:\n",
        root_cause(full_error)
    );
    if missing.is_empty() {
        summary.push_str("  (none)\n");
    }
    for m in &missing {
        let _ = writeln!(summary, "  {m}");
    }
    let id_short: String = sandbox_id.chars().take(12).collect();
    let files: Vec<(&str, String)> = vec![
        ("summary.txt", summary),
        ("error.txt", full_error.to_string()),
        ("config.json", String::from_utf8_lossy(&config).into_owned()),
        (
            "shared-sandbox.txt",
            listing(
                &Path::new("/run/kata-containers/shared/sandboxes").join(sandbox_id),
                4,
                2000,
            ),
        ),
        (
            "shared-containers.txt",
            listing(Path::new("/run/kata-containers/shared/containers"), 3, 2000),
        ),
        (
            "mounts.txt",
            std::fs::read_to_string("/proc/self/mountinfo")
                .unwrap_or_default()
                .lines()
                .filter(|l| l.contains(sandbox_id) || l.contains(&id_short))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        ("shim-journal.txt", journal_tail(sandbox_id).await),
    ];
    for (name, body) in files {
        if let Err(e) = std::fs::write(dir.join(name), body) {
            log::warn!(
                "sandbox {sandbox_id}: writing {}/{name}: {e}",
                dir.display()
            );
        }
    }
    // Keep the newest KEEP bundles.
    if let Ok(rd) = std::fs::read_dir(&sb_dir) {
        let mut all: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        all.sort();
        for old in all.iter().take(all.len().saturating_sub(KEEP)) {
            let _ = std::fs::remove_dir_all(old);
        }
    }
    Some(dir)
}

#[cfg(test)]
mod tests;
