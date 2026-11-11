# glider in a container: the CLI/server, glider-stream, and the benchmark
# generator, on a slim Debian with Python for the benchmark harness.
#
#   docker build -t harbor.home/library/glider:dev .
#   docker run --rm -p 7878:7878 -v $PWD/data:/data harbor.home/library/glider:dev \
#       /data/app.gldb serve --addr 0.0.0.0:7878 --cache-size 256M
#
# Zero crates: the build needs nothing but rustc and cargo.

ARG RUST_IMAGE=docker.io/rust:1-slim-bookworm
ARG RUNTIME_IMAGE=docker.io/python:3.12-slim-bookworm

FROM ${RUST_IMAGE} AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY src ./src
COPY bench/Cargo.toml bench/Cargo.toml
COPY bench/src bench/src
RUN cargo build --release --workspace --bins \
    && strip target/release/glider target/release/glider-stream target/release/scale-gen

FROM ${RUNTIME_IMAGE}
COPY --from=builder /src/target/release/glider /src/target/release/glider-stream \
     /src/target/release/scale-gen /usr/local/bin/
COPY bench/scale.py k8s/demo/bench.py /opt/glider/
ENV PYTHONUNBUFFERED=1
EXPOSE 7878
ENTRYPOINT ["glider"]
