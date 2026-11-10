# syntax=docker/dockerfile:1
#
# x86_64 Linux 向けマルチステージビルド。mat / matd は Phase 5 M8c-3 で pure
# Rust の native バックエンド一本になった（chip-tool は撤去済み）ので、イメージは
# バイナリ 2 本 + 最小ランタイムだけ。BLE (`ble` feature) は deploy 専用の opt-in
# なのでこの host ビルドでは無効 — libdbus 等の C 依存は不要。
#
# 注意: Matter は mDNS / IPv6 マルチキャストを使うため、実行は host networking 必須
#       （`docker run --network host ...`）。bridge では応答を受けられない。

# ── Stage 1: mat / matd をビルド（ワークスペース全体）──────────────────────────
FROM rust:1-bookworm AS mat-builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# --locked: コミット済み Cargo.lock と違う依存版を黙って解決させない（ずれていればビルド失敗）
RUN cargo build --locked --release

# ── Stage 1b: テスト専用ステージ（task docker:test 用、ローカルツールチェーン不要）─
FROM mat-builder AS test
RUN cargo test --locked --release

# ── Stage 2: ランタイム（軽量。バイナリだけ載せる）────────────────────────────
# native バックエンドは自前 crypto/mDNS（pure Rust）なので chip-tool 時代の
# libssl / libdbus / libglib / libavahi は一切不要。glibc に合わせて bookworm。
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=mat-builder /src/target/release/mat /usr/local/bin/mat
COPY --from=mat-builder /src/target/release/matd /usr/local/bin/matd

# 非 root で動かす（mat / matd は特権不要: mDNS も Matter も 1024 番以上の UDP）。
# store は fabric の root CA 秘密鍵を持つので、既定パスを 0700 で先に作っておく
# （空の named volume はこの所有者・モードを引き継ぐ）。bind mount するなら
# ホスト側 dir の所有者を合わせるか `docker run --user <uid>:<gid>` で上書きする。
RUN useradd --uid 10001 --user-group --create-home mat \
    && install -d -m 700 -o mat -g mat /home/mat/.config /home/mat/.config/mat
USER mat

ENTRYPOINT ["mat"]

# ── mat / matd を aarch64 へクロスビルド（jarvis 等 aarch64 実機向け）──────────
# deploy の標準ビルドは host の Taskfile `dist:arm64`（cross + `--features ble`）
# を使う（BLE commission を実機で回すため）。このステージは BLE なしの素の
# aarch64 クロス（gcc-aarch64-linux-gnu リンカ）で、CI/検証用の軽量成果物。
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
RUN cargo build --locked --release --target aarch64-unknown-linux-gnu
