# syntax=docker/dockerfile:1
FROM rust:1.88-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p rtflow-server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --home /var/lib/rtflow --shell /usr/sbin/nologin rtflow
COPY --from=build /src/target/release/rtflow-server /usr/local/bin/rtflow-server
COPY crates/rtflow-server/deploy/rtflow.toml /etc/rtflow/rtflow.toml
RUN mkdir -p /var/lib/rtflow/recordings /var/lib/rtflow/state /var/log/rtflow \
    && chown -R rtflow:rtflow /var/lib/rtflow /var/log/rtflow
USER rtflow
WORKDIR /var/lib/rtflow
EXPOSE 8554 1935 9000/udp 3000 8088
ENTRYPOINT ["rtflow-server", "--config", "/etc/rtflow/rtflow.toml"]
