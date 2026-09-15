"""Create a source-backed acceptance report from the native QA run files."""
from pathlib import Path
import csv
import hashlib
import json
import sys

root = Path(sys.argv[1]).resolve()
qa = root / 'qa'
binary_hash = hashlib.sha256((root / 'hdr-sdr-widget-0.4.0.exe').read_bytes()).hexdigest().upper()
runs = {}
for key in ['wgc', 'edge', 'dda', 'clarity']:
    folder = qa / f'acceptance-{key}'
    metadata = json.loads((folder / 'run.json').read_text(encoding='utf-8-sig'))
    if metadata['sha256'] != binary_hash:
        raise ValueError(f'{key}: the measured binary does not match the deliverable')
    runs[key] = json.loads((folder / 'diagnostics.json').read_text(encoding='utf-8-sig'))
with (qa / 'acceptance-wgc/resources.csv').open(encoding='utf-8-sig', newline='') as f:
    resources = [row for row in csv.DictReader(f) if float(row['seconds']) >= 60]
mem = [float(row['privateBytes']) / 1048576 for row in resources]
handles = [int(row['handles']) for row in resources]
clarity = json.loads((qa / 'acceptance-clarity/clarity-metrics.json').read_text(encoding='utf-8-sig'))
table = []
for key, label in [('wgc', 'WGC 连续形变 · 180秒'), ('edge', '侧边展开与反向 · 30秒'), ('dda', '强制 DDA 回退 · 20秒')]:
    d = runs[key]
    table.append(f"| {label} | {d['presentedFps']:.2f} | {d['presentedIntervalP95Ms']:.3f} | {d['submitIntervalP95Ms']:.3f} | {d['captureToSubmitP95Ms']:.3f} | {d['presentationObservationGaps']} |")
