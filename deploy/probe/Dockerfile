# heron-probe container image — for the privileged Kubernetes DaemonSet
# (deploy/probe/k8s-daemonset.yaml). Multi-stage: a builder with the BPF
# toolchain (nightly + rust-src + bpf-linker — mirrors scripts/prod/Dockerfile.
# ebpf-build) produces a glibc-2.35 `heron-probe` with the eBPF capture engine
# compiled in; the runtime stage carries only the binary + libpcap.
#
# Build from the REPO ROOT so the whole workspace is in context:
#   docker build -f deploy/probe/Dockerfile -t <registry>/heron-probe:<tag> .
#
# Base is Ubuntu 22.04 (glibc 2.35) to match common node userland; the eBPF
# program itself is kernel-version-independent via CO-RE/BTF at load time.

FROM ubuntu:22.04 AS builder
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential pkg-config libpcap-dev \
      git curl ca-certificates llvm clang \
 && rm -rf /var/lib/apt/lists/*

# Rust stable (host build) + nightly with rust-src (the BPF program is compiled
# with -Z build-std=core for bpfel-unknown-none).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH=/root/.cargo/bin:$PATH
RUN rustup toolchain install nightly --component rust-src --profile minimal
# bpf-linker performs the final link of the embedded BPF program.
RUN cargo install bpf-linker --locked

WORKDIR /build
COPY . .
# `ebpf` forwards to h-capture/ebpf; build.rs compiles h-ebpf-prog out-of-band.
RUN cd server && cargo build --release --bin heron-probe --features ebpf \
 && install -m0755 target/release/heron-probe /heron-probe

# --- runtime ---------------------------------------------------------------
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive
# libpcap is linked by the pcap crate (h-capture) even when only the eBPF source
# is used. No CA bundle needed: mTLS trust comes from the mounted server CA, not
# system roots.
RUN apt-get update && apt-get install -y --no-install-recommends libpcap0.8 \
 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /heron-probe /usr/local/bin/heron-probe
# Config + mTLS material are mounted at runtime (ConfigMap + Secret); BTF and the
# target processes come from the node via hostPID + the /sys mount.
ENTRYPOINT ["/usr/local/bin/heron-probe", "--config", "/etc/heron-probe/heron-probe.toml"]
