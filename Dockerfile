# syntax=docker/dockerfile:1

# wavo ships demix, demix-mcp and the agent in one image, because demix-mcp
# speaks MCP over stdio only and a stdio server has to be a child of its client
# (SPEC §3.2). The image is large — TensorFlow, via spleeter, dominates it.
#
# The two Python environments are built before the Rust binary is copied in, so
# editing wavo's source rebuilds only the last, cheap layer.
#
# NOTE: this image is linux/amd64 only. TensorFlow and tensorflow-io do publish
# cp38 aarch64 wheels, but essentia — which demix uses for key detection — has
# never published a linux aarch64 wheel in any release, and building it from
# source pulls in its whole C++ dependency tree. On an arm64 host, build with
# `--platform linux/amd64`.

# --- build the agent ---------------------------------------------------------

FROM rust:1-bookworm AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/wavo /usr/local/bin/wavo

# --- runtime -----------------------------------------------------------------

# Ubuntu 22.04 is the only mainstream base where both Python versions demix needs
# install side by side: 3.10 for the MCP SDK, 3.8 for spleeter.
FROM ubuntu:22.04

# demix is on PyPI; demix-mcp is not published yet, so it comes from the repo.
ARG DEMIX_VERSION=1.7.2
ARG DEMIX_MCP_REF=master

# yt-dlp is pinned, and the pin is the point: CI builds with `cache-from:
# type=gha`, so an unpinned `pip install yt-dlp` sits in a RUN line that never
# changes and is restored from the layer cache for ever — the image keeps
# shipping whatever release the first build happened to fetch, however green CI
# looks. YouTube breaks yt-dlp every few weeks, so bump this when downloads
# start failing with "YouTube blocked the download"; that is what invalidates
# the layer. Releases: https://pypi.org/project/yt-dlp/
ARG YT_DLP_VERSION=2026.8.19

# Node is not a nicety: YouTube's player now hands out a JavaScript challenge,
# and both downloaders demix tries need a JS runtime to answer it. yt-dlp solves
# the challenge in one (without it, formats are missing and the download dies
# with 403), and its pytubefix fallback runs botGuard for the PO token, which is
# Node specifically — `Node.js is required but not found. Tried path: node`.
# A local checkout usually has one installed already, which is exactly why this
# only ever fails in the container. Ubuntu 22.04's apt Node is 12.x, far too old,
# so the official binary is unpacked instead — just `bin/node`, no npm.
# Releases: https://nodejs.org/dist/ (SHASUMS256.txt next to the tarball).
ARG NODE_VERSION=24.20.0
ARG NODE_SHA256=855d581f8a4eb1a8117e3426de25fe02770592febcfb31369aee1ffbfee9e8ec

ENV DEBIAN_FRONTEND=noninteractive

# gpg-agent is not in the base image, and without it add-apt-repository cannot
# import the PPA's signing key — it shells out to gpg, which refuses to start.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        gpg-agent \
        software-properties-common \
    && add-apt-repository -y ppa:deadsnakes/ppa \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        ffmpeg \
        libsndfile1 \
        python3.10 \
        python3.10-venv \
        python3.8 \
        python3.8-venv \
        python3.8-distutils \
    && apt-get purge -y gpg-agent software-properties-common \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

# Python 3.10: the MCP server, and yt-dlp (apt's is too old to keep up with
# YouTube). The `[default]` extra is what pulls `yt-dlp-ejs`, the challenge
# solver script the JS runtime below executes; a bare `pip install yt-dlp` has a
# runtime but nothing to run in it, and every YouTube extraction then warns
# "Signature solving failed" and loses formats.
RUN python3.10 -m venv /opt/venv-mcp \
    && /opt/venv-mcp/bin/pip install --no-cache-dir --upgrade pip \
    && /opt/venv-mcp/bin/pip install --no-cache-dir \
        "yt-dlp[default]==${YT_DLP_VERSION}" \
        "demix-mcp @ git+https://github.com/pwittchen/demix.git@${DEMIX_MCP_REF}#subdirectory=mcp"

# Python 3.8: demix itself. `essentia` is what key detection and transposition
# to a target key are built on.
RUN python3.8 -m venv /opt/venv-demix \
    && /opt/venv-demix/bin/pip install --no-cache-dir --upgrade "pip<24" setuptools wheel \
    && /opt/venv-demix/bin/pip install --no-cache-dir \
        "demix==${DEMIX_VERSION}" \
        essentia

# The JavaScript runtime, last, so bumping it rebuilds neither Python
# environment.
RUN curl -fsSL -o /tmp/node.tar.gz \
        "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.gz" \
    && echo "${NODE_SHA256}  /tmp/node.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/node.tar.gz -C /usr/local/bin --strip-components=2 \
        "node-v${NODE_VERSION}-linux-x64/bin/node" \
    && rm /tmp/node.tar.gz \
    && node --version

# yt-dlp enables only `deno` by default, so an installed Node is ignored unless
# it is asked for by name. demix builds the yt-dlp command line itself, and
# DEMIX_YT_DLP_ARGS belongs to the operator, so the runtime is enabled where
# neither has to know about it: yt-dlp's own system config file.
RUN printf '%s\n' \
        '# Enable the Node installed in this image; yt-dlp defaults to deno only.' \
        '--js-runtimes node' \
    > /etc/yt-dlp.conf

# demix's environment comes first so that `demix-mcp`, which shells out to the
# `demix` command, finds it.
ENV PATH=/opt/venv-demix/bin:/opt/venv-mcp/bin:$PATH

COPY --from=builder /usr/local/bin/wavo /usr/local/bin/wavo

RUN useradd --system --uid 10001 --no-create-home --home-dir /work wavo \
    && install -d -o wavo -g wavo /work /work/jobs

USER wavo
WORKDIR /work

ENV WAVO_WORK_DIR=/work \
    WAVO_HEALTH_ADDR=0.0.0.0:8081 \
    RUST_LOG=wavo=info

EXPOSE 8081
VOLUME ["/work"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8081/healthz -o /dev/null || exit 1

CMD ["wavo"]
