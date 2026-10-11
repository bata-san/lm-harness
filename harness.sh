#!/usr/bin/env bash
# LLM Harness launcher
#   ./harness.sh install   依存関係を入れて UI をビルド
#   ./harness.sh [start]   サーバ起動 (http://localhost:8787)
#   ./harness.sh dev       開発モード (API --reload + Vite HMR :5173)
#   ./harness.sh service   systemd --user に登録して WSL 起動時に自動起動
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
PORT="${PORT:-8787}"
HOST="${HOST:-127.0.0.1}"  # LAN から使うなら HOST=0.0.0.0 ./harness.sh
VENV="$ROOT/backend/.venv"

install() {
  echo "==> backend venv"
  if command -v uv >/dev/null; then
    [ -d "$VENV" ] || uv venv "$VENV"
    VIRTUAL_ENV="$VENV" uv pip install -r "$ROOT/backend/requirements.txt"
  else
    [ -d "$VENV" ] || python3 -m venv "$VENV"
    "$VENV/bin/pip" install -q -U pip
    "$VENV/bin/pip" install -q -r "$ROOT/backend/requirements.txt"
  fi
  echo "==> frontend build"
  (cd "$ROOT/frontend" && npm ci && npm run build)
  echo "==> 完了。./harness.sh で起動"
}

start() {
  [ -x "$VENV/bin/python" ] || install
  [ -f "$ROOT/frontend/dist/index.html" ] || (cd "$ROOT/frontend" && npm ci && npm run build)
  cd "$ROOT/backend"
  exec "$VENV/bin/python" -m harness --host "$HOST" --port "$PORT"
}

dev() {
  cd "$ROOT/backend"
  "$VENV/bin/python" -m harness --host 127.0.0.1 --port "$PORT" --reload &
  trap 'kill $!' EXIT
  cd "$ROOT/frontend" && npm run dev -- --host
}

case "${1:-start}" in
  install) install ;;
  start) start ;;
  dev) dev ;;
  service) bash "$ROOT/scripts/wsl/install-service.sh" ;;
  *) sed -n '2,6p' "$0"; exit 1 ;;
esac
