#!/usr/bin/env bash
# chip-tool を Docker（host network）で実行するラッパー。
# ペアリング状態は $CHIP_TOOL_STORE に永続化される（コンテナ間で共有）。
set -euo pipefail
IMAGE="${CHIP_TOOL_IMAGE:-atios/chip-tool:latest}"
STORE="${CHIP_TOOL_STORE:-$HOME/.chip-tool-store}"
mkdir -p "$STORE"
exec docker run --rm --network host \
  -v "$STORE":/root/.chip-tool-store \
  -v "$PWD":/workdir -w /workdir \
  "$IMAGE" "$@" --storage-directory /root/.chip-tool-store
