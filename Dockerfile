# Build stage
FROM rust:1-slim-bookworm AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app

# Build dependencies first so they stay cached when only the app's code changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# Make sure cargo rebuilds the app rather than reusing the placeholder binary.
RUN touch src/main.rs && cargo build --release --locked

# Runtime stage
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data twitch-chat \
    && mkdir /data && chown twitch-chat /data
COPY --from=build /app/target/release/twitch-chat /usr/local/bin/twitch-chat

USER twitch-chat
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["twitch-chat"]
CMD ["--help"]
