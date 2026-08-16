FROM rust:1.91-bookworm

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends valgrind \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /source
