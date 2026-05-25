//! Async subprocess helpers for invoking the pelagos CLI.

use tokio::process::Command;

pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

pub async fn run_pelagos(bin: &str, args: &[&str]) -> std::io::Result<CmdOutput> {
    let out = Command::new(bin).args(args).output().await?;
    Ok(CmdOutput {
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        success: out.status.success(),
    })
}

/// Run a pelagos command and return stdout on success, Err on non-zero exit.
#[allow(dead_code)]
pub async fn run_pelagos_capture(bin: &str, args: &[&str]) -> std::io::Result<String> {
    let out = Command::new(bin).args(args).output().await?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(std::io::Error::other(msg))
    }
}
