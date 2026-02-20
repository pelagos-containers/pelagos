//! Build engine for creating OCI images from Remfiles (simplified Dockerfiles).
//!
//! The build process reads a Remfile, executes each instruction in sequence,
//! and produces an `ImageManifest` stored in the local image store.

use crate::container::{Command, Namespace, Stdio};
use crate::image::{self, ImageConfig, ImageManifest};
use crate::network::NetworkMode;
use std::io;
use std::path::Path;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("parse error at line {line}: {message}")]
    Parse { line: usize, message: String },

    #[error("FROM must be the first instruction")]
    MissingFrom,

    #[error("image '{0}' not found locally; run 'remora image pull {0}' first")]
    ImageNotFound(String),

    #[error("RUN command failed with exit code {0}")]
    RunFailed(i32),

    #[error("container error: {0}")]
    Container(#[from] crate::container::Error),

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

// ---------------------------------------------------------------------------
// Instruction AST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instruction {
    From(String),
    Run(String),
    Copy { src: String, dest: String },
    Cmd(Vec<String>),
    Env { key: String, value: String },
    Workdir(String),
    Expose(u16),
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse a Remfile into a list of instructions.
pub fn parse_remfile(content: &str) -> Result<Vec<Instruction>, BuildError> {
    let mut instructions = Vec::new();
    let mut lines = content.lines().enumerate().peekable();

    while let Some((line_num, raw_line)) = lines.next() {
        let line_num = line_num + 1; // 1-indexed for error messages
        let mut line = raw_line.trim().to_string();

        // Skip blank lines and comments.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Handle continuation lines (trailing backslash).
        while line.ends_with('\\') {
            line.pop(); // remove backslash
            if let Some((_, next)) = lines.next() {
                line.push(' ');
                line.push_str(next.trim());
            }
        }

        let (keyword, rest) = split_instruction(&line);
        let rest = rest.trim();

        match keyword.to_ascii_uppercase().as_str() {
            "FROM" => {
                if rest.is_empty() {
                    return Err(BuildError::Parse {
                        line: line_num,
                        message: "FROM requires an image reference".to_string(),
                    });
                }
                instructions.push(Instruction::From(rest.to_string()));
            }
            "RUN" => {
                if rest.is_empty() {
                    return Err(BuildError::Parse {
                        line: line_num,
                        message: "RUN requires a command".to_string(),
                    });
                }
                instructions.push(Instruction::Run(rest.to_string()));
            }
            "COPY" => {
                let parts: Vec<&str> = rest.splitn(2, char::is_whitespace).collect();
                if parts.len() < 2 {
                    return Err(BuildError::Parse {
                        line: line_num,
                        message: "COPY requires <src> <dest>".to_string(),
                    });
                }
                instructions.push(Instruction::Copy {
                    src: parts[0].to_string(),
                    dest: parts[1].trim().to_string(),
                });
            }
            "CMD" => {
                let cmd = parse_cmd_value(rest).map_err(|msg| BuildError::Parse {
                    line: line_num,
                    message: msg,
                })?;
                instructions.push(Instruction::Cmd(cmd));
            }
            "ENV" => {
                let (key, value) = parse_env_value(rest).ok_or_else(|| BuildError::Parse {
                    line: line_num,
                    message: "ENV requires KEY=VALUE or KEY VALUE".to_string(),
                })?;
                instructions.push(Instruction::Env { key, value });
            }
            "WORKDIR" => {
                if rest.is_empty() {
                    return Err(BuildError::Parse {
                        line: line_num,
                        message: "WORKDIR requires a path".to_string(),
                    });
                }
                instructions.push(Instruction::Workdir(rest.to_string()));
            }
            "EXPOSE" => {
                let port: u16 = rest
                    .split('/')
                    .next()
                    .unwrap_or(rest)
                    .parse()
                    .map_err(|_| BuildError::Parse {
                        line: line_num,
                        message: format!("invalid port number: {}", rest),
                    })?;
                instructions.push(Instruction::Expose(port));
            }
            other => {
                return Err(BuildError::Parse {
                    line: line_num,
                    message: format!("unknown instruction: {}", other),
                });
            }
        }
    }

    Ok(instructions)
}

/// Split a line into (keyword, rest).
fn split_instruction(line: &str) -> (&str, &str) {
    match line.split_once(char::is_whitespace) {
        Some((kw, rest)) => (kw, rest),
        None => (line, ""),
    }
}

/// Parse CMD value: supports JSON array `["a", "b"]` or shell form `a b c`.
fn parse_cmd_value(rest: &str) -> Result<Vec<String>, String> {
    let trimmed = rest.trim();
    if trimmed.starts_with('[') {
        // JSON array form: ["cmd", "arg1", "arg2"]
        let parsed: Vec<String> =
            serde_json::from_str(trimmed).map_err(|e| format!("invalid CMD JSON: {}", e))?;
        if parsed.is_empty() {
            return Err("CMD cannot be empty".to_string());
        }
        Ok(parsed)
    } else {
        // Shell form: wrap in /bin/sh -c
        if trimmed.is_empty() {
            return Err("CMD requires a command".to_string());
        }
        Ok(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            trimmed.to_string(),
        ])
    }
}

