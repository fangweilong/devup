#!/usr/bin/env bash
# 由 packaging/macos/icon.svg 生成 AppIcon.icns（本地产物需提交进仓库，CI 直接复用）。
#
#   bash packaging/macos/gen-icon.sh
#
# 渲染必须用 rsvg-convert（brew install librsvg）：macOS 自带的 QuickLook 会把
# SVG 的透明背景拍平成纯白，四角变成不透明，Dock 里就是一圈白边（已踩过）。
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
src="$here/icon.svg"
out_icns="$here/AppIcon.icns"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

png="$work/icon_1024.png"
command -v rsvg-convert >/dev/null 2>&1 || {
  echo "❌ 需要 rsvg-convert（保留 alpha 的 SVG 渲染器）：brew install librsvg" >&2
  exit 1
}
rsvg-convert -w 1024 -h 1024 -o "$png" "$src"

sips -g pixelWidth -g pixelHeight "$png" | grep -q '1024' || {
  echo "❌ 1024px 母图渲染异常: $png" >&2
  exit 1
}
python3 "$here/check-alpha.py" "$png"

iconset="$work/AppIcon.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done

iconutil -c icns "$iconset" -o "$out_icns"
echo "✅ $out_icns ($(du -h "$out_icns" | cut -f1 | tr -d ' '))"
