import { invoke } from "@tauri-apps/api/core";

interface NativeDiagnostics {
  backend: string; format: number; fallbackReason: string; error: string;
  submitted: number; captured: number; presentCount: number | null;
  submitIntervalP95Ms: number; inputToSubmitP95Ms: number; captureAgeMs: number;
  cpuFrameP95Ms: number; actualPresentationVerified: boolean; presentedFps: number;
  presentedIntervalP95Ms: number; displayRefreshHz: number; presentationSamples: number;
  updatedAt: number; phase: string; visualAudit: boolean;
  presentationObservationGaps: number; inputSamples: number; startupInputToSubmitMs: number;
  captureToSubmitP95Ms: number; captureUpdateGapMaxMs: number;
  brightness: {percent: number; raw: number | null; backend: string; mode: string; canControl: boolean; ddcAvailable: boolean; ddcError: string | null; fallbackReason: string | null; softwareTransmission: number; captureSafe: boolean; confirmed: boolean; error: string | null};
}
export async function mountNativeDiag(root: HTMLElement): Promise<boolean> {
  let latest = await invoke<NativeDiagnostics | null>("get_native_diagnostics");
  if (!latest) return false;
  root.style.cssText = "padding:24px;color:#eee;background:#17181c;min-height:100vh;font:14px/1.7 'Segoe UI',sans-serif;overflow:auto";
  const heading = document.createElement("h1"); heading.textContent = "原生玻璃诊断";
  heading.style.cssText = "font-size:21px;margin-bottom:14px";
  const status = document.createElement("p");
  const table = document.createElement("dl"); table.style.cssText = "display:grid;grid-template-columns:185px 1fr;gap:8px;margin:18px 0";
  const note = document.createElement("p"); note.style.cssText = "color:#aaa;font-size:12px;margin:16px 0";
  note.textContent = "显示帧统计来自 DXGI，提交间隔与实际显示间隔分开统计。背景静止时，缓存帧年龄增长属于正常现象。没有输入样本时不会宣称输入延迟为零。";
  const copy = document.createElement("button"); copy.textContent = "复制诊断报告";
  copy.style.cssText = "padding:8px 16px;border:1px solid #555;border-radius:8px;background:#30323a;color:white;cursor:pointer";
  copy.onclick = () => { void navigator.clipboard.writeText(JSON.stringify(latest, null, 2)).then(() => {copy.textContent="已复制";}).catch(() => {copy.textContent="复制失败，请重试";}); };
  root.replaceChildren(heading, status, table, note, copy);
  function render(d: NativeDiagnostics): void {
    const stale = Date.now() - d.updatedAt > 2000;
    status.textContent = d.error || (stale ? "渲染心跳暂停" : `${d.backend} · ${d.phase}${d.visualAudit ? " · 静态外观验收模式" : ""}`);
    status.style.color = d.error || stale ? "#ffb45c" : "#8fdaba";
    const rows: Array<[string, string]> = [
      ["桌面纹理", d.format === 10 ? "FP16 / scRGB" : `${d.format}（低精度捕获）`],
      ["实际显示帧率", d.presentationSamples >= 120 && !d.visualAudit ? `${d.presentedFps.toFixed(1)} fps` : "样本不足 / 外观冻结模式"],
      ["已观测显示间隔 P95", d.presentationSamples >= 120 ? `${d.presentedIntervalP95Ms.toFixed(2)} ms` : "样本不足"],
      ["显示统计覆盖", d.actualPresentationVerified ? "连续样本完整" : `${d.presentationObservationGaps} 次观察间断 / 请核对样本和模式`],
      ["提交帧间隔 P95", `${d.submitIntervalP95Ms.toFixed(2)} ms`],
      ["CPU 每帧工作 P95", `${d.cpuFrameP95Ms.toFixed(2)} ms`],
      ["输入到提交 P95", d.inputSamples > 0 ? `${d.inputToSubmitP95Ms.toFixed(2)} ms（${d.inputSamples} 个样本）` : "没有输入样本"],
      ["启动期输入到提交", `${d.startupInputToSubmitMs.toFixed(2)} ms`],
      ["捕获到提交 P95", `${d.captureToSubmitP95Ms.toFixed(2)} ms`],
      ["捕获帧 / 提交帧", `${d.captured} / ${d.submitted}`],
      ["显示统计样本", `${d.presentationSamples}`],
      ["缓存帧年龄", `${d.captureAgeMs.toFixed(1)} ms`],
      ["实际亮度控制", `${d.brightness.backend}（选择：${d.brightness.mode}）`],
      ["确认亮度", `${d.brightness.percent}%${d.brightness.raw === null ? "" : ` / raw ${d.brightness.raw}`}`],
      ["DDC/CI 状态", d.brightness.ddcAvailable ? "读取可用，写入以回读为准" : d.brightness.ddcError || "未检测 / 当前不需要"],
      ["控制降级原因", d.brightness.fallbackReason || "无"],
      ["软件透光 / 捕获排除", `${(d.brightness.softwareTransmission * 100).toFixed(1)}% / ${d.brightness.captureSafe}`],
      ["亮度请求错误", d.brightness.error || "无"],
      ["回退原因", d.fallbackReason || "无"],
    ];
    table.replaceChildren(...rows.flatMap(([label, value]) => {
      const term = document.createElement("dt"); term.textContent=label;term.style.color="#aaa";
      const detail = document.createElement("dd"); detail.textContent=value;
      return [term, detail];
    }));
  }
  render(latest);
  let running = true;
  window.addEventListener("pagehide", () => {running=false;}, {once:true});
  async function tick(): Promise<void> {
    try { const d=await invoke<NativeDiagnostics | null>("get_native_diagnostics"); if(d){latest=d;render(d);} }
    catch { status.textContent="无法读取原生渲染状态"; }
    if(running)window.setTimeout(() => void tick(),500);
  }
  window.setTimeout(() => void tick(),500);
  return true;
}
