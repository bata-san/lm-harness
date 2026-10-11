#!/usr/bin/env bash
# vLLM を専用 venv に導入 (torch の CUDA 版は uv が自動選択)。
set -euo pipefail
VLLM_VENV="${VLLM_VENV:-$HOME/.llm-harness/venvs/vllm}"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"

if ! command -v uv >/dev/null; then
  curl -LsSf https://astral.sh/uv/install.sh | sh
  export PATH="$HOME/.local/bin:$PATH"
fi

mkdir -p "$(dirname "$VLLM_VENV")"
[ -d "$VLLM_VENV" ] || uv venv --python 3.12 --seed "$VLLM_VENV"
VIRTUAL_ENV="$VLLM_VENV" uv pip install -U vllm --torch-backend=auto
"$VLLM_VENV/bin/python" - <<'PY'
import importlib.metadata as m, torch
print("vllm", m.version("vllm"), "| torch", torch.__version__, "| cuda", torch.version.cuda, "| gpu", torch.cuda.is_available() and torch.cuda.get_device_name(0))
PY
