FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive
ENV CARGO_HOME=/opt/cargo
ENV RUSTUP_HOME=/opt/rustup
ENV PATH=/opt/cargo/bin:${PATH}

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    gcc \
    clang \
    cmake \
    ninja-build \
    pkg-config \
    libudev-dev \
    libssl-dev \
    ca-certificates \
    curl \
    git \
    python3 \
    python3-pip \
    nodejs \
    npm \
    make \
    && rm -rf /var/lib/apt/lists/*

# ---------------------------------------------------------------------------
# Rust
# ---------------------------------------------------------------------------

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- \
        -y \
        --profile minimal \
        --no-modify-path

# ---------------------------------------------------------------------------
# Espressif Rust toolchain
#
# ESP32-S3 = Xtensa LX7
# no --std: we explicitly want the no_std toolchain + GCC support.
# ---------------------------------------------------------------------------

RUN cargo install espup --locked

RUN mkdir -p /opt/esp \
    && espup install \
        --export-file /opt/esp/export-esp.sh

# Make espup environment available in interactive shells.
RUN echo '. /opt/esp/export-esp.sh' >> /root/.bashrc \
    && echo '. /opt/esp/export-esp.sh' >> /etc/bash.bashrc

# ---------------------------------------------------------------------------
# ESP development tools
# ---------------------------------------------------------------------------

RUN cargo install espflash --locked \
    && cargo install esp-generate --locked

# ---------------------------------------------------------------------------
# Web tooling
#
# The browser will access the ESP32 through Web Serial.
# The container only builds and serves firmware/web assets.
# ---------------------------------------------------------------------------

RUN npm install -g http-server

# Installer Claude Code via npm
RUN npm install -g @anthropic-ai/claude-code

WORKDIR /workspace

EXPOSE 8080

CMD ["bash"]
