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
# YouTube). The `[default]` extra is what yt-dlp's own install instructions ask
# for: brotli, websockets and the impersonation support a bare `pip install
# yt-dlp` leaves out, all of which YouTube extraction leans on.
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
