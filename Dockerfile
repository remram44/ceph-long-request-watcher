FROM ubuntu:18.04 AS builder

RUN \
    apt-get update && \
    apt-get install -yy \
    curl gcc pkg-config git make musl-tools libssl-dev \
    libacl1-dev \
    && \
    rm -rf /var/lib/apt/lists/*
RUN curl -Lo rustup.sh https://sh.rustup.rs && \
    sh rustup.sh --default-toolchain nightly --no-modify-path -y && \
    rm rustup.sh

ENV PATH=$PATH:/bin:/root/.cargo/bin
WORKDIR /src

ARG TARGETPLATFORM

RUN if [ ${TARGETPLATFORM} = "linux/amd64" ]; then rustup target add x86_64-unknown-linux-musl; elif [ ${TARGETPLATFORM} = "linux/arm64" ]; then rustup target add aarch64-unknown-linux-musl; else echo "Unknown platform"; exit 1; fi
ENV PKG_CONFIG_ALLOW_CROSS=1

COPY Cargo.toml ./
COPY Cargo.lock ./
COPY src ./src

RUN --mount=type=cache,target=/root/.cargo/registry if [ ${TARGETPLATFORM} = "linux/amd64" ]; then cargo build --target x86_64-unknown-linux-musl --release && cp target/x86_64-unknown-linux-musl/release/ceph-long-request-watcher /ceph-long-request-watcher; elif [ ${TARGETPLATFORM} = "linux/arm64" ]; then cargo build --target aarch64-unknown-linux-musl --release && cp target/aarch64-unknown-linux-musl/release/ceph-long-request-watcher /ceph-long-request-watcher; else echo "Unknown platform"; exit 1; fi


FROM busybox

COPY --from=builder /ceph-long-request-watcher /usr/local/bin/ceph-long-request-watcher

ENTRYPOINT ["ceph-long-request-watcher"]
