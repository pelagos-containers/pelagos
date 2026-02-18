//! Two-container network diagnostic: nc server + nc client.
//!
//! Proves bridge networking and cross-container TCP work.
//!
//! ```bash
//! sudo -E cargo run --example net_debug
//! ```

use remora::container::{Command, Namespace, Stdio};
use remora::network::NetworkMode;
use std::env;

const ALPINE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn main() {
    env_logger::init();

    let rootfs = env::current_dir().unwrap().join("alpine-rootfs");
    if !rootfs.exists() {
        eprintln!("alpine-rootfs not found");
        std::process::exit(1);
    }

    println!("=== Net Debug: nc server + client ===\n");

    // --- Server: nc listening on port 8080, serves a one-line response ---
    let server_script = r#"
echo "[server] listening on :8080"
while true; do
    echo "HELLO FROM SERVER" | nc -l -p 8080 2>/dev/null
done
"#;

    println!("[main] Spawning server (nc -l -p 8080) ...");
    let mut server = Command::new("/bin/sh")
        .args(&["-c", server_script])
        .stdin(Stdio::Null)
        .stdout(Stdio::Null)
        .stderr(Stdio::Null)
        .with_chroot(&rootfs)
        .with_namespaces(Namespace::UTS | Namespace::MOUNT)
        .env("PATH", ALPINE_PATH)
        .with_proc_mount()
        .with_network(NetworkMode::Bridge)
        .spawn()
        .expect("Failed to spawn server");

    let ip_a = server.container_ip().expect("server has no bridge IP");
    let pid_a = server.pid();
    println!("[main] server: PID={} IP={}", pid_a, ip_a);

    std::thread::sleep(std::time::Duration::from_millis(500));

    // --- Client: ping + nc connect ---
    let client_script = format!(
        r#"
echo "=== ping {ip} ==="
ping -c1 -W2 {ip} 2>&1
echo "=== nc {ip}:8080 ==="
RESULT=$(echo "HI" | nc -w 3 {ip} 8080 2>&1)
echo "nc exit: $?"
echo "received: '$RESULT'"
"#,
        ip = ip_a,
    );

    println!("[main] Spawning client ...\n");
    let client = Command::new("/bin/sh")
        .args(&["-c", &client_script])
        .stdin(Stdio::Null)
        .stdout(Stdio::Piped)
        .stderr(Stdio::Piped)
        .with_chroot(&rootfs)
        .with_namespaces(Namespace::UTS | Namespace::MOUNT)
        .env("PATH", ALPINE_PATH)
        .with_proc_mount()
        .with_network(NetworkMode::Bridge)
        .spawn()
        .expect("Failed to spawn client");

    println!(
        "[main] client: PID={} IP={}\n",
        client.pid(),
        client.container_ip().unwrap()
    );

    let (_, stdout_b, stderr_b) = client.wait_with_output().expect("wait client");
    println!("--- client stdout ---");
    print!("{}", String::from_utf8_lossy(&stdout_b));
    if !stderr_b.is_empty() {
        println!("--- client stderr ---");
        print!("{}", String::from_utf8_lossy(&stderr_b));
    }

    // Kill server (use wait, not wait_with_output — pipes + shell loops hang)
    println!("\n[main] Killing server ...");
    unsafe {
        libc::kill(pid_a as i32, libc::SIGTERM);
    }
    let _ = server.wait();
    println!("[main] Done — networking works!");
}
