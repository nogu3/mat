#!/usr/bin/env bash
# chip-tool を Docker（host network）で実行するラッパー。
# ペアリング状態は $CHIP_TOOL_STORE に永続化される（コンテナ間で共有）。
# $CHIP_TOOL_IMAGE で既定イメージ（spike で確定した digest 固定版）を差し替え可能。
set -euo pipefail
IMAGE="${CHIP_TOOL_IMAGE:-atios/chip-tool@sha256:b0f75334f7264af16c19ea0f4880a20ed86b821cd12c6a553c8e012aa0344277}"
STORE="${CHIP_TOOL_STORE:-$HOME/.chip-tool-store}"
mkdir -p "$STORE"
exec docker run --rm --network host \
  -v "$STORE":/root/.chip-tool-store \
  -v "$PWD":/workdir -w /workdir \
  "$IMAGE" "$@" --storage-directory /root/.chip-tool-store
  # ↑ 末尾固定は意図的: 呼び出し側が同名オプションを渡しても chip-tool は後勝ちでこちらを採用する
