#!/usr/bin/env bash
# 把 SenseVoice 模型下到 ./models/sensevoice/，供本地非 Nix 构建或手工测试使用。
#
# 注意：Nix 包不需要跑这个脚本 —— 模型已经 fetchurl 打好包、随包安装。
# 这里只是给「不用 Nix，自己 cmake 构建」的场景和 debug 提供方便。
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="$REPO_ROOT/models/sensevoice"
MODEL_FILE="$TARGET_DIR/model.int8.onnx"
TOKENS_FILE="$TARGET_DIR/tokens.txt"

# 和 trime-bibi-keyboard 一样：镜像优先、官方源兜底
BASE_URL="https://hf-mirror.com/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main"
BASE_URL_FALLBACK="https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main"

download() {
    local url="$1" out="$2"
    echo "下载 $out"
    curl -L --fail --retry 5 --retry-all-errors --retry-delay 3 \
        --connect-timeout 30 --progress-bar -o "$out.part" "$url"
    mv "$out.part" "$out"
}

if [[ -s "$MODEL_FILE" && -s "$TOKENS_FILE" ]]; then
    echo "模型已存在，跳过下载。"
    ls -lh "$MODEL_FILE" "$TOKENS_FILE"
    exit 0
fi

mkdir -p "$TARGET_DIR"

if [[ ! -s "$TOKENS_FILE" ]]; then
    download "$BASE_URL/tokens.txt" "$TOKENS_FILE" || \
        download "$BASE_URL_FALLBACK/tokens.txt" "$TOKENS_FILE"
fi

if [[ ! -s "$MODEL_FILE" ]]; then
    download "$BASE_URL/model.int8.onnx" "$MODEL_FILE" || \
        download "$BASE_URL_FALLBACK/model.int8.onnx" "$MODEL_FILE"
fi

echo "完成："
ls -lh "$MODEL_FILE" "$TOKENS_FILE"