/// Parse ENV: supports `KEY=VALUE` or `KEY VALUE`.
fn parse_env_value(rest: &str) -> Option<(String, String)> {
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((k, v)) = trimmed.split_once('=') {
        Some((k.to_string(), v.to_string()))
    } else if let Some((k, v)) = trimmed.split_once(char::is_whitespace) {
        Some((k.to_string(), v.trim().to_string()))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Build execution
// ---------------------------------------------------------------------------

/// Execute a parsed Remfile and produce a tagged image.
///
/// `context_dir` is the directory context for COPY instructions.
/// `tag` is the image reference (e.g. `"myapp:latest"`).
/// `network_mode` is the network for RUN steps (bridge for root, pasta for rootless).
pub fn execute_build(
    instructions: &[Instruction],
    context_dir: &Path,
    tag: &str,
    network_mode: NetworkMode,
) -> Result<ImageManifest, BuildError> {
    if instructions.is_empty() {
        return Err(BuildError::MissingFrom);
    }

    // First instruction must be FROM.
    let base_ref = match &instructions[0] {
        Instruction::From(r) => r.clone(),
        _ => return Err(BuildError::MissingFrom),
    };

    // Load base image.
    let normalised = normalise_image_reference(&base_ref);
    let base_manifest =
        image::load_image(&normalised).map_err(|_| BuildError::ImageNotFound(base_ref.clone()))?;

    // Accumulated state.
    let mut layers: Vec<String> = base_manifest.layers.clone();
    let mut config = base_manifest.config.clone();
    let total = instructions.len();

    for (idx, instr) in instructions.iter().enumerate() {
        let step = idx + 1;
        match instr {
            Instruction::From(ref r) => {
                eprintln!("Step {}/{}: FROM {}", step, total, r);
            }
            Instruction::Run(ref cmd_text) => {
                eprintln!("Step {}/{}: RUN {}", step, total, cmd_text);
                let new_digest = execute_run(cmd_text, &layers, &config, network_mode.clone())?;
                if let Some(digest) = new_digest {
                    layers.push(digest);
                }
            }
            Instruction::Copy { ref src, ref dest } => {
                eprintln!("Step {}/{}: COPY {} {}", step, total, src, dest);
                let digest = execute_copy(src, dest, context_dir)?;
                layers.push(digest);
            }
            Instruction::Cmd(ref args) => {
                eprintln!("Step {}/{}: CMD {:?}", step, total, args);
                config.cmd = args.clone();
            }
            Instruction::Env { ref key, ref value } => {
                eprintln!("Step {}/{}: ENV {}={}", step, total, key, value);
                // Remove any existing entry for this key, then add.
                config.env.retain(|e| !e.starts_with(&format!("{}=", key)));
                config.env.push(format!("{}={}", key, value));
            }
            Instruction::Workdir(ref path) => {
                eprintln!("Step {}/{}: WORKDIR {}", step, total, path);
                config.working_dir = path.clone();
            }
            Instruction::Expose(port) => {
                eprintln!("Step {}/{}: EXPOSE {}", step, total, port);
                // Metadata only — no layer created.
            }
        }
    }

    // Compute a digest for the final manifest.
    let digest = compute_manifest_digest(&layers);

    let manifest = ImageManifest {
        reference: tag.to_string(),
        digest,
        layers,
        config,
    };

    image::save_image(&manifest)?;

    Ok(manifest)
}

/// Execute a RUN instruction: spawn a container, wait, capture upper layer.
fn execute_run(
    cmd_text: &str,
    current_layers: &[String],
    config: &ImageConfig,
    network_mode: NetworkMode,
) -> Result<Option<String>, BuildError> {
    let layer_dirs = current_layers
        .iter()
        .rev()
        .map(|d| image::layer_dir(d))
        .collect::<Vec<_>>();

    // Note: with_image_layers sets Namespace::MOUNT internally, so we must
    // add UTS|IPC *before* it (with_namespaces does assignment, not |=).
    let mut cmd = Command::new("/bin/sh")
        .args(["-c", cmd_text])
        .with_namespaces(Namespace::UTS | Namespace::IPC)
        .with_image_layers(layer_dirs)
        .stdin(Stdio::Null)
        .stdout(Stdio::Inherit)
        .stderr(Stdio::Inherit);

    // Apply accumulated environment.
    for env_str in &config.env {
        if let Some((k, v)) = env_str.split_once('=') {
            cmd = cmd.env(k, v);
        }
    }

    // Apply accumulated workdir.
    if !config.working_dir.is_empty() {
        cmd = cmd.with_cwd(&config.working_dir);
    }

    // Apply network mode for package installs etc.
    cmd = cmd.with_network(network_mode);

    let mut child = cmd.spawn()?;
    let (status, overlay_base) = child.wait_preserve_overlay()?;

    if !status.success() {
        // Clean up overlay base if present.
        if let Some(ref base) = overlay_base {
            let _ = std::fs::remove_dir_all(base);
        }
        return Err(BuildError::RunFailed(status.code().unwrap_or(1)));
    }

    // Check if upper dir has any content.
    let result = if let Some(ref base) = overlay_base {
        let upper = base.join("upper");
        if upper.is_dir() && dir_has_content(&upper)? {
            let digest = create_layer_from_dir(&upper)?;
            Some(digest)
        } else {
            None
        }
    } else {
        None
    };

    // Clean up overlay base dir now that we've captured the layer.
    if let Some(ref base) = overlay_base {
        let _ = std::fs::remove_dir_all(base);
    }

    Ok(result)
}

/// Execute a COPY instruction: create a layer from context files.
fn execute_copy(src: &str, dest: &str, context_dir: &Path) -> Result<String, BuildError> {
    let src_path = context_dir.join(src);
    if !src_path.exists() {
        return Err(BuildError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!("COPY source not found: {}", src_path.display()),
        )));
    }

    // Prevent path traversal outside context dir.
    let canonical_src = src_path.canonicalize()?;
    let canonical_ctx = context_dir.canonicalize()?;
    if !canonical_src.starts_with(&canonical_ctx) {
        return Err(BuildError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "COPY source '{}' is outside build context",
                src_path.display()
            ),
        )));
    }

    let tmp = tempfile::tempdir()?;

    // Build the destination path structure inside temp dir.
    // Strip leading '/' from dest to make it relative.
    let relative_dest = dest.strip_prefix('/').unwrap_or(dest);
    let dest_in_tmp = tmp.path().join(relative_dest);

    if let Some(parent) = dest_in_tmp.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if src_path.is_dir() {
        copy_dir_recursive(&src_path, &dest_in_tmp)?;
    } else {
        std::fs::copy(&src_path, &dest_in_tmp)?;
    }

    let digest = create_layer_from_dir(tmp.path())?;
    Ok(digest)
}

