#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
BINARY="$ROOT_DIR/target/release/transfer-files"

if [[ ! -x "$BINARY" ]]; then
    echo "首次启动，正在构建 Release 版本……" >&2
    cargo build --release --locked --manifest-path "$ROOT_DIR/Cargo.toml"
fi

exec env WINIT_UNIX_BACKEND="${WINIT_UNIX_BACKEND:-x11}" "$BINARY" "$@"
