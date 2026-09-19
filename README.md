# HDR SDR Widget

由于Windows在hdr下调节sdr内容亮度的操作如同安禄山进长安——唐完了

加上某个算是大厂的显示器上了电视系统就不给你DDC/CI控制导致调节亮度需要及其阴间的步骤（说的就是你某米）

迫不得已动用AI神力开发的该项目

照着Liquid Glass的ios音量滑条仿刻之物，毕竟ios审美确实太棒了😋，动画也尽量复刻了

本人代码能力羸弱，有能力的大佬随便拿去维护和二次开发喵

下面是gpt老师整理的开发文档捏：

一款面向 Windows 11 HDR 用户的轻量悬浮控制器，用一个常驻桌面的 Liquid Glass 面板快速调节系统的“SDR 内容亮度”。

当前版本：`0.5.0`

## 功能

- 直接读取和写入 Windows HDR 的 SDR 内容亮度
- 支持多显示器识别与跟随鼠标选择目标显示器
- 无边框、置顶的深色 Liquid Glass 悬浮面板
- 托盘常驻、快捷呼出、预设亮度与位置记忆
- HDR 关闭、系统写入失败等场景的明确降级提示
- 原生 Direct3D 11 / Desktop Duplication 光学材质渲染与诊断工具

## 技术栈

- [Tauri 2](https://tauri.app/) 桌面应用框架
- Rust 2021 + `windows` crate（Win32、DXGI、D3D11、注册表与显示器控制）
- TypeScript + Vite 前端
- HLSL 原生玻璃材质着色器

## 项目结构

```text
.
├─ src/hdr_sdr_widget/     # 应用源码
│  ├─ src/                 # TypeScript UI
│  ├─ src-tauri/           # Rust、Win32 与 HLSL 原生层
│  ├─ scripts/             # 构建、诊断与视觉对比脚本
│  └─ tools/               # 辅助工具
├─ docs/hdr-sdr-widget/    # PRD 与架构文档
└─ tools/                  # 仓库级视觉检测工具
```

构建产物、安装包、依赖目录、QA 原始数据与本地日志不会进入 Git 历史；它们都可以从源码重新生成。正式安装包通过 GitHub Releases 分发。

## 开发环境

- Windows 11（建议开启 HDR）
- Node.js 20 或更高版本
- Rust 1.82 或更高版本，MSVC 工具链
- Visual Studio 2022 Build Tools（Desktop development with C++）
- WebView2 Runtime

## 本地开发

```powershell
Set-Location 'src/hdr_sdr_widget'
npm ci
npm run tauri:dev
```

## 检查与构建

```powershell
Set-Location 'src/hdr_sdr_widget'

# 类型检查与前端构建
npm run build

# Rust 测试
cargo test --manifest-path 'src-tauri/Cargo.toml'

# Windows NSIS 安装包
npm run tauri:build
```

生产环境若直接调用 Cargo，必须启用 `custom-protocol`，否则前端资源不会嵌入可执行文件：

```powershell
cargo build --release --features custom-protocol --manifest-path 'src-tauri/Cargo.toml'
```

## 文档

- [产品需求](docs/hdr-sdr-widget/PRD.md)
- [架构说明](docs/hdr-sdr-widget/ARCHITECTURE.md)
- [v2 产品需求](docs/hdr-sdr-widget/v2/PRD.md)
- [v2 架构说明](docs/hdr-sdr-widget/v2/ARCHITECTURE.md)
- [原生渲染说明](src/hdr_sdr_widget/NATIVE-RENDERER.md)

## 平台说明

该项目依赖 Windows HDR、注册表、DXGI、D3D11、DWM 和 Windows Graphics Capture 等平台能力，只支持 Windows。实际效果会受到显卡驱动、显示器 HDR 能力和 Windows 版本影响。

## 许可证

本项目采用 [MIT License](LICENSE) 开源。
