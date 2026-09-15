#!/usr/bin/env python3
r"""审计设计令牌：找出 `tokens.css` 里**声明了却无人使用**的令牌。

# 为什么需要它

本项目已经因为"死令牌"栽过两次：

1. `--glass-blur` 声明了很久（注释写着"磨砂模糊"），但 `readOptics` 没读、
   shader 也没用 —— **磨砂从来没被实现过**，直到用户反馈"加一点毛玻璃"才查到。
2. `--glass-rim-gain` 被改名成 `--glass-spec-gain` 后，旧名字的处理逻辑残留，
   一度出现"令牌改了、代码没改"的静默失效。

死令牌的危害不是"多几行没人读的 CSS"，而是**它对后来者是谎言**：
看到 `--glass-blur: 0.6px` 会以为磨砂生效了、调它也会有反应。
所以这份审计要定期跑，让"声明"与"生效"对齐。

# 用法

    python tools/audit_tokens.py            # 只报告
    python tools/audit_tokens.py --strict   # 有死令牌时以非零码退出（可挂 CI）

# 注意（踩过的坑）

**不能把 `tokens.css` 整个文件排除在搜索之外** —— `--font-ui` / `--window-w`
这类令牌正是在该文件自己的规则块里被使用的（`:root` / `#app`），
一刀切排除会产生假阳性。正确做法是**只剔除声明行**（`^\s*--name:`），
保留规则块。
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# 工具位于 <工作区>/tools/，项目根是 <工作区>/src/hdr_sdr_widget/。
ROOT = Path(__file__).resolve().parent.parent
PROJECT = ROOT / "src" / "hdr_sdr_widget"
TOKENS = PROJECT / "src" / "styles" / "tokens.css"
SCAN_SUFFIXES = {".css", ".ts", ".html"}
DECL = re.compile(r"^\s*(--[a-z0-9-]+)\s*:", re.M)
DECL_LINE = re.compile(r"^\s*--[a-z0-9-]+\s*:")


def main() -> int:
    if not TOKENS.exists():
        print(f"找不到 {TOKENS}")
        return 2

    raw = TOKENS.read_text(encoding="utf-8")
    names = sorted(set(DECL.findall(raw)))

    # 只剔除**声明行**，保留 tokens.css 里规则块中的使用点。
    stripped = "\n".join(l for l in raw.splitlines() if not DECL_LINE.match(l))
    others = [
        p
        for p in (PROJECT / "src").rglob("*")
        if p.suffix in SCAN_SUFFIXES and p != TOKENS
    ]
    blob = stripped + "\n" + "\n".join(
        p.read_text(encoding="utf-8", errors="ignore") for p in others
    )

    dead = [n for n in names if n not in blob]
    print(f"tokens.css 声明 {len(names)} 个令牌；真·无人使用 {len(dead)} 个。")
    if not dead:
        print("✓ 声明与生效一致")
        return 0

    print("\n⚠️ 死令牌（声明了却没有任何使用点）：")
    for d in dead:
        print(f"   {d}")
    print(
        "\n处置二选一：\n"
        "  · 接上它（把硬编码的值换成 var(...)）—— 若它本来就该生效\n"
        "  · 删掉它 —— 若是废弃/改名残留\n"
        "别留着：它对后来者是谎言。"
    )
    return 1 if "--strict" in sys.argv else 0


if __name__ == "__main__":
    raise SystemExit(main())
