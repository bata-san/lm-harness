"""Launch-parameter recommendations for llama.cpp based on the detected host.

The goal is "everything on the GPU if it fits, otherwise push the least useful
weights (MoE experts first) to the Threadripper's memory channels".
"""

from __future__ import annotations

from . import system

GIB = 1024**3
# CUDA context + compute buffers + fragmentation headroom
OVERHEAD = int(1.2 * GIB)
KV_BYTES = {"f16": 2.0, "q8_0": 1.0625, "q4_0": 0.5625}


def kv_cache_bytes(info: dict, ctx: int, kv_type: str, parallel: int = 1) -> int:
    layers = info.get("block_count") or 0
    n_kv = info.get("head_count_kv") or info.get("head_count") or 0
    k = info.get("key_length") or 0
    v = info.get("value_length") or k
    # llama-server splits -c across slots, so total cache scales with --parallel
    return int(layers * ctx * max(parallel, 1) * n_kv * (k + v) * KV_BYTES.get(kv_type, 2.0))


def recommend(model: dict, ctx: int | None = None, kv_type: str = "q8_0", parallel: int = 1) -> dict:
    info = model.get("info") or {}
    gpus = system.gpus()
    topo = system.cpu_topology()
    vram_free = int(sum((g["memory_free_mb"] or 0) for g in gpus) * 1024**2)
    vram_total = int(sum((g["memory_total_mb"] or 0) for g in gpus) * 1024**2)
    size = model["size"]
    native_ctx = info.get("context_length") or 8192
    ctx = ctx or min(native_ctx, 32768)
    layers = info.get("block_count") or 0
    moe = bool(info.get("expert_count"))

    kv = kv_cache_bytes(info, ctx, kv_type, parallel)
    budget = vram_free - OVERHEAD - kv
    notes: list[str] = []
    args: list[str] = []
    n_cpu_moe = 0

    if not gpus:
        ngl = 0
        notes.append("GPU 未検出のため CPU 推論。")
    elif size <= budget:
        ngl = 999
        notes.append(f"モデル {size / GIB:.1f} GiB + KV {kv / GIB:.1f} GiB が VRAM 空き {vram_free / GIB:.1f} GiB に収まるため全層 GPU。")
    elif moe and layers:
        # Keep attention/shared weights on GPU, offload expert FFNs of the first N layers.
        ngl = 999
        per_layer = size / layers
        # ~90% of a MoE layer is expert weights
        overflow = size - budget
        n_cpu_moe = min(layers, int(overflow / (per_layer * 0.9)) + 1)
        notes.append(
            f"MoE モデル ({info.get('expert_count')} experts) が VRAM を {overflow / GIB:.1f} GiB 超過。"
            f"先頭 {n_cpu_moe} 層の expert を CPU に置く (--n-cpu-moe)。Attention は GPU のまま。"
        )
    elif layers:
        ngl = max(0, int(layers * budget / size))
        notes.append(f"VRAM 不足のため {ngl}/{layers} 層を GPU にオフロード。残りは CPU (メモリ帯域が律速)。")
    else:
        ngl = 999

    gpu_resident = ngl >= (layers or 999) and n_cpu_moe == 0 and gpus
    # Fully-offloaded generation is GPU-bound; extra CPU threads only add sync overhead.
    threads = 4 if gpu_resident else topo["physical_cores"]
    threads_batch = topo["physical_cores"] if gpu_resident else topo["logical_cores"]

    args += ["-ngl", str(ngl), "-c", str(ctx * max(parallel, 1)), "-t", str(threads), "-tb", str(threads_batch)]
    if n_cpu_moe:
        args += ["--n-cpu-moe", str(n_cpu_moe)]
    if gpus:
        args += ["-fa", "on"]
        if kv_type != "f16":
            args += ["-ctk", kv_type, "-ctv", kv_type]
    args += ["-np", str(parallel), "-b", "4096" if gpu_resident else "2048", "-ub", "1024" if gpu_resident else "512"]
    if info.get("has_chat_template"):
        args += ["--jinja"]
    args += ["--metrics"]
    if not gpu_resident:
        notes.append(f"CPU スレッドは物理コア数 {topo['physical_cores']} に合わせる (SMT は生成速度を上げない)。")
    if model.get("mmproj"):
        args += ["--mmproj", model["mmproj"]]
        notes.append("mmproj (ビジョン) を検出したので付与。")
    if ctx > native_ctx:
        notes.append(f"注意: ネイティブ context {native_ctx} を超えています (RoPE scaling が必要)。")

    return {
        "args": args,
        "notes": notes,
        "estimate": {
            "model_bytes": size,
            "kv_bytes": kv,
            "vram_free": vram_free,
            "vram_total": vram_total,
            "fits": bool(gpus) and size <= budget,
            "ctx": ctx,
            "native_ctx": native_ctx,
        },
    }
