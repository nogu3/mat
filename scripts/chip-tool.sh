#!/usr/bin/env bash
# chip-tool を Docker（host network）で実行するラッパー。
# ペアリング状態は $CHIP_TOOL_STORE に永続化される（コンテナ間で共有）。
# $CHIP_TOOL_IMAGE で既定イメージ（spike で確定した digest 固定版）を差し替え可能。
set -euo pipefail
# Resolve relative to this script's own location, not $PWD: $PWD is the
# caller's cwd, which this script deliberately leaves untouched (it's
# mounted below as the container's /workdir).
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
. "$SCRIPT_DIR/chip-tool-image.env"
IMAGE="${CHIP_TOOL_IMAGE:-$CHIP_TOOL_IMAGE_DEFAULT}"
STORE="${CHIP_TOOL_STORE:-$HOME/.chip-tool-store}"
mkdir -p "$STORE"
exec docker run --rm --network host \
  -v "$STORE":/root/.chip-tool-store \
  -v "$PWD":/workdir -w /workdir \
  "$IMAGE" "$@" --storage-directory /root/.chip-tool-store
  # ↑ 末尾固定は意図的: 呼び出し側が同名オプションを渡しても chip-tool は後勝ちでこちらを採用する
