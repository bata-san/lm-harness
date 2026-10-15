#!/usr/bin/env bash
# Strata のベンチマーク (./bench.sh)。5〜10 分かかり、GPU / VRAM を使う。
# 起動中の Strata モデルがあると結果がぶれるので、先にハーネスで停止しておくこと。
set -euo pipefail
DIR="${STRATA_DIR:-$HOME/.llm-harness/strata}"
if [ ! -f "$DIR/bench.sh" ]; then
  echo "$DIR/bench.sh が見つかりません。先に『Strata を取得 / 更新』を実行してください" >&2
  exit 1
fi
cd "$DIR"
echo "==> ./bench.sh ($DIR)"
./bench.sh
echo "==> ベンチマークが完了しました"
