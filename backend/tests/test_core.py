import struct

from harness import gguf, tuning

GIB = 1024**3


def _gguf_bytes(kv: dict) -> bytes:
    def s(x: str) -> bytes:
        b = x.encode()
        return struct.pack("<Q", len(b)) + b

    out = b"GGUF" + struct.pack("<IQQ", 3, 0, len(kv))
    for k, v in kv.items():
        out += s(k)
        if isinstance(v, str):
            out += struct.pack("<I", 8) + s(v)
        elif isinstance(v, list):  # string array (tokenizer-like)
            out += struct.pack("<IIQ", 9, 8, len(v)) + b"".join(s(t) for t in v)
        else:
            out += struct.pack("<II", 4, v)
    return out


def test_gguf_reader(tmp_path):
    p = tmp_path / "m.gguf"
    p.write_bytes(
        _gguf_bytes(
            {
                "general.architecture": "qwen3moe",
                "general.file_type": 15,
                "tokenizer.ggml.tokens": ["a", "b", "c"],
                "qwen3moe.block_count": 48,
                "qwen3moe.context_length": 40960,
                "qwen3moe.attention.head_count": 32,
                "qwen3moe.attention.head_count_kv": 4,
                "qwen3moe.embedding_length": 2048,
                "qwen3moe.attention.key_length": 128,
                "qwen3moe.expert_count": 128,
                "qwen3moe.expert_used_count": 8,
                "tokenizer.chat_template": "{{x}}",
            }
        )
    )
    info = gguf.summarize(gguf.read_metadata(p))
    assert info["architecture"] == "qwen3moe"
    assert info["file_type"] == "Q4_K_M"
    assert info["block_count"] == 48 and info["head_count_kv"] == 4
    assert info["expert_count"] == 128 and info["has_chat_template"]


def _fake_hw(monkeypatch, vram_gb=24, cores=(24, 48)):
    monkeypatch.setattr(tuning.system, "gpus", lambda: [{"memory_free_mb": vram_gb * 1024 - 500, "memory_total_mb": vram_gb * 1024}])
    monkeypatch.setattr(tuning.system, "cpu_topology", lambda: {"model": "x", "physical_cores": cores[0], "logical_cores": cores[1]})


INFO_DENSE = {"block_count": 64, "head_count_kv": 8, "key_length": 128, "value_length": 128, "context_length": 40960, "has_chat_template": True}


def test_fits_fully_on_gpu(monkeypatch):
    _fake_hw(monkeypatch)
    r = tuning.recommend({"size": 9 * GIB, "info": INFO_DENSE}, ctx=16384)
    a = r["args"]
    assert a[a.index("-ngl") + 1] == "999"
    assert a[a.index("-t") + 1] == "4"  # GPU-resident: few CPU threads
    assert "-fa" in a and "--jinja" in a
    assert r["estimate"]["fits"]


def test_dense_partial_offload(monkeypatch):
    _fake_hw(monkeypatch)
    r = tuning.recommend({"size": 40 * GIB, "info": INFO_DENSE}, ctx=8192)
    a = r["args"]
    ngl = int(a[a.index("-ngl") + 1])
    assert 0 < ngl < 64
    assert a[a.index("-t") + 1] == "24"  # physical cores


def test_moe_uses_n_cpu_moe(monkeypatch):
    _fake_hw(monkeypatch)
    info = {**INFO_DENSE, "block_count": 48, "head_count_kv": 4, "expert_count": 128, "expert_used_count": 8}
    r = tuning.recommend({"size": 60 * GIB, "info": info}, ctx=32768)
    a = r["args"]
    assert a[a.index("-ngl") + 1] == "999"
    n = int(a[a.index("--n-cpu-moe") + 1])
    assert 0 < n <= 48


def test_kv_scales_with_parallel():
    one = tuning.kv_cache_bytes(INFO_DENSE, 8192, "f16", 1)
    four = tuning.kv_cache_bytes(INFO_DENSE, 8192, "f16", 4)
    assert four == 4 * one
    # 64 layers * 8192 ctx * 8 kv heads * 256 (k+v dims) * 2 bytes = 2 GiB
    assert one == 2 * GIB
