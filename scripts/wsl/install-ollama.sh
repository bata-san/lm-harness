#!/usr/bin/env bash
# Ollama を導入し、性能寄りの環境変数を systemd に設定する。
set -euo pipefail
curl -fsSL https://ollama.com/install.sh | sh

if command -v systemctl >/dev/null && systemctl list-unit-files | grep -q '^ollama.service'; then
  sudo mkdir -p /etc/systemd/system/ollama.service.d
  sudo tee /etc/systemd/system/ollama.service.d/override.conf >/dev/null <<CONF
[Service]
Environment="OLLAMA_HOST=127.0.0.1:11434"
Environment="OLLAMA_FLASH_ATTENTION=1"
Environment="OLLAMA_KV_CACHE_TYPE=q8_0"
Environment="OLLAMA_NUM_PARALLEL=2"
Environment="OLLAMA_KEEP_ALIVE=30m"
Environment="OLLAMA_MAX_LOADED_MODELS=2"
CONF
  sudo systemctl daemon-reload
  sudo systemctl restart ollama
fi
ollama --version
