from pathlib import Path
import json,hashlib
p=Path('deliverables/v0.4.4-native')
sha=hashlib.sha256((p/'hdr-sdr-widget-0.4.4.exe').read_bytes()).hexdigest().upper()
lines=['# 0.4.4 实施与验收报告','','## 设计与实现','',
'FP16/scRGB 本体渲染；透明中央面无额外模糊；白底下部增加自适应灰色底衬。高光改为顶部短弧与窄折射亮带，取消被否定的长边强亮带。',
'侧边从 65% 到 100%，位移 response=0.50s、阻尼=0.825，尺寸 response=0.52s、阻尼=0.825；附加横向拉伸上限 4%。重定向保留位置和速度。',
'参考公开 API 的参数语义和默认值，不声称掌握 iOS 系统控件私有参数。',
'',
'- [Apple spring 文档](https://developer.apple.com/documentation/swiftui/animation/spring(response:dampingfraction:blendduration:))',
'- [Apple Materials 指南](https://developer.apple.com/design/human-interface-guidelines/materials)',
'- [WWDC26 暗边与镜面高光](https://developer.apple.com/videos/play/wwdc2026/102/)',
'','## 最终程序测试','',
'39 项主程序测试（含两个像素着色器与弹簧轨迹）及 42 项核心测试通过；TypeScript 检查、发布构建通过。',
'',f'最终 exe SHA256：{sha}','',
'| 场景 | 呈现 fps | 已观测 P95 ms | 捕获至提交 P95 ms | 更新最大间隔 ms | 观测缺口 |',
'|---|---:|---:|---:|---:|---:|']
for name in ['final-edge-right','final-edge-left','final-dda']:
 folder=p/'qa'/name
 if not (folder/'diagnostics.json').exists():continue
 run=json.loads((folder/'run.json').read_text(encoding='utf-8-sig'));assert run['sha256']==sha
 d=json.loads((folder/'diagnostics.json').read_text(encoding='utf-8-sig'))
 lines.append(f"| {name} | {d['presentedFps']:.2f} | {d['presentedIntervalP95Ms']:.3f} | {d['captureToSubmitP95Ms']:.3f} | {d['captureUpdateGapMaxMs']:.3f} | {d['presentationObservationGaps']} |")
for name in ['final-material','final-record']:
 run=json.loads((p/'qa'/name/'run.json').read_text(encoding='utf-8-sig'))
 assert run['sha256']==sha, f'{name}: stale executable evidence'
hdr=p/'qa/final-material/hdr-analysis.json'
if hdr.exists():
 a=json.loads(hdr.read_text());assert len(a)==20 and all('error' not in x for x in a)
 contrast=min(x['whiteBoundaryContrast'] for x in a if 'whiteBoundaryContrast' in x)
 assert contrast>=3 and max(x['nearPeakAreaPercent'] for x in a)<=5
 lines+=['','## HDR 与白底','',f"显示器报告峰值 {a[0]['peakNits']:.0f} nit；SDR 白点 {a[0]['whiteNits']:.0f} nit；GPU 输出最大亮度 {max(x['maximumNits'] for x in a):.1f} nit；接近峰值面积最高 {max(x['nearPeakAreaPercent'] for x in a):.3f}%；白底分界最低对比度 {contrast:.2f}:1。",
 '亮度由原始 scRGB 浮点像素换算，不是光度计测得的物理亮度。RGBA32F 和逐图元数据保留；PNG 会截断 HDR 高光，仅用于结构对照。']
lines+=['','## 证据与未完成项','',
'- final-material/material-grid.png：四类背景和五档液位的原生 GPU 输出。',
'- final-record/reveal.mp4：程序内自动展开及反向的 GPU 帧序列（已隔离真实鼠标的收回计时干扰），不是外部实拍或人工手势录像。录制有测试专用读回，其性能数据不作验收依据。',
'- revised-input：调试版冻结背景的实机点击和拖动。有限样本 input-to-submit P95 为 23.21ms，未达到 20ms；不能代替正式发布版端到端测量。',
'- 呈现统计存在观测缺口；60/120Hz、不同 DPI、HDR 切换、睡眠、全屏、多显示器及长时稳定性尚未完成完整回归。',
'- GPU 重建会重新初始化底衬历史并渐入；尚未验证所有重建情况的无闪烁恢复。',
'- 未完成与 iOS 视频逐帧对齐；材质满意度仍需实机确认。rejected-candidate 和早期 qa 目录均不代表最终版本。',
'','## 构建与回退','',
'PowerShell 7、Rust MSVC、Node.js 环境中执行 src/hdr_sdr_widget/scripts/build.ps1。此机器 Rust 位于 D:/Rust，RUSTUP_HOME=D:/Rust/.rustup，CARGO_HOME=D:/Rust/.cargo。',
'QA 入口 scripts/qa-native.ps1，支持 Material、Docked、DockedLeft、Record、Dda。相邻 v0.4.3-native 保留回退包。用户配置语义不变，HDR 未拆版。']
(p/'实施与验收报告.md').write_text('\n'.join(lines),encoding='utf-8')
print(str(p/'实施与验收报告.md'))
