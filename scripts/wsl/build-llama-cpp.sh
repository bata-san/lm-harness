#!/usr/bin/env bash
# llama.cpp を CUDA + ネイティブ CPU 最適化でビルド (再実行で git pull & 差分ビルド)。
set -euo pipefail

LLAMA_CPP_DIR="${LLAMA_CPP_DIR:-$HOME/.llm-harness/llama.cpp}"
REF="${LLAMA_CPP_REF:-master}"
export PATH="/usr/local/cuda/bin:$PATH"

if ! command -v nvcc >/dev/null; then
  echo "nvcc が見つかりません。先に scripts/wsl/setup.sh を実行してください" >&2
  exit 1
fi

# GPU の compute capability を自動検出 (RTX A5000 = 8.6, RTX 5000 Ada = 8.9)
SMI=$(command -v nvidia-smi || echo /usr/lib/wsl/lib/nvidia-smi)
ARCH=$("$SMI" --query-gpu=compute_cap --format=csv,noheader 2>/dev/null | head -n1 | tr -d '. ')
ARCH="${CUDA_ARCH:-${ARCH:-86}}"
echo "==> CUDA arch: sm_$ARCH"

if [ ! -d "$LLAMA_CPP_DIR/.git" ]; then
  git clone --depth 1 -b "$REF" https://github.com/ggml-org/llama.cpp "$LLAMA_CPP_DIR"
else
  git -C "$LLAMA_CPP_DIR" fetch --depth 1 origin "$REF"
  git -C "$LLAMA_CPP_DIR" reset --hard FETCH_HEAD
fi
cd "$LLAMA_CPP_DIR"
echo "==> commit $(git rev-parse --short HEAD)"

LAUNCHER=()
command -v ccache >/dev/null && LAUNCHER=(-DCMAKE_C_COMPILER_LAUNCHER=ccache -DCMAKE_CXX_COMPILER_LAUNCHER=ccache -DCMAKE_CUDA_COMPILER_LAUNCHER=ccache)

# GGML_NATIVE      : Zen3 (Threadripper PRO 5000WX) の AVX2/FMA を有効化 → CPU オフロード層が速くなる
# GGML_CUDA_FA_ALL_QUANTS : 量子化 KV キャッシュ (q8_0/q4_0) でも FlashAttention を使える
cmake -B build -G Ninja \
  -DCMAKE_BUILD_TYPE=Release \
  -DGGML_CUDA=ON \
  -DCMAKE_CUDA_ARCHITECTURES="$ARCH" \
  -DGGML_NATIVE=ON \
  -DGGML_CUDA_FA_ALL_QUANTS=ON \
  -DLLAMA_CURL=ON \
  -DLLAMA_BUILD_TESTS=OFF \
  "${LAUNCHER[@]}"

cmake --build build --config Release -j "$(nproc)" \
  --target llama-server llama-cli llama-bench llama-quantize llama-gguf-split

echo
./build/bin/llama-server --version
echo "==> 完了: $LLAMA_CPP_DIR/build/bin/llama-server"
