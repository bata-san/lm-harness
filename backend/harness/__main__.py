import argparse

import uvicorn


def main() -> None:
    ap = argparse.ArgumentParser(prog="llm-harness")
    ap.add_argument("--host", default="127.0.0.1", help="WSL から Windows へは localhost で届く。LAN に公開する場合のみ 0.0.0.0")
    ap.add_argument("--port", type=int, default=8787)
    ap.add_argument("--reload", action="store_true")
    a = ap.parse_args()
    uvicorn.run("harness.main:app", host=a.host, port=a.port, reload=a.reload, log_level="info")


if __name__ == "__main__":
    main()
