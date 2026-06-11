//! Build script: when the `ebpf` feature is on, compile the BPF program
//! (`h-ebpf-prog`) for `bpfel-unknown-none` and embed the result so
//! `src/ebpf/source.rs` can load it with `include_bytes_aligned!`. A no-op for
//! every default (non-`ebpf`) build, so macOS dev and the Linux CI never need
//! the BPF toolchain.
//!
//! We invoke `cargo` directly with `--manifest-path` (rather than aya-build's
//! `--package`) because `h-ebpf-prog` is intentionally EXCLUDED from the server
//! workspace — it builds for a different target with a pinned nightly toolchain,
//! and a workspace member would be pulled into `cargo test --workspace` on the
//! host and fail. `--manifest-path` resolves it as its own standalone workspace.

fn main() {
    #[cfg(feature = "ebpf")]
    build_bpf();
}

#[cfg(feature = "ebpf")]
fn build_bpf() {
    use std::path::PathBuf;
    use std::process::Command;

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let prog_manifest = format!("{manifest_dir}/../h-ebpf-prog/Cargo.toml");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let target_dir = format!("{out_dir}/h-ebpf-prog-target");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".to_string());

    println!("cargo:rerun-if-changed=../h-ebpf-prog/src");
    println!("cargo:rerun-if-changed=../h-ebpf-prog/Cargo.toml");
    println!("cargo:rerun-if-changed=../h-ebpf-common/src");

    // RUSTFLAGS for the BPF compile, encoded for CARGO_ENCODED_RUSTFLAGS (one
    // \x1f-separated token each). `bpf_target_arch` is read by aya-ebpf;
    // `--btf` makes bpf-linker emit BTF for CO-RE.
    let rustflags = [
        "--cfg".to_string(),
        format!("bpf_target_arch=\"{arch}\""),
        "-Cdebuginfo=2".to_string(),
        "-Clink-arg=--btf".to_string(),
    ]
    .join("\x1f");

    let status = Command::new("rustup")
        .args([
            "run",
            "nightly",
            "cargo",
            "build",
            "--manifest-path",
            &prog_manifest,
            "-Z",
            "build-std=core",
            "--release",
            "--target",
            "bpfel-unknown-none",
            "--target-dir",
            &target_dir,
        ])
        .env("CARGO_ENCODED_RUSTFLAGS", rustflags)
        // Don't leak the host workspace's rustc wrapper into the BPF build.
        .env_remove("RUSTC")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .status()
        .expect("spawn cargo for h-ebpf-prog");
    assert!(status.success(), "h-ebpf-prog BPF build failed");

    let produced =
        PathBuf::from(&target_dir).join("bpfel-unknown-none/release/h-ebpf-prog");
    let dest = PathBuf::from(&out_dir).join("h-ebpf-prog");
    std::fs::copy(&produced, &dest).unwrap_or_else(|e| {
        panic!(
            "copy BPF object {} -> {}: {e}",
            produced.display(),
            dest.display()
        )
    });
}
