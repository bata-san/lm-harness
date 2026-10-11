#!/usr/bin/env bash
# ハーネス自体を systemd --user サービスとして登録 (WSL 起動時に自動起動)。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$HOME/.config/systemd/user"
cat > "$HOME/.config/systemd/user/llm-harness.service" <<UNIT
[Unit]
Description=LLM Harness
After=network-online.target

[Service]
WorkingDirectory=$ROOT
ExecStart=$ROOT/backend/target/release/llm-harness --host 127.0.0.1 --port 8787
Environment=HARNESS_REPO=$ROOT
Restart=on-failure
LimitMEMLOCK=infinity
Environment=PATH=/usr/local/cuda/bin:/usr/lib/wsl/lib:/usr/local/bin:/usr/bin:/bin

[Install]
WantedBy=default.target
UNIT
systemctl --user daemon-reload
systemctl --user enable --now llm-harness
# ログインしていなくても起動させる
sudo loginctl enable-linger "$USER" || true
systemctl --user status llm-harness --no-pager | head -n 5
