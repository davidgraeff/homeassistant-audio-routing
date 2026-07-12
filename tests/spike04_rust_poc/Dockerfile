# Spike 4 addendum: native pipewire-rs proof-of-concept, layered on the real
# pw-audio-router image so it links against the exact same libpipewire the
# production add-on will ship. Not part of the production Dockerfile — this
# is evidence for the language-choice decision in spikes/04-graph-control.md,
# not a packaging artifact.
ARG BASE_IMAGE=pw-audio-router:dev
FROM ${BASE_IMAGE}
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl build-essential pkg-config clang \
        libpipewire-0.3-dev libspa-0.2-dev \
    && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
WORKDIR /spike
RUN cargo init --name pw_graph_spike
RUN cargo add pipewire@0.10
COPY main.rs src/main.rs
RUN cargo build --release
