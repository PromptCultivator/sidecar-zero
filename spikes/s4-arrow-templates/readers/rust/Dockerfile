# Spike S4: Rust reader (arrow-ipc) and the alignment-64 writer probe.
FROM rust:1
WORKDIR /s4/rust
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
# -j 4: other spikes build on the same VM.
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=scz-spike-s4-cargo-registry \
    --mount=type=cache,target=/s4/rust/target,id=scz-spike-s4-cargo-target \
    cargo build --release -j 4 && cp target/release/s4-rust /usr/local/bin/s4-rust \
    && cargo tree --depth 1 > /s4/rust/versions.txt && rustc --version >> /s4/rust/versions.txt
ENTRYPOINT ["/usr/local/bin/s4-rust"]
