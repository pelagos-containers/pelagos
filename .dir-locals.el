;; This project only targets Linux (see rust-toolchain.toml) -- rust-analyzer
;; defaults to the host target (aarch64-apple-darwin on this Mac), which fails
;; to resolve Linux-only libc bindings (seccomp, cgroups2, eventfd, prctl).
;; Point it at the matching Linux musl target instead. Requires a real
;; aarch64-linux-musl cross-compiler on PATH as `musl-gcc` (matches
;; .cargo/config.toml's linker setting) -- see
;; https://github.com/messense/homebrew-macos-cross-toolchains
;; (`brew install messense/macos-cross-toolchains/aarch64-unknown-linux-musl`,
;; then symlink the installed aarch64-linux-musl-gcc as musl-gcc on PATH).
((rust-mode . ((lsp-rust-analyzer-cargo-target . "aarch64-unknown-linux-musl")))
 (rust-ts-mode . ((lsp-rust-analyzer-cargo-target . "aarch64-unknown-linux-musl"))))
