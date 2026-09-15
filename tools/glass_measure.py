#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""量胶囊内的折射位移 / 色散 / 液面可读性。

参考 = 测试卡本身（每根竖条的硬边位置是已知的），所以不需要额外的未折射参照。

坐标：页面里 #app 在 (60,60)，.capsule 的 margin=14 → 胶囊 CSS (74,74) 40×200。
截图以 1.75 设备倍率拍摄（350×595），胶囊设备矩形 = (129.5,129.5)-(199.5,479.5)。
"""
from PIL import Image
import sys

CARD = r"D:\Agent\interesting app\tools\glass_testcard.png"
SHOT = sys.argv[1] if len(sys.argv) > 1 else r"D:\Agent\interesting app\.workbuddy\tmp\glass-qa\after-dark.png"
ROWS = [int(x) for x in (sys.argv[2].split(",") if len(sys.argv) > 2 else ["80", "300", "420"])]

card = Image.open(CARD).convert("RGB")
shot = Image.open(SHOT).convert("RGB")
print(f"card={card.size}  shot={shot.size}")

SCALE = 1.75
cx0, cy0 = 74 * SCALE, 74 * SCALE
W, H = card.size
# 四舍五入到整数设备像素（半像素误差 ≤1，测量里按 ±2 容差看）
ix0, iy0 = round(cx0), round(cy0)
cap = shot.crop((ix0, iy0, ix0 + W, iy0 + H))
cap.save(r"D:\Agent\interesting app\.workbuddy\tmp\glass-qa\_capsule_crop.png")


def edges(img, y):
    """返回该行所有「相邻像素亮度跳变 > 40」的 x 位置。"""
    px = img.load()
    out = []
    prev = sum(px[0, y]) / 3.0
    for x in range(1, img.size[0]):
        cur = sum(px[x, y]) / 3.0
        if abs(cur - prev) > 40:
            out.append(x)
        prev = cur
    return out


print(f"\n{'row':>5} {'card edges (ref)':<34} {'capsule edges':<34} {'Δ(最左2条)'}")
for y in ROWS:
    if y >= H:
        continue
    ec = edges(card, y)
    ep = edges(cap, y)
    d = ""
    if len(ec) >= 2 and len(ep) >= 2:
        # 最左两条硬边（折射在左右边缘最强）
        d = f"{ep[0]-ec[0]:+d}, {ep[1]-ec[1]:+d}"
    print(f"{y:>5} {str(ec[:6]):<34} {str(ep[:6]):<34} {d}")


def row_profile(img, y):
    px = img.load()
    return [sum(px[x, y]) / 3.0 for x in range(img.size[0])]


print("\n--- 液面可读性（顶面亮线处的亮度阶跃）---")
for y in ROWS:
    if y >= H - 2:
        continue
    a = sum(row_profile(cap, y)) / W
    b = sum(row_profile(cap, y + 3)) / W
    print(f"  row {y}: 本行均值 {a:6.1f} → +3px 后 {b:6.1f}  Δ={a-b:+6.1f}")

print("\n--- 边缘高光（最外 2px vs 内部 10px 的平均亮度）---")
for y in ROWS:
    if y >= H:
        continue
    prof = row_profile(cap, y)
    outer = (prof[0] + prof[1] + prof[-2] + prof[-1]) / 4
    inner = sum(prof[10:60]) / 50
    print(f"  row {y}: 外缘 {outer:6.1f}  内部 {inner:6.1f}  Δ={outer-inner:+6.1f}")
