#!/usr/bin/env bash
# ハーネス本体の更新: git pull --ff-only → ./harness.sh install (UI + release バイナリを再ビルド)
# 完了後はリモート管理画面の restart-harness (または systemctl --user restart llm-harness) で反映。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
echo "==> git pull --ff-only ($ROOT)"
git pull --ff-only
echo "==> ./harness.sh install"
./harness.sh install
echo "==> 更新完了。ハーネスを再起動すると新しいバイナリで動作します"