edge = runs['edge']
text = f'''# 0.4.0 原生胶囊 · 实施与验收报告

日期：2026-09-15。本机：Windows 11、RTX 3070、3840×2160、约160Hz、175% DPI、HDR开启。

**已交付可运行的原生优化版本和 NSIS 安装包；尚未完成全部跨环境验收或 iOS 1:1 视觉标定。**

## 主要变化

主胶囊改为 Rust / Win32 / D3D11 / DirectComposition。桌面纹理直接进入 GPU 光学处理，主路径不再经过 Base64、JavaScript 逐像素处理和 Canvas 缩放。几何形变与屏幕坐标采样分离；模糊半径保持物理像素口径。WGC 和本机 DDA 回退均取得 FP16 格式10，合成为 scRGB。

统一可中断弹簧保留位置、速度与目标；增加局部按压高光、拖动直接跟随、释放速度衔接、非线性边界阻力、贴边吸附和展开中反向。保留60ms边缘停留、1200ms移出收回规则。亮度工作线程合并最新请求，硬件写入按30ms间隔执行，最终请求确认与空闲回读分开，旧结果不会覆盖新请求。

## 最终交付文件的实测

所有下表运行的 `run.json` 中 SHA256 均与交付可执行文件一致：

`{binary_hash}`

| 场景 | 实际呈现 fps | 已观测显示间隔 P95 ms | 提交间隔 P95 ms | 捕获到提交 P95 ms | 统计观察间断次数 |
|---|---:|---:|---:|---:|---:|
{chr(10).join(table)}

实际帧率来自 DXGI 的 PresentCount / QPC，不是渲染循环次数。逐帧 P95 只统计可直接观察的间隔；存在观察间断时，原始报告会保留 `actualPresentationVerified=false`，没有隐藏间断或补造时间点。运行中的形变帧率接近160Hz；完整端到端验收不能仅凭提交间隔判定。

侧边测试重复完成展开、收回，并在200ms后反向接管同一轨迹；这次运行的背景更新最大间隔为 **{edge['captureUpdateGapMaxMs']:.3f}ms**，未观测到超过50ms的更新冻结。它是应用内部驱动真实边缘状态机的测试，不能替代鼠标停留规则的全部人工回归。

捕获到提交使用捕获帧的系统时间戳；不包含提交后到屏幕实际显示的等待，因此不能直接宣称“捕获到呈现 P95≤25ms”已经完成外部验证。PresentMon 的 ETW 会话因本机权限限制未能启动，本次使用 DXGI 原始统计。

### 资源稳定性

三分钟 WGC 测试中，后两分钟私有内存范围 **{min(mem):.2f}–{max(mem):.2f}MiB**，首尾变化 **{mem[-1]-mem[0]:+.3f}MiB**；句柄数 **{min(handles)}–{max(handles)}**。这段区间未见资源持续增长，不能代替长时间耐久测试。

## 清晰度

![四种形状的 GPU 测试卡](qa/acceptance-clarity/shape-comparison.png)

同一桌面坐标测试卡分别输出原尺寸、悬停放大、按压和拖动状态。中心未折射、未填充区域 ROI 为物理像素 `{clarity['roiPhysicalPixels']}`；四种形状相对原尺寸的平均 RGB 差值均为 **{max(v['meanAbsoluteRgbDelta'] for v in clarity['metrics']):.3f}**。该区域没有因位图缩放而增加模糊。折射边缘与液位材质本来就会变化，不应要求它们逐像素相同。

图片来自显式 GPU 测试卡，不是摄影图。真实桌面外观与拖动截图另见 `qa/desktop-drag-audit.jpg`、`qa/desktop-input-audit.jpg`。

旧版源代码、原始0.3.4程序和已有自检/截图完整保留在原目录。旧版已有证据为 BGRA8 格式87和 Canvas/Base64路径。本次没有取得旧版与新版的完整同背景实拍序列，因此不把不同背景的历史截图当作量化前后对照，也不声称已完成这一项交付。

## 输入与业务正确性

原生窗口实测完成横向拖动、连续20次点击重定向和6次交替纵向拖动；最终系统确认为30%（raw2500）。截图验收模式会冻结背景，因此其输入数据与上述动态捕获性能分开。输入样本统计和启动期耗时见 `qa/input-feedback.json`；该记录对应的候选构建哈希见 `qa/input-build.json`。输入到提交也不等于输入到屏幕光子延迟。

37项主程序测试、42项领域核心测试通过；包括不同刷新率下弹簧轨迹一致、反向保留速度、阻力有界、最新请求合并、旧结果拒收、最终值与系统吸附确认、驱动失败恢复、空闲回读不覆盖最终失败、HLSL编译。失败回读竞态测试另重复10次通过。旧渲染器回滚启动与捕获检查通过，记录见 `qa/final-legacy/diagnostics.json`。

## 尚未完成的验收

- 60/120Hz、其他DPI、HDR切换、多显示器与旋转、真实睡眠恢复、独占全屏、长时间运行的完整实机矩阵。
- 输入到首个真实显示反馈≤20ms、捕获到真实呈现≤25ms的完整端到端测量。
- 与旧版同背景的全状态实拍对照，以及按参考视频进行的 iOS 1:1 主观视觉标定。
- 安装包已构建；本次运行验证使用便携可执行文件，未执行安装/卸载向导。

## 使用与复现

运行 `hdr-sdr-widget-0.4.0.exe`，或使用同目录 NSIS 安装包。旧版本保留在 `../v0.3.4-glass`。同一新版设置进程环境变量 `HSDR_RENDERER=legacy` 可使用保留的旧渲染器。

构建说明和实现细节见项目 `src/hdr_sdr_widget/NATIVE-RENDERER.md`。验收脚本为 `scripts/qa-native.ps1`，清晰度分析为 `scripts/analyze-clarity.py`，本报告由 `scripts/report-native.py` 从原始记录生成。浏览器打开 `qa/test-card.html` 可做文字、细线、棋盘格及移动背景的人工检查。

参考：[Microsoft屏幕捕获](https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture)、[DXGI呈现统计](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/ns-dxgi-dxgi_frame_statistics)、[Apple Liquid Glass演示](https://developer.apple.com/videos/play/wwdc2025/219/)。本项目使用自定义物理参数，不宣称掌握Apple私有参数。
'''
(root / '实施与验收报告.md').write_text(text, encoding='utf-8')
print(root / '实施与验收报告.md')
