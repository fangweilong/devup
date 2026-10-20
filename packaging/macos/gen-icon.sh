#!/usr/bin/env bash
# 由 packaging/macos/icon.svg 生成 AppIcon.icns（本地产物需提交进仓库，CI 直接复用）。
#
#   bash packaging/macos/gen-icon.sh
#
# 光栅化优先用 rsvg-convert（brew install librsvg），没有则退回 macOS 自带的 QuickLook。
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
src="$here/icon.svg"
out_icns="$here/AppIcon.icns"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

png="$work/icon_1024.png"
if command -v rsvg-convert >/dev/null 2>&1; then
  rsvg-convert -w 1024 -h 1024 -o "$png" "$src"
else
  # QuickLook 渲染：要求 SVG 自身声明 width/height=1024，否则会被缩放到画布一角。
  qlmanage -t -s 1024 -o "$work" "$src" >/dev/null
  mv "$work/icon.png" "$png" 2>/dev/null || mv "$work/$(basename "$src").png" "$png"
fi

sips -g pixelWidth -g pixelHeight "$png" | grep -q '1024' || {
  echo "❌ 1024px 母图渲染异常: $png" >&2
  exit 1
}

iconset="$work/AppIcon.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done

iconutil -c icns "$iconset" -o "$out_icns"
echo "✅ $out_icns ($(du -h "$out_icns" | cut -f1 | tr -d ' '))"
