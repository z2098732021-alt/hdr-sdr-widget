#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
只读探针：枚举活动显示路径，读取 HDR 高级色彩状态与 SDR 内容亮度。

用途
----
V1「只读探针基线」验证脚本（ARCHITECTURE.md §10 V1）。
本脚本只做读取，绝不调用 DisplayConfigSetDeviceInfo，对系统零副作用。

期望输出（本机 Mi Monitor / Windows 11 build 26200）
---------------------------------------------------
sizeof PathInfo=72  ModeInfo=64  SDRWhiteLevel=24  AdvancedColorInfo=32  TargetDeviceName=420
1 条活动路径 / Mi Monitor / raw=2850 / 228.0 nits / 37% / HDR on / bpc=12

运行
----
python tools/probe_read.py
python tools/probe_read.py --json     # 机器可读输出
"""

from __future__ import annotations

import argparse
import ctypes
import json
import sys
from ctypes import wintypes

# ---------------------------------------------------------------------------
# 常量
# ---------------------------------------------------------------------------

QDC_ONLY_ACTIVE_PATHS = 0x00000002

DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME = 2
DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO = 9
DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL = 11

# 换算公式（ARCHITECTURE.md §5.2，三重验证锁定）
RAW_MIN = 1000
RAW_MAX = 6000
RAW_PER_PERCENT = 50
NITS_PER_RAW = 0.08
NITS_PER_PERCENT = 4.0
NITS_MIN = 80.0


# ---------------------------------------------------------------------------
# 基础类型
# ---------------------------------------------------------------------------

UINT32 = ctypes.c_uint32
ULONG = ctypes.c_ulong
LONG = ctypes.c_long
BOOL = ctypes.c_int
WCHAR = ctypes.c_wchar


class LUID(ctypes.Structure):
    """本地唯一标识符（适配器 ID）。"""
    _fields_ = [("LowPart", wintypes.DWORD), ("HighPart", LONG)]


class DISPLAYCONFIG_DEVICE_INFO_HEADER(ctypes.Structure):
    """所有 DisplayConfig Get/Set DeviceInfo 结构体的公共头，20 字节。"""
    _fields_ = [
        ("type", UINT32),
        ("size", UINT32),
        ("adapterId", LUID),
        ("id", UINT32),
    ]


# ---------------------------------------------------------------------------
# 路径 / 模式结构体
# ---------------------------------------------------------------------------


class DISPLAYCONFIG_PATH_SOURCE_INFO(ctypes.Structure):
    """路径源端信息，20 字节。"""
    _fields_ = [
        ("adapterId", LUID),
        ("id", UINT32),
        ("modeInfoIdx", UINT32),
        ("statusFlags", UINT32),
    ]


class DISPLAYCONFIG_RATIONAL(ctypes.Structure):
    """有理数，用于刷新率。"""
    _fields_ = [("Numerator", UINT32), ("Denominator", UINT32)]


class DISPLAYCONFIG_PATH_TARGET_INFO(ctypes.Structure):
    """路径目标端（显示器）信息，48 字节。"""
    _fields_ = [
        ("adapterId", LUID),
        ("id", UINT32),
        ("modeInfoIdx", UINT32),
        ("outputTechnology", UINT32),
        ("rotation", UINT32),
        ("scaling", UINT32),
        ("refreshRate", DISPLAYCONFIG_RATIONAL),
        ("scanLineOrdering", UINT32),
        ("targetAvailable", BOOL),
        ("statusFlags", UINT32),
    ]


class DISPLAYCONFIG_PATH_INFO(ctypes.Structure):
    """一条显示路径，72 字节。"""
    _fields_ = [
        ("sourceInfo", DISPLAYCONFIG_PATH_SOURCE_INFO),
        ("targetInfo", DISPLAYCONFIG_PATH_TARGET_INFO),
        ("flags", UINT32),
    ]


class DISPLAYCONFIG_MODE_INFO(ctypes.Structure):
    """模式信息（源模式 / 目标模式的共用容器），64 字节。"""
    _fields_ = [
        ("infoType", UINT32),
        ("id", UINT32),
        ("adapterId", LUID),
        ("modeInfo", ctypes.c_ubyte * 48),
    ]


# ---------------------------------------------------------------------------
# 设备信息结构体（Get 系列）
# ---------------------------------------------------------------------------


class DISPLAYCONFIG_SDR_WHITE_LEVEL(ctypes.Structure):
    """SDR 内容亮度读取结果，24 字节。"""
    _fields_ = [
        ("header", DISPLAYCONFIG_DEVICE_INFO_HEADER),
        ("SDRWhiteLevel", ULONG),
    ]


class DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO(ctypes.Structure):
    """高级色彩（HDR）状态，32 字节。"""
    _fields_ = [
        ("header", DISPLAYCONFIG_DEVICE_INFO_HEADER),
        ("value", UINT32),
        ("colorEncoding", UINT32),
        ("bitsPerColorChannel", UINT32),
    ]


class DISPLAYCONFIG_TARGET_DEVICE_NAME(ctypes.Structure):
    """显示器友好名与设备路径，420 字节。"""
    _fields_ = [
        ("header", DISPLAYCONFIG_DEVICE_INFO_HEADER),
        ("flags", UINT32),
        ("outputTechnology", UINT32),
        ("monitorManufactureId", wintypes.WORD),
        ("monitorProductId", wintypes.WORD),
        ("connectorInstance", UINT32),
        ("monitorFriendlyDeviceName", WCHAR * 64),
        ("monitorDevicePath", WCHAR * 128),
    ]


# ---------------------------------------------------------------------------
# Win32 函数绑定
# ---------------------------------------------------------------------------

_user32 = ctypes.WinDLL("user32", use_last_error=True)

_user32.GetDisplayConfigBufferSizes.argtypes = [
    UINT32,
    ctypes.POINTER(UINT32),
    ctypes.POINTER(UINT32),
]
_user32.GetDisplayConfigBufferSizes.restype = LONG

_user32.QueryDisplayConfig.argtypes = [
    UINT32,
    ctypes.POINTER(UINT32),
    ctypes.POINTER(DISPLAYCONFIG_PATH_INFO),
    ctypes.POINTER(UINT32),
    ctypes.POINTER(DISPLAYCONFIG_MODE_INFO),
    ctypes.POINTER(ctypes.c_ulonglong),
]
_user32.QueryDisplayConfig.restype = LONG

_user32.DisplayConfigGetDeviceInfo.argtypes = [ctypes.c_void_p]
_user32.DisplayConfigGetDeviceInfo.restype = LONG


# ---------------------------------------------------------------------------
# 纯函数：单位换算（与 Rust core/convert.rs 必须完全一致）
# ---------------------------------------------------------------------------


def raw_to_percent(raw: int) -> float:
    """把 raw 值换算为系统滑块百分比。"""
    if raw < RAW_MIN:
        return 0.0
    if raw > RAW_MAX:
        return 100.0
    return (raw - RAW_MIN) / RAW_PER_PERCENT


def raw_to_nits(raw: int) -> float:
    """把 raw 值换算为绝对亮度（cd/m²）。"""
    return raw * NITS_PER_RAW


def percent_to_raw(percent: float) -> int:
    """把百分比换算为 raw 值（用于交叉校验）。"""
    value = int(round(RAW_MIN + RAW_PER_PERCENT * percent))
    return max(RAW_MIN, min(RAW_MAX, value))


# ---------------------------------------------------------------------------
# 探测逻辑
# ---------------------------------------------------------------------------


def _check(rc: int, what: str) -> None:
    """检查 Win32 返回码，非 0 即抛出带 Win32 错误码的异常。"""
    if rc != 0:
        raise OSError(f"{what} 失败：返回码 {rc}（Win32 错误 {rc}）")


def query_paths() -> tuple[list[DISPLAYCONFIG_PATH_INFO], list[DISPLAYCONFIG_MODE_INFO]]:
    """查询当前全部活动显示路径。"""
    path_count = UINT32(0)
    mode_count = UINT32(0)
    rc = _user32.GetDisplayConfigBufferSizes(
        QDC_ONLY_ACTIVE_PATHS, ctypes.byref(path_count), ctypes.byref(mode_count)
    )
    _check(rc, "GetDisplayConfigBufferSizes")

    paths = (DISPLAYCONFIG_PATH_INFO * path_count.value)()
    modes = (DISPLAYCONFIG_MODE_INFO * mode_count.value)()
    out_paths = UINT32(path_count.value)
    out_modes = UINT32(mode_count.value)

    rc = _user32.QueryDisplayConfig(
        QDC_ONLY_ACTIVE_PATHS,
        ctypes.byref(out_paths),
        paths,
        ctypes.byref(out_modes),
        modes,
        None,
    )
    _check(rc, "QueryDisplayConfig")

    return list(paths[: out_paths.value]), list(modes[: out_modes.value])


def _get_advanced_color(adapter: LUID, target_id: int) -> tuple[int, DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO]:
    """读取该目标的高级色彩（HDR）状态。"""
    info = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO()
    info.header.type = DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO
    info.header.size = ctypes.sizeof(DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO)
    info.header.adapterId = adapter
    info.header.id = target_id
    rc = _user32.DisplayConfigGetDeviceInfo(ctypes.byref(info))
    return rc, info


def _get_sdr_white(adapter: LUID, target_id: int) -> tuple[int, int]:
    """读取该目标的 SDR 内容亮度 raw 值。"""
    info = DISPLAYCONFIG_SDR_WHITE_LEVEL()
    info.header.type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL
    info.header.size = ctypes.sizeof(DISPLAYCONFIG_SDR_WHITE_LEVEL)
    info.header.adapterId = adapter
    info.header.id = target_id
    rc = _user32.DisplayConfigGetDeviceInfo(ctypes.byref(info))
    return rc, int(info.SDRWhiteLevel)


def _get_target_name(adapter: LUID, target_id: int) -> tuple[int, str, str, int]:
    """读取该目标的友好名、设备路径与输出接口类型。"""
    info = DISPLAYCONFIG_TARGET_DEVICE_NAME()
    info.header.type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME
    info.header.size = ctypes.sizeof(DISPLAYCONFIG_TARGET_DEVICE_NAME)
    info.header.adapterId = adapter
    info.header.id = target_id
    rc = _user32.DisplayConfigGetDeviceInfo(ctypes.byref(info))
    return (
        rc,
        str(info.monitorFriendlyDeviceName),
        str(info.monitorDevicePath),
        int(info.outputTechnology),
    )


def probe() -> dict:
    """执行一次完整只读探测，返回结构化结果。"""
    paths, modes = query_paths()
    result: dict = {
        "sizes": {
            "PathInfo": ctypes.sizeof(DISPLAYCONFIG_PATH_INFO),
            "ModeInfo": ctypes.sizeof(DISPLAYCONFIG_MODE_INFO),
            "SdrWhiteLevel": ctypes.sizeof(DISPLAYCONFIG_SDR_WHITE_LEVEL),
            "AdvancedColorInfo": ctypes.sizeof(DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO),
            "TargetDeviceName": ctypes.sizeof(DISPLAYCONFIG_TARGET_DEVICE_NAME),
        },
        "pathCount": len(paths),
        "modeCount": len(modes),
        "displays": [],
    }

    for index, path in enumerate(paths):
        target = path.targetInfo
        adapter = target.adapterId
        target_id = int(target.id)

        rc_name, friendly, device_path, output_tech = _get_target_name(adapter, target_id)
        rc_adv, adv = _get_advanced_color(adapter, target_id)
        rc_sdr, raw = _get_sdr_white(adapter, target_id)

        percent = raw_to_percent(raw)
        nits = raw_to_nits(raw)
        roundtrip_raw = percent_to_raw(percent)

        result["displays"].append(
            {
                "index": index,
                "adapterId": f"0x{adapter.HighPart & 0xFFFFFFFF:08X}_{adapter.LowPart:08X}",
                "targetId": target_id,
                "targetAvailable": int(target.targetAvailable),
                "outputTechnology": output_tech,
                "refreshRate": f"{target.refreshRate.Numerator}/{target.refreshRate.Denominator}",
                "friendlyName": friendly if rc_name == 0 else "",
                "devicePath": device_path if rc_name == 0 else "",
                "advancedColor": {
                    "rc": rc_adv,
                    "value": int(adv.value),
                    "supported": int(adv.value & 0x1),
                    "enabled": int((adv.value >> 1) & 0x1),
                    "wideColorEnforced": int((adv.value >> 2) & 0x1),
                    "bitsPerColorChannel": int(adv.bitsPerColorChannel),
                },
                "sdrWhiteLevel": {
                    "rc": rc_sdr,
                    "raw": raw,
                    "percent": percent,
                    "nits": round(nits, 2),
                    "nitsFormulaCheck": round(NITS_MIN + NITS_PER_PERCENT * percent, 2),
                    "roundtripRaw": roundtrip_raw,
                    "roundtripOk": roundtrip_raw == raw,
                },
            }
        )

    return result


# ---------------------------------------------------------------------------
# 输出
# ---------------------------------------------------------------------------

_OUTPUT_TECH_NAMES = {
    0: "HD15 (VGA)",
    1: "S-Video",
    2: "复合视频",
    3: "分量视频",
    4: "DVI",
    5: "HDMI",
    6: "LVDS / 内置面板",
    8: "D端子",
    9: "SDI",
    10: "DisplayPort 外置",
    11: "DisplayPort 内置",
    12: "UDI 外置",
    13: "UDI 内置",
    14: "SDTV 复合",
    15: "Miracast",
    16: "间接有线",
    17: "间接无线",
    0x80000000: "内部/未初始化",
}


def _tech_name(code: int) -> str:
    """把输出接口类型码转成人话。"""
    return _OUTPUT_TECH_NAMES.get(code, f"未知({code})")


def print_report(result: dict) -> None:
    """把探测结果打印为人类可读报告。"""
    sizes = result["sizes"]
    print("sizeof PathInfo=%d  ModeInfo=%d  SDR_WHITE_LEVEL=%d  ADVANCED_COLOR_INFO=%d  TARGET_DEVICE_NAME=%d"
          % (sizes["PathInfo"], sizes["ModeInfo"], sizes["SdrWhiteLevel"],
             sizes["AdvancedColorInfo"], sizes["TargetDeviceName"]))
    print("活动路径数 = %d，模式数 = %d" % (result["pathCount"], result["modeCount"]))
    print("")

    for disp in result["displays"]:
        print("--- path %d ---" % disp["index"])
        print("  adapterId   = %s   targetId = %d   available = %d   outputTech = %d (%s)"
              % (disp["adapterId"], disp["targetId"], disp["targetAvailable"],
                 disp["outputTechnology"], _tech_name(disp["outputTechnology"])))
        adv = disp["advancedColor"]
        print("  advancedColor: rc=%d  value=0x%02X  supported=%d  enabled=%d  wideColorEnforced=%d  bitsPerColorChannel=%d"
              % (adv["rc"], adv["value"], adv["supported"], adv["enabled"],
                 adv["wideColorEnforced"], adv["bitsPerColorChannel"]))
        sdr = disp["sdrWhiteLevel"]
        print("  sdrWhiteLevel: rc=%d  raw=%d  nits=%.1f  percent=%.1f"
              % (sdr["rc"], sdr["raw"], sdr["nits"], sdr["percent"]))
        print("  公式交叉校验 : nits_formula=%.1f（与 raw×0.08 一致=%s）  raw回环=%d（一致=%s）"
              % (sdr["nitsFormulaCheck"],
                 "是" if abs(sdr["nitsFormulaCheck"] - sdr["nits"]) < 0.01 else "否",
                 sdr["roundtripRaw"], "是" if sdr["roundtripOk"] else "否"))
        print("  friendlyName : '%s'" % disp["friendlyName"])
        print("  devicePath   : '%s'" % disp["devicePath"])
        print("")


def main(argv: list[str] | None = None) -> int:
    """命令行入口。"""
    parser = argparse.ArgumentParser(description="HDR SDR 内容亮度 —— 只读探针（V1）")
    parser.add_argument("--json", action="store_true", help="以 JSON 输出（机器可读）")
    args = parser.parse_args(argv)

    try:
        result = probe()
    except OSError as exc:
        print("探测失败：%s" % exc, file=sys.stderr)
        return 2

    if args.json:
        print(json.dumps(result, ensure_ascii=False, indent=2))
    else:
        print_report(result)
    return 0


if __name__ == "__main__":
    sys.exit(main())
