;; This project only targets Linux (see rust-toolchain.toml) -- rust-analyzer
;; defaults to the host target (aarch64-apple-darwin on this Mac), which fails
;; to resolve Linux-only libc bindings (seccomp, cgroups2, eventfd, prctl).
;; Point it at the matching Linux musl target instead.
;;
;; Requires a real cross-compiler on PATH as `musl-gcc` -- see
;; https://github.com/messense/homebrew-macos-cross-toolchains
;; (`brew install messense/macos-cross-toolchains/aarch64-unknown-linux-musl`,
;; then symlink the installed aarch64-linux-musl-gcc as musl-gcc on PATH)
;; -- plus per-crate env vars (e.g. rustls's crypto backend needs
;; AWS_LC_SYS_NO_ASM=1 to skip a Linux-only asm build path) that belong in
;; ~/.cargo/config.toml (machine-local, not project config -- see
;; https://doc.rust-lang.org/cargo/reference/config.html), not here.
((rust-mode . ((lsp-rust-analyzer-cargo-target . "aarch64-unknown-linux-musl")))
 (rust-ts-mode . ((lsp-rust-analyzer-cargo-target . "aarch64-unknown-linux-musl"))))
