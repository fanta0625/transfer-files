#!/usr/bin/env bash
# Native Debian/Loongnix build. No root privileges needed.
set -euo pipefail
umask 022
ROOT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ "${1:-}" == --help ]]; then
    echo "用法：bash scripts/build-deb.sh [--binary 已编译的本机程序]"
    echo "默认构建 Release；输出到 target/debian/。"
    exit 0
fi
BINARY=""
if [[ $# == 2 && "$1" == --binary ]]; then
    BINARY="$(realpath -- "$2")"
elif [[ $# != 0 ]]; then
    echo "参数错误，请使用 --help" >&2
    exit 1
fi
for tool in cargo rustc python3 dpkg dpkg-deb dpkg-shlibdeps install; do
    command -v "$tool" >/dev/null || { echo "缺少命令：$tool（打包工具可安装 dpkg-dev）" >&2; exit 1; }
done
cd "$ROOT_DIR"
METADATA="$(cargo metadata --no-deps --format-version 1 --locked)"
VERSION="$(python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "transfer-files"))' <<< "$METADATA")"
ARCH="$(dpkg --print-architecture)"
if [[ -z "$BINARY" ]]; then
    TARGET="$(rustc -vV | sed -n 's/^host: //p')"
    TARGET_DIR="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])' <<< "$METADATA")"
    cargo build --release --locked --target "$TARGET" --bin transfer-files
    BINARY="$TARGET_DIR/$TARGET/release/transfer-files"
fi
[[ -x "$BINARY" ]] || { echo "找不到可执行程序：$BINARY" >&2; exit 1; }
OUTPUT_DIR="$ROOT_DIR/target/debian"
mkdir -p "$OUTPUT_DIR"
STAGE="$(mktemp -d /var/tmp/transfer-files-deb.XXXXXX)"
trap 'rm -rf -- "$STAGE"' EXIT
PACKAGE="$STAGE/package"
install -Dm755 "$BINARY" "$PACKAGE/usr/lib/transfer-files/transfer-files"
mkdir -p "$PACKAGE/usr/bin" "$PACKAGE/DEBIAN" "$STAGE/debian"
cat > "$PACKAGE/usr/bin/transfer-files" <<'EOF'
#!/bin/sh
export WINIT_UNIX_BACKEND="${WINIT_UNIX_BACKEND:-x11}"
exec /usr/lib/transfer-files/transfer-files "$@"
EOF
chmod 755 "$PACKAGE/usr/bin/transfer-files"
install -Dm644 packaging/transfer-files.desktop "$PACKAGE/usr/share/applications/transfer-files.desktop"
# Resolve linked library versions on the build machine. Slint also loads these
# desktop libraries dynamically, so they and OpenSSH are listed explicitly.
cat > "$STAGE/debian/control" <<'EOF'
Source: transfer-files
Section: utils
Priority: optional
Maintainer: Transfer Files contributors <noreply@example.com>

Package: transfer-files
Architecture: any
Description: SSH file transfer client
EOF
DEPENDS="$(cd "$STAGE" && dpkg-shlibdeps -O -e"$PACKAGE/usr/lib/transfer-files/transfer-files")"
DEPENDS="${DEPENDS#shlibs:Depends=}"
[[ -n "$DEPENDS" ]] || { echo "无法检测运行依赖" >&2; exit 1; }
SIZE="$(du -sk "$PACKAGE/usr" | cut -f1)"
cat > "$PACKAGE/DEBIAN/control" <<EOF
Package: transfer-files
Version: $VERSION
Section: utils
Priority: optional
Architecture: $ARCH
Maintainer: Transfer Files contributors <noreply@example.com>
Installed-Size: $SIZE
Depends: $DEPENDS, python3, openssh-client, libx11-6, libxcb1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0
Description: GUI file transfer client over SSH
 Browse Windows directories and transfer files through optional SSH hops.
EOF
DEB="$OUTPUT_DIR/transfer-files_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$PACKAGE" "$DEB"
echo "打包完成：$DEB"
echo "安装：sudo apt install \"$DEB\""
