"""Minimal GGUF header reader.

Only the metadata key/value section is parsed (no tensors). Large arrays such as
the tokenizer vocabulary are skipped without being materialised.
"""

from __future__ import annotations

import struct
from pathlib import Path
from typing import Any, BinaryIO

_SCALARS = {
    0: "<B",  # uint8
    1: "<b",  # int8
    2: "<H",  # uint16
    3: "<h",  # int16
    4: "<I",  # uint32
    5: "<i",  # int32
    6: "<f",  # float32
    7: "<?",  # bool
    10: "<Q",  # uint64
    11: "<q",  # int64
    12: "<d",  # float64
}
_STRING, _ARRAY = 8, 9

# llama.cpp general.file_type -> human name
FILE_TYPES = {
    0: "F32", 1: "F16", 2: "Q4_0", 3: "Q4_1", 7: "Q8_0", 8: "Q5_0", 9: "Q5_1",
    10: "Q2_K", 11: "Q3_K_S", 12: "Q3_K_M", 13: "Q3_K_L", 14: "Q4_K_S", 15: "Q4_K_M",
    16: "Q5_K_S", 17: "Q5_K_M", 18: "Q6_K", 19: "IQ2_XXS", 20: "IQ2_XS", 21: "Q2_K_S",
    22: "IQ3_XS", 23: "IQ3_XXS", 24: "IQ1_S", 25: "IQ4_NL", 26: "IQ3_S", 27: "IQ3_M",
    28: "IQ2_S", 29: "IQ2_M", 30: "IQ4_XS", 31: "IQ1_M", 32: "BF16", 36: "TQ1_0",
    37: "TQ2_0", 38: "MXFP4",
}


def _read(f: BinaryIO, fmt: str) -> Any:
    size = struct.calcsize(fmt)
    data = f.read(size)
    if len(data) != size:
        raise EOFError
    return struct.unpack(fmt, data)[0]


def _read_str(f: BinaryIO) -> str:
    n = _read(f, "<Q")
    return f.read(n).decode("utf-8", errors="replace")


def _read_value(f: BinaryIO, vtype: int, keep_arrays: bool) -> Any:
    if vtype in _SCALARS:
        return _read(f, _SCALARS[vtype])
    if vtype == _STRING:
        return _read_str(f)
    if vtype == _ARRAY:
        itype = _read(f, "<I")
        n = _read(f, "<Q")
        if itype in _SCALARS and not keep_arrays:
            f.seek(struct.calcsize(_SCALARS[itype]) * n, 1)
            return f"<array {n}>"
        if itype == _STRING and not keep_arrays:
            for _ in range(n):
                f.seek(_read(f, "<Q"), 1)
            return f"<array {n}>"
        return [_read_value(f, itype, keep_arrays) for _ in range(n)]
    raise ValueError(f"unknown gguf type {vtype}")


def read_metadata(path: str | Path) -> dict[str, Any]:
    with open(path, "rb") as f:
        if f.read(4) != b"GGUF":
            raise ValueError("not a GGUF file")
        version = _read(f, "<I")
        n_tensors = _read(f, "<Q")
        n_kv = _read(f, "<Q")
        meta: dict[str, Any] = {"gguf.version": version, "gguf.tensor_count": n_tensors}
        for _ in range(n_kv):
            key = _read_str(f)
            vtype = _read(f, "<I")
            # head_count_kv can legitimately be a per-layer array; keep small int arrays
            meta[key] = _read_value(f, vtype, keep_arrays=key.endswith("head_count_kv"))
        return meta


def summarize(meta: dict[str, Any]) -> dict[str, Any]:
    arch = meta.get("general.architecture", "unknown")

    def a(key: str, default: Any = None) -> Any:
        return meta.get(f"{arch}.{key}", default)

    head_kv = a("attention.head_count_kv")
    if isinstance(head_kv, list):
        head_kv = max(head_kv) if head_kv else None
    n_head = a("attention.head_count")
    n_embd = a("embedding_length")
    key_len = a("attention.key_length") or (n_embd // n_head if n_embd and n_head else None)
    val_len = a("attention.value_length") or key_len
    ft = meta.get("general.file_type")
    return {
        "architecture": arch,
        "name": meta.get("general.name"),
        "size_label": meta.get("general.size_label"),
        "file_type": FILE_TYPES.get(ft, str(ft) if ft is not None else None),
        "context_length": a("context_length"),
        "block_count": a("block_count"),
        "embedding_length": n_embd,
        "head_count": n_head,
        "head_count_kv": head_kv,
        "key_length": key_len,
        "value_length": val_len,
        "expert_count": a("expert_count"),
        "expert_used_count": a("expert_used_count"),
        "has_chat_template": "tokenizer.chat_template" in meta,
    }
