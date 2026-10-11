#!/usr/bin/env bash
# LLM Harness launcher (Rust バックエンド + 埋め込み React UI の単一バイナリ)
#   ./harness.sh install   UI をビルドして release バイナリを作る (target-cpu=native)
#   ./harness.sh [start]   起動 (http://localhost:8787)
#   ./harness.sh dev       開発モード (cargo run + Vite HMR :5173)
#   ./harness.sh service   systemd --user に登録して WSL 起動時に自動起動
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
export PORT="${PORT:-8787}"
export HOST="${HOST:-127.0.0.1}"  # LAN から使うなら HOST=0.0.0.0 ./harness.sh
BIN="$ROOT/backend/target/release/llm-harness"
export PATH="$HOME/.cargo/bin:$PATH"

ensure_rust() {
  command -v cargo >/dev/null && return
  echo "==> Rust toolchain をインストール"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  export PATH="$HOME/.cargo/bin:$PATH"
}

install() {
  ensure_rust
  echo "==> frontend build"
  (cd "$ROOT/frontend" && npm ci && npm run build)
  echo "==> backend build (release, LTO, target-cpu=native)"
  # native: このマシン (Zen3 など) の命令セットで最適化。別マシンに配る場合は外す
  (cd "$ROOT/backend" && RUSTFLAGS="${RUSTFLAGS:--C target-cpu=native}" cargo build --release)
  echo "==> 完了: $BIN"
}

start() {
  [ -x "$BIN" ] || install
  exec "$BIN" --host "$HOST" --port "$PORT"
}

dev() {
  ensure_rust
  (cd "$ROOT/backend" && cargo run -- --host 127.0.0.1 --port "$PORT") &
  trap 'kill $!' EXIT
  cd "$ROOT/frontend" && npm run dev -- --host
}

case "${1:-start}" in
  install) install ;;
  start) start ;;
  dev) dev ;;
  service) [ -x "$BIN" ] || install; bash "$ROOT/scripts/wsl/install-service.sh" ;;
  *) sed -n '2,7p' "$0"; exit 1 ;;
esac
