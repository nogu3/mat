# syntax=docker/dockerfile:1
#
# x86_64 UGREEN NAS（DXP4800 Plus / Pentium Gold 8505）向けマルチステージビルド。
# chip-tool のビルドは数 GB・長時間なので「一度焼いてランタイムにバイナリだけ載せる」。
# クロスコンパイル不要・glibc ミスマッチなしの前提。
#
# 注意: Matter は mDNS / IPv6 マルチキャストを使うため、実行は host networking 必須
#       （`docker run --network host ...`）。bridge では応答を受けられない。

# ── Stage 1: chip-tool を connectedhomeip からビルド ───────────────────────────
FROM ubuntu:24.04 AS chip-builder

ARG CHIP_REF=master
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        git gcc g++ pkg-config libssl-dev libdbus-1-dev libglib2.0-dev \
        libavahi-client-dev ninja-build python3-venv python3-dev python3-pip \
        unzip libgirepository1.0-dev libcairo2-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /work
# サブモジコミ込みで数 GB。shallow + 必要サブモジュールのみ取得。
RUN git clone --depth 1 --branch ${CHIP_REF} \
        https://github.com/project-chip/connectedhomeip.git
WORKDIR /work/connectedhomeip
RUN scripts/checkout_submodules.py --shallow --platform linux

# pigweed ベースのビルド環境を bootstrap して chip-tool をビルド。
RUN bash -c "source scripts/activate.sh && \
    scripts/build/gn_gen.sh && \
    ninja -C out/host chip-tool"

# ── Stage 2: mat をビルド ─────────────────────────────────────────────────────
FROM rust:1-bookworm AS mat-builder
WORKDIR /src
COPY Cargo.toml ./
COPY src ./src
COPY tests ./tests
RUN cargo build --release

# ── Stage 2b: テスト専用ステージ（task docker:test 用、ローカルツールチェーン不要）─
FROM mat-builder AS test
RUN cargo test --release

# ── Stage 3: ランタイム（軽量。バイナリだけ載せる）────────────────────────────
FROM ubuntu:24.04 AS runtime
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        libssl3 libdbus-1-3 libglib2.0-0 libavahi-client3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=chip-builder /work/connectedhomeip/out/host/chip-tool /usr/local/bin/chip-tool
COPY --from=mat-builder /src/target/release/mat /usr/local/bin/mat

ENV MAT_CHIP_TOOL_BIN=/usr/local/bin/chip-tool
ENTRYPOINT ["mat"]
