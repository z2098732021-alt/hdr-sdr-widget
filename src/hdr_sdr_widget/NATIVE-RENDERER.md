# 0.4.0 原生玻璃

## 构建与回滚

环境：Windows 10/11、PowerShell 7、Node.js、Rust MSVC 工具链、Visual Studio C++ Build Tools 和 Windows SDK。锁文件已保留。

```powershell
pwsh -NoProfile -File ./scripts/build.ps1
```

本机构建时另外设置进程环境变量 `RUSTUP_HOME=D:\Rust\.rustup`、`CARGO_HOME=D:\Rust\.cargo`，其他机器使用自己的 Rust 安装位置。脚本固定输出到 `src-tauri/target`；生成 Release 可执行文件和 NSIS 安装包。`-Debug` 构建调试安装包；`-SkipFrontend` 仅用于已完成前端构建的场景。

默认使用原生胶囊。临时设置 `HSDR_RENDERER=legacy` 可运行保留的 Canvas 前端；不改变用户配置含义。原始 0.3.4 可执行文件、安装包保留在仓库 `deliverables/v0.3.4-glass`。重构前源码保留在 `.workbuddy/baseline-0.3.4-before-native/frontend` 与 `rust`。

## 实现

- `native/mod.rs`：Win32 输入队列、统一几何、边缘状态机、每帧快照、诊断。窗口使用 DirectComposition；WebView 仅用于诊断、Toast 和回滚界面。
- `native/gpu.rs`：同一输出适配器上的 D3D11 设备、FP16 预乘透明交换链、单帧延迟等待对象、DXGI 呈现统计。
- `native/capture.rs`：WGC 无边框 FP16 捕获，失败时回退 DuplicateOutput1；保存实际纹理格式、色彩空间、来源时间和独立 GPU 桌面缓存。较新 Windows 请求最小捕获间隔为零。
- `native/glass.hlsl`：物理像素 SDF 轮廓、桌面坐标采样、边缘折射和色散、独立液位磨砂。形变修改轮廓，不缩放处理后的位图。FP16 保持 scRGB；RGB10A2 HDR10 按 PQ/BT.2020 转换；8 位回退明确标为低精度。
- `native/motion.rs`：保留位置、速度、目标的解析阻尼弹簧；`native/tuning.rs` 集中主要运动与材质参数。这些是本项目参数，不是 Apple 私有参数。
- `native/brightness.rs`：独立工作线程、每显示器最新请求邮箱、30ms 写入间隔、可被新请求替代的延后回读、请求编号防止旧结果覆盖。驱动调用期间不持邮箱锁。最终值确认、系统吸附和失败恢复都有测试。托盘预设也走这一线程。

横向移动、纵向调节、滚轮和 Shift 滚轮、边缘 60ms 停留、移出 1200ms 收回规则保留。显示器拓扑变化与唤醒会使捕获资源重新建立；窗口隐藏时释放捕获。跟随鼠标显示器使用 GDI 源与显示配置稳定键匹配。

## 诊断口径

`submitted` 是 Present 调用成功次数，不等于显示器实际显示次数。`presentedFps` 由 DXGI PresentCount 和 QPC 时间跨度计算。`presentedIntervalP95Ms` 来自能逐帧观察到的 PresentRefreshCount 差值。漏掉中间观察时增加 `presentationObservationGaps`；因此 `actualPresentationVerified` 只有足够样本、无漏观测且不处于外观冻结模式时才为真。

`inputToSubmitP95Ms` 测量 Win32 输入进入本程序到提交，不是输入设备到屏幕光子延迟。首次 GPU 建立期间单列 `startupInputToSubmitMs`。`inputSamples=0` 表示没有样本。

`captureToSubmitP95Ms` 使用 WGC SystemRelativeTime / DDA LastPresentTime 与 QPC，测量新捕获帧到提交。它仍不包含提交后到实际显示的等待。`captureAgeMs` 是 GPU 缓存最后更新后的时间；桌面静止时增长正常，不能单凭它判断卡顿。

## 可复现验收

先从托盘退出已有胶囊，避免单实例程序把测试启动转交给旧进程。

```powershell
pwsh -NoProfile -File ./scripts/qa-native.ps1 -Mode Benchmark -Seconds 60
pwsh -NoProfile -File ./scripts/qa-native.ps1 -Mode Dda -Seconds 30 -OutputDirectory ./qa-dda
pwsh -NoProfile -File ./scripts/qa-native.ps1 -Mode Clarity -Seconds 10 -OutputDirectory ./qa-clarity
python ./scripts/analyze-clarity.py ./qa-clarity
```

QA 使用独立配置目录，不覆盖用户设置。输出 JSON、每秒资源 CSV、stderr；Clarity 另输出四种形状的 GPU 读回 PPM。读回只在显式验收模式中执行，生产绘制不经过 CPU 像素拷贝。

`HSDR_VISUAL_AUDIT=1` 会先冻结一帧桌面，然后关闭本窗口的捕获排除，供外部截图观察。该模式不能用于动态背景或性能验收。正常模式保持窗口捕获排除。

## 尚需更完整的实机覆盖

60/120Hz、其他 DPI、不同显卡和多显示器、旋转显示器、HDR 切换、真实睡眠恢复、独占全屏与长时间资源稳定性需要对应环境回归。当前单机测试与单元测试不能替代整个矩阵。

“接近 iOS”是视觉和交互标定目标，不能由一条弹簧公式或高帧率直接证明。仍需对照参考视频连续操作：展开中抓取、反向、边界回弹、快速连续点击。参考：[Apple Liquid Glass 官方演示](https://developer.apple.com/videos/play/wwdc2025/219/)。
