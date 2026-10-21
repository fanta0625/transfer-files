#!/usr/bin/env bash
set -euo pipefail

demo_root=$(mktemp -d "${TMPDIR:-/tmp}/transfer-files-demo.XXXXXX")
mkdir -p "$demo_root/left/资料 空格/空目录" "$demo_root/right" "$demo_root/external"
printf '双向复制测试\n' > "$demo_root/left/中文 文件.txt"
printf '嵌套目录内容\n' > "$demo_root/left/资料 空格/内容.txt"
: > "$demo_root/left/零字节.bin"
printf '从右侧复制到左侧\n' > "$demo_root/right/反向.txt"
printf '从 Dolphin 拖入\n' > "$demo_root/external/拖入 文件.txt"
printf '保留左侧原内容\n' > "$demo_root/left/同名.txt"
printf '保留右侧原内容\n' > "$demo_root/right/同名.txt"
printf '测试目录：%s\n' "$demo_root"
printf '从项目根目录运行：\n'
printf 'cargo run --locked -- %q %q\n' "$demo_root/left" "$demo_root/right"
printf '用 Dolphin 打开 %q，测试外部拖入。\n' "$demo_root/external"
printf '目录会保留供检查，完成后可自行删除该测试目录。\n'
