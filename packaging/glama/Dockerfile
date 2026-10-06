# Dockerfile for Glama's server checks (paste into the Glama admin page after
# claiming the listing). It runs the prebuilt release binaries, so it builds
# in seconds. ubuntu:24.04 because the Linux release binaries need glibc 2.39.
FROM ubuntu:24.04

ARG VEX_VERSION=1.27.2
ARG BASE=https://github.com/tenatarika/vex/releases/download/v${VEX_VERSION}

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl git \
 && rm -rf /var/lib/apt/lists/* \
 && curl -fsSL "${BASE}/vex-x86_64-unknown-linux-gnu.tar.gz" | tar -xz -C /usr/local/bin vex \
 && curl -fsSL "${BASE}/vex-mcp-x86_64-unknown-linux-gnu.tar.gz" | tar -xz -C /usr/local/bin vex-mcp \
 && mkdir -p /workspace

# vex-mcp shells out to the vex CLI; VEX_ROOT is the repository to search.
ENV VEX_BIN=/usr/local/bin/vex \
    VEX_ROOT=/workspace

WORKDIR /workspace
CMD ["vex-mcp"]
