#!/usr/bin/env bash
# Strata (https://github.com/Niko1221/Strata) を取得 / 更新する。モデル本体 (~70GB) の導入は対話式なので
# ここでは行わず、完了後に表示される手順 (./setup.sh) をターミナルで一度だけ実行する。
set -euo pipefail
DIR="${STRATA_DIR:-$HOME/.llm-harness/strata}"
if [ -d "$DIR/.git" ]; then
  git -C "$DIR" pull --ff-only
else
  mkdir -p "$(dirname "$DIR")"
  git clone --depth 1 https://github.com/Niko1221/Strata "$DIR"
fi
echo
echo "Strata を $DIR に取得しました。"
echo "次にターミナルで一度だけ実行してください (GPU/RAM を検出し、モデルをダウンロードします):"
echo "  cd $DIR && ./setup.sh"
echo "完了すると strata-<model>.json が作られ、ハーネスの『サーバを起動』で Strata を選べます。"
