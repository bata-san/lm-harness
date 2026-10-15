#!/usr/bin/env bash
# Strata の更新: 取得済みチェックアウトで ./update.sh だけを実行する (モデルの取得は行わない)。
# 取得 (git clone) は「Strata を取得 / 更新」ジョブで行う。update.sh が無ければ何もせず失敗する。
set -euo pipefail
DIR="${STRATA_DIR:-$HOME/.llm-harness/strata}"
if [ ! -f "$DIR/update.sh" ]; then
  echo "$DIR/update.sh が見つかりません。先に『Strata を取得 / 更新』を実行してください" >&2
  exit 1
fi
cd "$DIR"
echo "==> ./update.sh ($DIR)"
./update.sh
echo "==> Strata の更新が完了しました。次に起動するサーバから新しいコードを使います"
