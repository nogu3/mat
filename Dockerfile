# syntax=docker/dockerfile:1
#
# x86_64 Linux 向けマルチステージビルド。
# chip-tool のビルドは数 GB・長時間なので「一度焼いてランタイムにバイナリだけ載せる」。
# クロスコンパイル不要・glibc ミスマッチなしの前提。
#
# 注意: Matter は mDNS / IPv6 マルチキャストを使うため、実行は host networking 必須
#       （`docker run --network host ...`）。bridge では応答を受けられない。

# ── Stage 1: chip-tool を connectedhomeip からビルド ───────────────────────────
# base image はホスト（実行先）の glibc に合わせる。バイナリを取り出してホストで
# 直接実行する場合、ホストの glibc がビルド時より新しい必要がある（後方互換は無い）。
# 取り出し実行先が Ubuntu 22.04（glibc 2.35）なので 22.04 でビルドする。
FROM ubuntu:22.04 AS chip-builder

# master は Python ビルド venv の pip 依存が壊れることがある。安定リリースタグに固定する。
ARG CHIP_REF=v1.4.2.0
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        git curl gcc g++ pkg-config libssl-dev libdbus-1-dev libglib2.0-dev \
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
# gn_gen.sh は引数なしだと out/ に gen する（out/host ではない）。
RUN bash -c "source scripts/activate.sh && \
    scripts/build/gn_gen.sh && \
    ninja -C out chip-tool"

# ── Stage 1b: chip-all-clusters-app（Phase 5 開発の相手役デバイス）──────────────
# chip-builder のビルド済みツリー上で example を1つ追加ビルドするだけ（キャッシュが効く）。
FROM chip-builder AS all-clusters-builder
RUN bash -c "source scripts/activate.sh && \
    scripts/examples/gn_build_example.sh examples/all-clusters-app/linux out/all-clusters"

# ── Stage 2: mat / matd をビルド（ワークスペース全体）──────────────────────────
FROM rust:1-bookworm AS mat-builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release

# ── Stage 2b: テスト専用ステージ（task docker:test 用、ローカルツールチェーン不要）─
FROM mat-builder AS test
RUN cargo test --release

# ── Stage 3: ランタイム（軽量。バイナリだけ載せる）────────────────────────────
# ホスト（Ubuntu 22.04）と揃える。
FROM ubuntu:22.04 AS runtime
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        libssl3 libdbus-1-3 libglib2.0-0 libavahi-client3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=chip-builder /work/connectedhomeip/out/chip-tool /usr/local/bin/chip-tool
COPY --from=mat-builder /src/target/release/mat /usr/local/bin/mat
COPY --from=mat-builder /src/target/release/matd /usr/local/bin/matd

ENV MAT_CHIP_TOOL_BIN=/usr/local/bin/chip-tool
ENTRYPOINT ["mat"]

# ── arm64 クロスビルド（Raspberry Pi 3 B+ 等 aarch64 実機向け）────────────────────
# QEMU エミュレーションではなく、公式クロスコンパイル環境イメージで母艦ネイティブ
# 速度のクロスビルドを行う。chip-build-crosscompile は aarch64 sysroot を同梱し、
# 環境変数 SYSROOT_AARCH64 を設定済み。build_examples.py の linux-arm64 ターゲットが
# target_cpu="arm64" / sysroot=$SYSROOT_AARCH64 を自動で渡す。
# タグ 145 は v1.4.2.0 ツリーの integrations/docker version と同世代。
#
# 取り出したバイナリは Pi 上で Docker 無しに直接実行する（1GB RAM で常駐 Docker は
# 重いため）。glibc は実機 trixie(2.41) > ビルド側 → 前方互換で動作する。
# 実機ランタイム依存: libssl3 libdbus-1-3 libglib2.0-0 libavahi-client3 avahi-daemon
FROM ghcr.io/project-chip/chip-build-crosscompile:145 AS chip-builder-arm64
ARG CHIP_REF=v1.4.2.0
WORKDIR /work
RUN git clone --depth 1 --branch ${CHIP_REF} \
        https://github.com/project-chip/connectedhomeip.git
WORKDIR /work/connectedhomeip
RUN scripts/checkout_submodules.py --shallow --platform linux
# arm64 ボードは clang 必須（gcc 単体は ONLY IF '-(clang|nodeps)' で弾かれる）。
# clang = 通常の system 依存（avahi/glib/dbus/ssl, sysroot 同梱）を使う。
# 出力: out/linux-arm64-chip-tool-clang/chip-tool
RUN bash -c "source scripts/activate.sh && \
    ./scripts/build/build_examples.py --target linux-arm64-chip-tool-clang build"

# ── mat / matd を aarch64 へクロスビルド ──────────────────────────────────────
# mat は軽量なので母艦ネイティブの Rust クロス（gcc-aarch64-linux-gnu リンカ）で焼く。
FROM rust:1-bookworm AS mat-builder-arm64
RUN rustup target add aarch64-unknown-linux-gnu \
    && apt-get update && apt-get install -y --no-install-recommends \
        gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
    && rm -rf /var/lib/apt/lists/*
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# 出力: target/aarch64-unknown-linux-gnu/release/{mat,matd}
RUN cargo build --release --target aarch64-unknown-linux-gnu
