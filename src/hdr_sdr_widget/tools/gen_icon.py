#!/usr/bin/env python3
"""生成 HDR SDR 控制器应用图标源 PNG（1024x1024 RGBA）。

纯 Python 标准库实现（zlib + struct），零第三方依赖。
图形：深色圆角方块底 + 暖黄太阳圆 + 8 条光芒线 —— 单色扁平风格。
用法: python gen_icon.py <输出路径>
"""
import struct
import sys
import zlib

W = H = 1024
CORNER = 220          # 圆角半径
BG = (22, 22, 28, 255)        # 深色底
SUN = (255, 204, 128, 255)    # 暖黄太阳
MARGIN = 80                   # 底色外扩边距


def in_rounded_rect(x, y):
    """判断 (x,y) 是否在圆角矩形内。"""
    if MARGIN <= x < W - MARGIN or MARGIN <= y < H - MARGIN:
        # 中心区域或直边区域：检查四个角的圆弧
        pass
    # 四角圆弧判定
    corners = [
        (MARGIN + CORNER, MARGIN + CORNER, -1, -1),
        (W - MARGIN - CORNER, MARGIN + CORNER, 1, -1),
        (MARGIN + CORNER, H - MARGIN - CORNER, -1, 1),
        (W - MARGIN - CORNER, H - MARGIN - CORNER, 1, 1),
    ]
    for cx, cy, sx, sy in corners:
        # 位于该角所属象限
        if (x - cx) * sx >= 0 and (y - cy) * sy >= 0:
            dx, dy = x - cx, y - cy
            if dx * dx + dy * dy > CORNER * CORNER:
                return False
    return True


def dist_point_segment(px, py, ax, ay, bx, by):
    """点到线段距离的平方。"""
    vx, vy = bx - ax, by - ay
    wx, wy = px - ax, py - ay
    c1 = vx * wx + vy * wy
    if c1 <= 0:
        return wx * wx + wy * wy
    c2 = vx * vx + vy * vy
    if c2 <= c1:
        return (px - bx) ** 2 + (py - by) ** 2
    t = c1 / c2
    dx, dy = px - (ax + t * vx), py - (ay + t * vy)
    return dx * dx + dy * dy


def pixel(x, y):
    if not in_rounded_rect(x, y):
        return (0, 0, 0, 0)
    cx = cy = W // 2
    # 太阳圆
    if (x - cx) ** 2 + (y - cy) ** 2 <= 140 ** 2:
        return SUN
    # 8 条光芒线（线宽 24，从半径 210 到 300）
    half = 12
    for i in range(8):
        a = i * 3.141592653589793 / 4
        x1 = cx + 210 * __import__('math').cos(a)
        y1 = cy + 210 * __import__('math').sin(a)
        x2 = cx + 300 * __import__('math').cos(a)
        y2 = cy + 300 * __import__('math').sin(a)
        if dist_point_segment(x, y, x1, y1, x2, y2) <= half * half:
            return SUN
    return BG


def write_png(path):
    rows = []
    for y in range(H):
        row = bytearray([0])
        for x in range(W):
            r, g, b, a = pixel(x, y)
            row += bytes((r, g, b, a))
        rows.append(bytes(row))
    raw = b"".join(rows)

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", W, H, 8, 6, 0, 0, 0)  # 8bit RGBA
    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", ihdr)
    png += chunk(b"IDAT", zlib.compress(raw, 9))
    png += chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)
    print(f"OK: {path} ({len(png)} bytes)")


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else "app-icon-source.png"
    write_png(out)
