# Two stages: one to compile with the pinned toolchain, one to run with nothing
# but the certificate bundle. The service needs no writable directory and no
# shell, so the runtime image is as small as it can be.
#
# Build for the target host:
#   docker build -t skyatlaz-service .
#   docker run --rm -p 127.0.0.1:8080:8080 --env-file .env skyatlaz-service

FROM rust:1-bookworm AS build

WORKDIR /src

# Warm the dependency layer with an empty program, so a source change does not
# recompile the world.
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
COPY openapi.json ./
# The dummy main has a newer mtime than the copied sources in some build
# contexts; touching the real one forces the final compile.
RUN touch src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /nonexistent --shell /usr/sbin/nologin skyatlaz

COPY --from=build /src/target/release/skyatlaz-service /usr/local/bin/skyatlaz-service

USER skyatlaz

# In a container the service has to listen on every interface of the container
# namespace; publish it to loopback on the host (`-p 127.0.0.1:8080:8080`).
ENV BIND=0.0.0.0:8080 \
    RUST_LOG=info

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/skyatlaz-service"]