// ---------------------------------------------------------------------------
// Layer creation
// ---------------------------------------------------------------------------

/// Create a content-addressable layer from a directory's contents.
///
/// 1. Tar+gzip the directory contents to compute sha256 digest.
/// 2. If layer already exists (dedup), return early.
/// 3. Copy the directory contents to the layer store.
/// 4. Return the `sha256:<hex>` digest string.
pub fn create_layer_from_dir(source_dir: &Path) -> Result<String, io::Error> {
    use sha2::{Digest, Sha256};

    // Create a tar.gz in memory to compute the digest.
    let mut tar_gz_bytes = Vec::new();
    {
        let gz_encoder =
            flate2::write::GzEncoder::new(&mut tar_gz_bytes, flate2::Compression::fast());
        let mut tar_builder = tar::Builder::new(gz_encoder);
        tar_builder.append_dir_all(".", source_dir)?;
        let gz_encoder = tar_builder.into_inner()?;
        gz_encoder.finish()?;
    }

    let mut hasher = Sha256::new();
    hasher.update(&tar_gz_bytes);
    let hash = hasher.finalize();
    let hex = format!("{:x}", hash);
    let digest = format!("sha256:{}", hex);

    // Check if layer already exists (dedup).
    if image::layer_exists(&digest) {
        log::debug!("layer {} already exists, skipping", &hex[..12]);
        return Ok(digest);
    }

    // Copy directory contents to the layer store.
    let dest = image::layer_dir(&digest);
    std::fs::create_dir_all(&dest)?;
    copy_dir_recursive(source_dir, &dest)?;

    log::debug!("created layer {}", &hex[..12]);
    Ok(digest)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a directory contains any entries.
fn dir_has_content(dir: &Path) -> Result<bool, io::Error> {
    let mut entries = std::fs::read_dir(dir)?;
    Ok(entries.next().is_some())
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), io::Error> {
    if !dst.exists() {
        std::fs::create_dir_all(dst)?;
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else if file_type.is_symlink() {
            let target = std::fs::read_link(entry.path())?;
            // Remove existing symlink/file if present.
            let _ = std::fs::remove_file(&dest_path);
            std::os::unix::fs::symlink(target, &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}

/// Compute a deterministic digest from the ordered layer list.
fn compute_manifest_digest(layers: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for layer in layers {
        hasher.update(layer.as_bytes());
        hasher.update(b"\n");
    }
    let hash = hasher.finalize();
    format!("sha256:{:x}", hash)
}

/// Expand bare image names: "alpine" -> "docker.io/library/alpine:latest".
fn normalise_image_reference(reference: &str) -> String {
    let r = reference.to_string();
    let r = if !r.contains(':') && !r.contains('@') {
        format!("{}:latest", r)
    } else {
        r
    };
    if !r.contains('/') {
        format!("docker.io/library/{}", r)
    } else {
        r
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_remfile() {
        let content = r#"
FROM alpine:latest
RUN apk add --no-cache curl
COPY index.html /var/www/index.html
ENV APP_PORT=8080
WORKDIR /var/www
CMD ["httpd", "-f", "-p", "8080"]
EXPOSE 8080
"#;
        let instructions = parse_remfile(content).unwrap();
        assert_eq!(instructions.len(), 7);
        assert_eq!(instructions[0], Instruction::From("alpine:latest".into()));
        assert_eq!(
            instructions[1],
            Instruction::Run("apk add --no-cache curl".into())
        );
        assert_eq!(
            instructions[2],
            Instruction::Copy {
                src: "index.html".into(),
                dest: "/var/www/index.html".into()
            }
        );
        assert_eq!(
            instructions[3],
            Instruction::Env {
                key: "APP_PORT".into(),
                value: "8080".into()
            }
        );
        assert_eq!(instructions[4], Instruction::Workdir("/var/www".into()));
        assert_eq!(
            instructions[5],
            Instruction::Cmd(vec![
                "httpd".into(),
                "-f".into(),
                "-p".into(),
                "8080".into()
            ])
        );
        assert_eq!(instructions[6], Instruction::Expose(8080));
    }

    #[test]
    fn test_parse_comments_and_blank_lines() {
        let content = r#"
# This is a comment
FROM alpine

# Another comment

RUN echo hello
"#;
        let instructions = parse_remfile(content).unwrap();
        assert_eq!(instructions.len(), 2);
    }

    #[test]
    fn test_parse_continuation_lines() {
        let content = "FROM alpine\nRUN apk add \\\n  curl \\\n  wget";
        let instructions = parse_remfile(content).unwrap();
        assert_eq!(instructions.len(), 2);
        assert_eq!(
            instructions[1],
            Instruction::Run("apk add  curl  wget".into())
        );
    }

    #[test]
    fn test_parse_cmd_shell_form() {
        let content = "FROM alpine\nCMD echo hello world";
        let instructions = parse_remfile(content).unwrap();
        assert_eq!(
            instructions[1],
            Instruction::Cmd(vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo hello world".into()
            ])
        );
    }

    #[test]
    fn test_parse_cmd_json_form() {
        let content = r#"FROM alpine
CMD ["/bin/sh", "-c", "echo hello"]"#;
        let instructions = parse_remfile(content).unwrap();
        assert_eq!(
            instructions[1],
            Instruction::Cmd(vec!["/bin/sh".into(), "-c".into(), "echo hello".into()])
        );
    }

    #[test]
    fn test_parse_env_equals_form() {
        let content = "FROM alpine\nENV MY_VAR=hello_world";
        let instructions = parse_remfile(content).unwrap();
        assert_eq!(
            instructions[1],
            Instruction::Env {
                key: "MY_VAR".into(),
                value: "hello_world".into()
            }
        );
    }

    #[test]
    fn test_parse_env_space_form() {
        let content = "FROM alpine\nENV MY_VAR hello world";
        let instructions = parse_remfile(content).unwrap();
        assert_eq!(
            instructions[1],
            Instruction::Env {
                key: "MY_VAR".into(),
                value: "hello world".into()
            }
        );
    }

    #[test]
    fn test_parse_expose_with_protocol() {
        let content = "FROM alpine\nEXPOSE 8080/tcp";
        let instructions = parse_remfile(content).unwrap();
        assert_eq!(instructions[1], Instruction::Expose(8080));
    }

    #[test]
    fn test_parse_error_empty_from() {
        let content = "FROM";
        let err = parse_remfile(content).unwrap_err();
        assert!(err.to_string().contains("requires an image reference"));
    }

    #[test]
    fn test_parse_error_unknown_instruction() {
        let content = "FROM alpine\nFOOBAR something";
        let err = parse_remfile(content).unwrap_err();
        assert!(err.to_string().contains("unknown instruction"));
    }

    #[test]
    fn test_parse_error_copy_missing_dest() {
        let content = "FROM alpine\nCOPY onlysrc";
        let err = parse_remfile(content).unwrap_err();
        assert!(err.to_string().contains("COPY requires <src> <dest>"));
    }

    #[test]
    fn test_parse_case_insensitive() {
        let content = "from alpine\nrun echo hi\ncmd echo hello";
        let instructions = parse_remfile(content).unwrap();
        assert_eq!(instructions.len(), 3);
        assert_eq!(instructions[0], Instruction::From("alpine".into()));
    }

    #[test]
    fn test_normalise_image_reference() {
        assert_eq!(
            normalise_image_reference("alpine"),
            "docker.io/library/alpine:latest"
        );
        assert_eq!(
            normalise_image_reference("alpine:3.19"),
            "docker.io/library/alpine:3.19"
        );
        assert_eq!(
            normalise_image_reference("myregistry.io/myimage:v1"),
            "myregistry.io/myimage:v1"
        );
    }

    #[test]
    fn test_compute_manifest_digest_deterministic() {
        let layers = vec!["sha256:aaa".to_string(), "sha256:bbb".to_string()];
        let d1 = compute_manifest_digest(&layers);
        let d2 = compute_manifest_digest(&layers);
        assert_eq!(d1, d2);
        assert!(d1.starts_with("sha256:"));
    }

    #[test]
    fn test_parse_empty_file() {
        let content = "";
        let instructions = parse_remfile(content).unwrap();
        assert!(instructions.is_empty());
    }
}
