#!/usr/bin/env bash
# WSL2 (Ubuntu 22.04/24.04) の基本セットアップ。sudo が必要なのでターミナルで実行:
#   bash scripts/wsl/setup.sh
# 重要: WSL 内に Linux 用 NVIDIA ドライバを入れてはいけない (Windows 側ドライバが /usr/lib/wsl/lib に投影される)。
set -euo pipefail

CUDA_PKG="${CUDA_PKG:-cuda-toolkit-12-9}"
. /etc/os-release

echo "==> apt packages"
sudo apt-get update
sudo apt-get install -y build-essential cmake ninja-build ccache git git-lfs curl wget pkg-config \
  libcurl4-openssl-dev libssl-dev python3 python3-venv python3-dev htop nvtop jq

echo "==> CUDA Toolkit for WSL ($CUDA_PKG)"
if ! dpkg -s cuda-keyring >/dev/null 2>&1; then
  tmp=$(mktemp -d)
  wget -qO "$tmp/cuda-keyring.deb" https://developer.download.nvidia.com/compute/cuda/repos/wsl-ubuntu/x86_64/cuda-keyring_1.1-1_all.deb
  sudo dpkg -i "$tmp/cuda-keyring.deb"
  rm -rf "$tmp"
  sudo apt-get update
fi
# wsl-ubuntu リポジトリの toolkit はドライバを含まないので安全
sudo apt-get install -y "$CUDA_PKG"
if ! grep -q '/usr/local/cuda/bin' "$HOME/.bashrc"; then
  cat >> "$HOME/.bashrc" <<'RC'
export PATH=/usr/local/cuda/bin:$PATH
export LD_LIBRARY_PATH=/usr/lib/wsl/lib:/usr/local/cuda/lib64:${LD_LIBRARY_PATH:-}
RC
fi

echo "==> uv (Python パッケージマネージャ)"
command -v uv >/dev/null || curl -LsSf https://astral.sh/uv/install.sh | sh

echo "==> Node.js 22 (フロントエンドのビルド用)"
if ! command -v node >/dev/null || [ "$(node -v | cut -d. -f1 | tr -d v)" -lt 20 ]; then
  curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
  sudo apt-get install -y nodejs
fi

echo "==> systemd を有効化 (ollama / harness のサービス化に必要)"
if ! grep -q 'systemd=true' /etc/wsl.conf 2>/dev/null; then
  printf '[boot]\nsystemd=true\n\n[interop]\nappendWindowsPath=false\n' | sudo tee /etc/wsl.conf >/dev/null
  echo "   /etc/wsl.conf を更新。Windows 側で 'wsl --shutdown' してから再起動してください"
fi

echo "==> mlock 上限を解除 (llama.cpp --mlock でモデルをスワップさせない)"
echo -e "*  soft  memlock  unlimited\n*  hard  memlock  unlimited" | sudo tee /etc/security/limits.d/90-llm.conf >/dev/null

echo "==> スワップしにくくする"
echo "vm.swappiness=10" | sudo tee /etc/sysctl.d/90-llm.conf >/dev/null
sudo sysctl -p /etc/sysctl.d/90-llm.conf >/dev/null || true

mkdir -p "$HOME/models"
echo
echo "完了。確認:"
echo "  nvidia-smi                # Windows ドライバ経由で GPU が見えるか"
echo "  /usr/local/cuda/bin/nvcc --version"
