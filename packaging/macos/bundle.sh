#!/usr/bin/env bash
# 把 cargo 产出的裸二进制打包成 macOS 标准应用：DevUp.app + .dmg + .app.tar.gz
#
#   bash packaging/macos/bundle.sh \
#     --binary target/aarch64-apple-darwin/release/devup \
#     --version 0.2.0 --arch arm64 --out dist
#
# 产物（供 GitHub Release 直接上传）：
#   dist/DevUp.app                      可直接双击的应用包
#   dist/DevUp_0.2.0_arm64.dmg          人用的安装镜像（拖到 Applications）
#   dist/DevUp_0.2.0_arm64.app.tar.gz   应用内「检查更新」用的整包
#
# 默认做 ad-hoc 签名（避免「已损坏，无法打开」）；传入 --identity 走 Developer ID
# + hardened runtime，配合 --notarize 可再走 notarytool 公证。
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

binary=""
version=""
arch_label=""
out_dir=""
identity="${APPLE_SIGNING_IDENTITY:-}"
notarize=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --binary)   binary="$2"; shift 2 ;;
    --version)  version="$2"; shift 2 ;;
    --arch)     arch_label="$2"; shift 2 ;;
    --out)      out_dir="$2"; shift 2 ;;
    --identity) identity="$2"; shift 2 ;;
    --notarize) notarize=true; shift ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

[[ -n "$binary" && -f "$binary" ]] || { echo "❌ --binary 指定的可执行文件不存在" >&2; exit 2; }
[[ -n "$version" ]] || { echo "❌ 缺少 --version" >&2; exit 2; }
[[ -n "$out_dir" ]] || { echo "❌ 缺少 --out" >&2; exit 2; }
if [[ -z "$arch_label" ]]; then
  arch_label="$(uname -m)"
  [[ "$arch_label" == "arm64" ]] || arch_label="x64"
fi

mkdir -p "$out_dir"
out_dir="$(cd "$out_dir" && pwd)"

app="$out_dir/DevUp.app"
contents="$app/Contents"
macos_dir="$contents/MacOS"
res_dir="$contents/Resources"

echo "==> 组装 bundle ($arch_label, v$version)"
rm -rf "$app"
mkdir -p "$macos_dir" "$res_dir"

sed "s/__VERSION__/$version/g" "$here/Info.plist" > "$contents/Info.plist"
plutil -lint "$contents/Info.plist" >/dev/null

install -m 755 "$binary" "$macos_dir/devup"
install -m 644 "$here/AppIcon.icns" "$res_dir/AppIcon.icns"
printf 'APPL????' > "$contents/PkgInfo"

if [[ -n "$identity" && "$identity" != "-" ]]; then
  echo "==> Developer ID 签名 (hardened runtime)"
  codesign --force --options runtime --timestamp \
    --entitlements "$here/entitlements.plist" \
    --sign "$identity" "$app"
else
  echo "==> ad-hoc 签名（无证书，首次打开需右键 → 打开放行一次）"
  codesign --force --sign - "$app"
fi
codesign --verify --verbose=2 "$app"

if [[ "$notarize" == true && -n "$identity" && "$identity" != "-" ]]; then
  echo "==> 公证 stapling"
  staple_app() {
    local target="$1"
    xcrun notarytool submit "$target" --keychain-profile devup-notary --wait
    xcrun stapler staple "$target"
  }
  staple_app "$app"
fi

base="DevUp_${version}_${arch_label}"

echo "==> 打包 $base.app.tar.gz（自升级用）"
# COPYFILE_DISABLE=1 阻止 bsdtar 写入 ._* AppleDouble 条目，否则解包后会污染 bundle
( cd "$out_dir" && COPYFILE_DISABLE=1 tar -czf "$out_dir/$base.app.tar.gz" --no-xattrs DevUp.app )

echo "==> 制作 $base.dmg"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
cp -R "$app" "$stage/DevUp.app"
ln -s /Applications "$stage/Applications"
hdiutil create -quiet -volname DevUp -srcfolder "$stage" -ov -format UDZO "$out_dir/$base.dmg"
# DMG 同样签名，避免「已损坏」；ad-hoc 身份不影响内容签名
if [[ -n "$identity" && "$identity" != "-" ]]; then
  codesign --force --timestamp --sign "$identity" "$out_dir/$base.dmg"
else
  codesign --force --sign - "$out_dir/$base.dmg"
fi

echo
echo "✅ 产物："
ls -la "$out_dir" | awk -v app="DevUp.app" '$NF==app || $NF ~ /^DevUp_/ {print "   " $NF}'
