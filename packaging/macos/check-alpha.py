#!/usr/bin/env python3
"""校验 PNG 四角是透明的（stdlib only，无需 Pillow）。

macOS 应用图标必须是「圆角方形 + 四角透明」。用 QuickLook 之类会把
SVG 透明背景拍平成白色的渲染器时，四角会变成不透明纯白，Dock 里就是
一圈白边 —— 这个检查就是为了在生成 .icns 之前把这类渲染器挡在门外。

    python3 packaging/macos/check-alpha.py <png> [边距]

只适合校验 1024 母图（或 ≥128 的尺寸）：小图标的圆角半径只有几个像素，
按默认边距取样会落进图形内部，属于正常不透明，不是背景被拍平。
"""
import struct
import sys
import zlib


def decode_png(path):
    data = open(path, 'rb').read()
    if data[:8] != b'\x89PNG\r\n\x1a\n':
        raise ValueError('不是 PNG 文件')
    pos, idat = 8, b''
    w = h = ch = None
    while pos < len(data):
        length, kind = struct.unpack('>I4s', data[pos:pos + 8])
        chunk = data[pos + 8:pos + 8 + length]
        if kind == b'IHDR':
            w, h, depth, color, _comp, _filt, interlace = struct.unpack('>IIBBBBB', chunk)
            if depth != 8 or interlace != 0 or color not in (2, 6):
                raise ValueError(f'只支持非隔行 8bit RGB/RGBA，实际 depth={depth} color={color} interlace={interlace}')
            ch = 4 if color == 6 else 3
        elif kind == b'IDAT':
            idat += chunk
        elif kind == b'IEND':
            break
        pos += 12 + length

    raw = zlib.decompress(idat)
    stride = w * ch
    rows = []
    prev = bytearray(stride)
    cursor = 0

    def paeth(a, b, c):
        p_a, p_b, p_c = abs(b - c), abs(a - c), abs(a + b - 2 * c)
        return a if (p_a <= p_b and p_a <= p_c) else (b if p_b <= p_c else c)

    for _ in range(h):
        filt = raw[cursor]
        cursor += 1
        line = bytearray(raw[cursor:cursor + stride])
        cursor += stride
        for i in range(stride):
            a = line[i - ch] if i >= ch else 0
            b = prev[i]
            c = prev[i - ch] if i >= ch else 0
            if filt == 1:
                line[i] = (line[i] + a) & 255
            elif filt == 2:
                line[i] = (line[i] + b) & 255
            elif filt == 3:
                line[i] = (line[i] + ((a + b) >> 1)) & 255
            elif filt == 4:
                line[i] = (line[i] + paeth(a, b, c)) & 255
        rows.append(line)
        prev = line
    return w, h, ch, rows


def main():
    if len(sys.argv) < 2:
        print('用法: check-alpha.py <png> [边距]', file=sys.stderr)
        return 2
    path = sys.argv[1]
    inset = int(sys.argv[2]) if len(sys.argv) > 2 else 3
    w, h, ch, rows = decode_png(path)

    corners = {
        '左上': (inset, inset),
        '右上': (w - 1 - inset, inset),
        '左下': (inset, h - 1 - inset),
        '右下': (w - 1 - inset, h - 1 - inset),
    }
    samples = {}
    for name, (x, y) in corners.items():
        o = x * ch
        px = rows[y][o:o + ch]
        alpha = px[3] if ch == 4 else 255
        samples[name] = (px[0], px[1], px[2], alpha)

    print(f'{w}x{h}  四角取样: ' + '  '.join(f'{k}=rgba{v}' for k, v in samples.items()))
    opaque = [k for k, (_, _, _, a) in samples.items() if a > 8]
    if ch == 3:
        print('❌ PNG 没有 alpha 通道，图标会有白边', file=sys.stderr)
        return 1
    if opaque:
        print(f'❌ 四角不透明（{", ".join(opaque)}），背景被渲染器拍平了；换用 rsvg-convert 重新生成', file=sys.stderr)
        return 1
    print('✅ 四角透明，符合 macOS 图标要求')
    return 0


if __name__ == '__main__':
    sys.exit(main())
