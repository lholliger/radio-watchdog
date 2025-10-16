# does this first one need to be 1-bookworm-slim?
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef 
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder 
COPY --from=planner /app/recipe.json recipe.json

RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
RUN cargo build --release

FROM debian:bookworm-slim AS nrsc-builder
WORKDIR /app
RUN apt-get update && \
    apt-get install -y \
    git \
    build-essential \
    cmake \
    autoconf \
    libtool \
    libao-dev \
    libfftw3-dev \
    librtlsdr-dev && \
    git clone https://github.com/theori-io/nrsc5.git && \
    cd nrsc5 && \
    mkdir build && \
    cd build && \
    cmake .. && \
    make


FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install rtl-sdr openssl ffmpeg libao4 libfftw3-dev netcat-openbsd libc6 -y
WORKDIR /app
COPY --from=nrsc-builder /app/nrsc5/build/src/nrsc5 /usr/local/bin
COPY --from=builder /app/target/release/watchdog /usr/local/bin
ENTRYPOINT ["/usr/local/bin/watchdog"]