#!/bin/bash
# YanyunGame launcher v2.2
# 直启官方 Simulator (绕 Gatekeeper), 按配置注入 lsfg-metal 与 backend 选择
# 配置文件: ~/lsfg-metal/yanyun.conf  (与 settings.command 共用)
# v2.1: 修复 GPTK_ROOT 被清空的 bug; GPTK 环境变量无条件导出 (与 backend 解耦)
# v2.2: DX12 会话跳过 lsfg 注入 (shim 与 D3DMetal DX12 present 不兼容, 见 README)
# v2.4: 按 yanyun.conf 的 resolution 切 1080/1440 无 HiDPI, 退出还原 1440 HiDPI (需 displayplacer)
# v2.5: dxmt_fps (yanyun.conf) — 原生模式 (lsfg=off) 经 DXMT_CONFIG 注入
#       d3d11.preferredMaxFrameRate, Metal/CoreAnimation 对齐 vblank 的帧节奏器
# v2.6: pacer (yanyun.conf) — lsfg=off 时注入 shim 走 fork 自带的槽位锁相
#       帧节奏器 (LSFGM_PACER), 替代已判死的 dxmt_fps (DXMT 官方排程, 11-22 实测黑屏)
# v2.7: lsfg 分支守卫 libMoltenVK.real.dylib — 垫片内部 dlopen 真驱动 (插帧
#       Vulkan 后端直呼), 缺了它 lsfg 会话黑启动 ~97s 自灭; 源头在
#       Yanyun.app/Contents/Resources/wine-release/lib64/libMoltenVK.dylib

set -u

YANYUN_APP="/Applications/Yanyun.app"
SIMULATOR="$YANYUN_APP/Contents/MacOS/Simulator"
WINE_ROOT="$YANYUN_APP/Contents/Resources/wine-release"
CFG="$HOME/lsfg-metal"
APP_SUPPORT="$HOME/Library/Application Support/yanyun.simulator"
GPTK_ROOT="$APP_SUPPORT/gptk"
LSFG_DIR="$CFG"

log() { echo "[$(date '+%T')] [launcher] $*" >> "$HOME/Library/Logs/yanyun-game.log" 2>/dev/null; }

# ---------- 前置检查 ----------
if [ ! -x "$SIMULATOR" ]; then
  osascript -e 'display dialog "找不到官方模拟器：/Applications/Yanyun.app\n请先重新安装燕云模拟器。" buttons {"好"} with icon stop'
  exit 1
fi

# ---------- 读配置 (yanyun.conf: key=value, # 注释) ----------
CONF="$CFG/yanyun.conf"
cfg() { sed -n "s/^$1=//p" "$CONF" 2>/dev/null | tail -1 | tr -d ' ' | head -c 20; }
backend="d3dmetal"; [ -n "$(cfg backend)" ] && backend="$(cfg backend)"
lsfg="off";         [ -n "$(cfg lsfg)" ]     && lsfg="$(cfg lsfg)"
hdr="off";          [ -n "$(cfg hdr)" ]      && hdr="$(cfg hdr)"
deadband="off";     [ -n "$(cfg deadband)" ] && deadband="$(cfg deadband)"
fsr4="off";         [ -n "$(cfg fsr4)" ]     && fsr4="$(cfg fsr4)"
# pacing/multiplier: adaptive 铺满刷新槽 (更顺, 插帧多); vsync 固定倍率 (插帧少)
pacing="adaptive";  [ -n "$(cfg pacing)" ]   && pacing="$(cfg pacing)"
multiplier="3";     [ -n "$(cfg multiplier)" ] && multiplier="$(cfg multiplier)"
# resolution: 1080/1440=游戏用该分辨率(均无 HiDPI, 退出还原 1440 HiDPI); none/其它=不改
res_choice="none";  [ -n "$(cfg resolution)" ] && res_choice="$(cfg resolution)"

log "backend=$backend lsfg=$lsfg hdr=$hdr deadband=$deadband fsr4=$fsr4 resolution=$res_choice"

# ---------- 环境变量 ----------
arch -x86_64 true 2>/dev/null || { log "Rosetta 不可用"; exit 1; }

DYLD_INSERT=""

# GPTK (D3DMetal) 环境: 无条件激活 (v2.1 修复)
# 原逻辑: 先 export GPTK_ROOT="" 清空了第 13 行的值, 再仅在 d3dmetal 分支里
# 检查 "$GPTK_ROOT/external/..." (空路径永远失败) → GPTK 环境变量从未导出。
# 实测: yysls.exe 的 gptk 版 d3d 系 dll 需靠 CX_APPLEGPTK_LIBD3DSHARED_PATH
# 挂接 libd3dshared.dylib (D3DMetal 实现), 与所选 backend 无关, 故全部模式导出。
if [ -f "$GPTK_ROOT/external/libd3dshared.dylib" ]; then
  export GPTK_ROOT
  export CX_APPLEGPTK_LIBD3DSHARED_PATH="$GPTK_ROOT/external/libd3dshared.dylib"
  export DYLD_FRAMEWORK_PATH="$GPTK_ROOT/external"
  log "GPTK 已激活: $GPTK_ROOT"
else
  log "WARN: GPTK 目录缺失, gptk 版 d3d dll 无法挂接 (DX12 不可用)"
fi

# lsfg-metal: 注入 shim (Metal hook 同时覆盖 D3DMetal 与 DXMT)
# v2.2: shim 的 Metal present hook 与 D3DMetal 的 DX12 呈现路径不兼容 ——
#   D3DMCommandWorker::DoPresent 内反复抛 unrecognized-selector ObjC 异常,
#   present worker 满载空转, 游戏卡死在加载界面 (DX11 路径不受影响)。
#   实证 2026-11-18: 关 shim 后 DX12 顺利进游戏 (LocalShaders/compiled.dx12 出现)。
#   上次会话为 DX12 时跳过注入, 保证 DX12 可玩 (代价: DX12 无插帧);
#   切回 DX11 后恢复正常注入。
LAST_API_TAG="/Volumes/removeable/yysls_fast/LocalData/last_graphics_api.tag"
SETTING_INI="$APP_SUPPORT/wine-prefix/drive_c/Program Files/yysls/Win32/deploy/setting.ini"
# LSFG_FORCE=1 强制注入 (测试新 shim 时绕过 DX12 检测)
if [ "$lsfg" = "on" ] && [ "${LSFG_FORCE:-}" != "1" ]; then
  if grep -qs dx12 "$LAST_API_TAG" || grep -qs "^DX12=true" "$SETTING_INI"; then
    log "DX12 会话: 跳过 lsfg 注入 (shim 与 D3DMetal DX12 present 不兼容, DX12 无插帧)"
    lsfg="off"
  fi
fi
if [ "$lsfg" = "on" ]; then
  if [ -f "$LSFG_DIR/libMoltenVK.dylib" ]; then
    # v2.7: 真驱动是垫片的运行时依赖 (编译在二进制里, 脚本层看不见) — 缺件报警
    if [ ! -f "$LSFG_DIR/libMoltenVK.real.dylib" ]; then
      log "WARN: libMoltenVK.real.dylib 缺失 — 插帧后端无法加载真驱动, 会话将黑启动自灭"
    fi
    DYLD_INSERT="$LSFG_DIR/libMoltenVK.dylib"
    export DYLD_LIBRARY_PATH="$LSFG_DIR${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
    export LSFGM_METAL=1
    export LSFGM_ENV=1
    # HDR 10-bit 去噪实验: 4/1023 阈值，修静态直线被光流抖动
    [ "$deadband" = "on" ] && export LSFGM_DEADBAND=1
    # adaptive + cap 3: fit the actual source cadence into the display's vblank grid.
    # vsync forces exactly 2x (27fps source -> 54fps output on 60Hz = one dropped vblank
    # every ~6 pairs, perceived as periodic stutter). adaptive emits 1..3 slots per source
    # frame so every vblank carries consecutively-phased content.
    export LSFGM_MULTIPLIER=$multiplier
    export LSFGM_PACING_MODE=$pacing
    export LSFGM_LOG_FILE="$LSFG_DIR/lsfg.log"
    export LSFGM_STATS=1
    # 诊断: LSFG_METAL_DUMP=<目录> 转发为 shim 的 LSFGM_METAL_DUMP, 周期 dump 插帧内容
    [ -n "${LSFG_METAL_DUMP:-}" ] && export LSFGM_METAL_DUMP="$LSFG_METAL_DUMP"
    # 诊断: LSFG_PACE_DEBUG=1 逐帧记录 interval/blocked/trusted
    [ -n "${LSFG_PACE_DEBUG:-}" ] && export LSFGM_PACE_DEBUG=1
    # 日志: 默认只输出 WARN/ERROR (安静); LSFGM_LOG_LEVEL=info 恢复完整日志
    # (启动判据 Loaded/hdr10 判定行/stats 心跳都是 INFO, 验证时必须设 info)
    [ -n "${LSFGM_LOG_LEVEL:-}" ] && export LSFGM_LOG_LEVEL="$LSFGM_LOG_LEVEL"
    # 诊断: DXMT_LOG=1 记录 dxmt 日志 (DXMT_LOG_LEVEL 可选 trace/debug/info/warn, 默认 debug)
    if [ -n "${DXMT_LOG:-}" ]; then
      export DXMT_LOG_PATH="$LSFG_DIR/dxmt.log"
      export DXMT_LOG_LEVEL="${DXMT_LOG_LEVEL:-debug}"
      log "dxmt 日志: $DXMT_LOG_PATH (级别 $DXMT_LOG_LEVEL)"
    fi
    if [ -f "$LSFG_DIR/lsfg-vk.dll" ]; then
      export LSFGM_DLL_PATH="$LSFG_DIR/lsfg-vk.dll"
    else
      log "WARN: lsfg-vk.dll 缺失, 插帧不生效 (从 Steam Lossless Scaling lsfg-vk beta 分支拷贝)"
    fi
    log "lsfg-metal 已注入 (pacing=$pacing, multiplier=$multiplier, deadband=$deadband)"
  else
    log "WARN: shim 缺失, 跳过 lsfg 注入"
  fi
fi

# ---------- v2.6: 原生模式 pacer (自家 fork, 无插帧) ----------
# pacer=30: lsfg=off 时注入 shim, worker 睡到 vblank 槽位才放行 present
# (阻塞式反压, 引擎被锁到目标帧率) —— 完全不经 DXMT 排程, 与已判死的
# dxmt_fps (DXMT 官方 CoreAnimation 排程, 11-22 实测 ~6min 黑屏) 无关。
pacer="off"; [ -n "$(cfg pacer)" ] && pacer="$(cfg pacer)"
if [ -n "$pacer" ] && [ "$pacer" != "off" ]; then
  if [ "$lsfg" = "on" ]; then
    log "pacer 忽略 (lsfg=on 由插帧节拍器管辖)"
  elif grep -qs dx12 "$LAST_API_TAG" || grep -qs "^DX12=true" "$SETTING_INI"; then
    log "DX12 会话: pacer 跳过 (shim 与 D3DMetal DX12 present 不兼容)"
  elif [ -f "$LSFG_DIR/libMoltenVK.dylib" ]; then
    DYLD_INSERT="$LSFG_DIR/libMoltenVK.dylib"
    export DYLD_LIBRARY_PATH="$LSFG_DIR${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
    export LSFGM_METAL=1
    export LSFGM_ENV=1
    export LSFGM_MULTIPLIER=2   # 仅为通过 env 校验; pacer 模式不插帧
    export LSFGM_PACER="$pacer"
    export LSFGM_LOG_FILE="$LSFG_DIR/lsfg.log"
    export LSFGM_LOG_LEVEL="${LSFGM_LOG_LEVEL:-info}"
    export LSFGM_STATS=1
    log "Metal pacer 已注入: ${pacer}fps 槽位锁相 (原生模式)"
  else
    log "WARN: shim 缺失, pacer 跳过"
  fi
fi

# ---------- 实验: 第三方 hook 注入 (评估用) ----------
# YANYUN_INJECT=<dylib> 时用它替换注入, 不加载本项目的 shim
if [ -n "${YANYUN_INJECT:-}" ]; then
  DYLD_INSERT="$YANYUN_INJECT"
  export DYLD_LIBRARY_PATH="$(dirname "$YANYUN_INJECT")${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
  log "实验注入: $YANYUN_INJECT"
fi

# ---------- DXMT 日志 (独立于 lsfg; 原先写在 lsfg 分支里, lsfg=off 时失效) ----------
if [ -n "${DXMT_LOG:-}" ]; then
  export DXMT_LOG_PATH="$LSFG_DIR/dxmt.log"
  export DXMT_LOG_LEVEL="${DXMT_LOG_LEVEL:-debug}"
  log "dxmt 日志: $DXMT_LOG_PATH (级别 $DXMT_LOG_LEVEL)"
fi

# fsr4: 仅 d3dmetal 后端有效 (已弃用, 保留开关)
if [ "$fsr4" = "on" ] && [ "$backend" = "d3dmetal" ]; then
  export FSR4_ENABLE=1
  log "WARN: fsr4 实验性开关打开"
fi

# 后端覆盖 (winecompat: 1=wined3d 2=dxmt 3=d3dmetal)
case "$backend" in
  wined3d)  export SIM_BACKEND_OVERRIDE=1 ;;
  dxmt)     export SIM_BACKEND_OVERRIDE=2 ;;
  d3dmetal) export SIM_BACKEND_OVERRIDE=3 ;;
  *)        unset SIM_BACKEND_OVERRIDE ;;
esac

# dxmt 配置透传 (DXMT_CONFIG): 语法同 dxmt.conf, 多项用 ; 分隔
# 例: DXMT_CONFIG="dxgi.forceSDR=True" 让 dxmt 对游戏报 SDR (BitsPerColor=8 + G22 sRGB)
[ -n "${DXMT_CONFIG:-}" ] && export DXMT_CONFIG && log "DXMT_CONFIG: $DXMT_CONFIG"

# 原生模式帧节奏器 (dxmt_fps): DXMT 官方选项 d3d11.preferredMaxFrameRate ——
# 上屏节奏由 Metal/CoreAnimation 对齐 vblank 而非 CPU 模拟 (上游文档: "beautiful
# straight line"; 60Hz 请求 40 实得 30, 按刷新分频量化)。30 + 游戏内 30fps 档
# = 垂直同步式匀速 30。仅 lsfg!=on 生效 (lsfg=on 时 shim 自管节奏, 避免双节拍器)。
dxmt_fps=""; [ -n "$(cfg dxmt_fps)" ] && dxmt_fps="$(cfg dxmt_fps)"
if [ -n "$dxmt_fps" ] && [ "$dxmt_fps" != "off" ] && [ "$lsfg" != "on" ]; then
  export DXMT_CONFIG="d3d11.preferredMaxFrameRate=$dxmt_fps${DXMT_CONFIG:+;$DXMT_CONFIG}"
  log "DXMT 帧节奏器: preferredMaxFrameRate=$dxmt_fps (原生模式, Metal 对齐 vblank)"
fi

# ---------- 日志 ----------
export PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$PATH"
LOGDIR="$HOME/Library/Logs"
LOGFILE="$LOGDIR/yanyun-game.log"
{
  echo "===== $(date '+%F %T') backend=$backend lsfg=$lsfg hdr=$hdr fsr4=$fsr4 ====="
} >> "$LOGFILE" 2>/dev/null

# ---------- 游戏模式显示配置: hdr=on 时启动切 HDR, 退出还原 ----------
# 注意: 数字 id (2/3) 重启后会漂移, 必须用稳定的显示器 UUID
TOGGLE_HDR="$CFG/ToggleHDR"
MAIN_UUID="85322CD1-474F-4E79-AA45-47F4DA72C4F6"

# 分辨率切换: 游戏用 1080/1440 无 HiDPI; 退出后还原 1440 HiDPI
DISPLAYPLACER="$(command -v displayplacer 2>/dev/null || echo /opt/homebrew/bin/displayplacer)"
DISPLAY_CHANGED=""

apply_res() {  # $1: 1080 | 1440 —— 均为 scaling:off (无 HiDPI)
  [ -x "$DISPLAYPLACER" ] || return 1
  case "$1" in
    1080) "$DISPLAYPLACER" "id:$MAIN_UUID res:1920x1080 hz:60 scaling:off origin:(0,0) degree:0" >/dev/null 2>&1 ;;
    1440) "$DISPLAYPLACER" "id:$MAIN_UUID res:2560x1440 hz:60 scaling:off origin:(0,0) degree:0" >/dev/null 2>&1 ;;
  esac
}

restore_res() {  # 退出还原: 1440 HiDPI
  [ -x "$DISPLAYPLACER" ] || return 1
  "$DISPLAYPLACER" "id:$MAIN_UUID res:2560x1440 hz:60 scaling:on origin:(0,0) degree:0" >/dev/null 2>&1
}

restore_display() {
  [ "$hdr" = "on" ] && "$TOGGLE_HDR" "$MAIN_UUID" off >/dev/null 2>&1 && log "主屏 HDR 已还原 off"
  if [ -n "$DISPLAY_CHANGED" ]; then
    restore_res && log "分辨率已还原 1440 HiDPI"
  fi
}

if [ "$hdr" = "on" ]; then
  if [ -x "$TOGGLE_HDR" ]; then
    if "$TOGGLE_HDR" "$MAIN_UUID" on >/dev/null 2>&1; then
      log "主屏 HDR 已开"
    else
      log "WARN: 主屏 HDR 开启失败"
    fi
  else
    log "WARN: ToggleHDR 缺失, HDR 不自动切换"
  fi
fi

# ---------- 分辨率: 游戏用 1080/1440 无 HiDPI (退出由 restore_display 还原 1440 HiDPI) ----------
if [ -x "$DISPLAYPLACER" ]; then
  case "$res_choice" in
    1080|1080p) apply_res 1080 && DISPLAY_CHANGED=1 ;;
    1440|1440p) apply_res 1440 && DISPLAY_CHANGED=1 ;;
  esac
  [ -n "$DISPLAY_CHANGED" ] && log "游戏分辨率已切换: $res_choice (无 HiDPI)"
else
  log "WARN: displayplacer 缺失, 分辨率不自动切换"
fi

# ---------- 启动: 前台等 Simulator, 退出后还原 HDR/分辨率 ----------
# 不 exec — launcher 驻留为 Simulator 父进程, 退出后由自己还原 (watchdog 会被 Simulator 清算, 不可用)
log "启动 Simulator..."
if [ -n "$DYLD_INSERT" ]; then
  arch -x86_64 env DYLD_INSERT_LIBRARIES="$DYLD_INSERT" "$SIMULATOR" &
else
  arch -x86_64 "$SIMULATOR" &
fi
SIM_PID=$!

# 事件驱动等 Simulator 结束; 若它 daemonize 则按进程名排空兜底
wait "$SIM_PID" 2>/dev/null
while pgrep -x Simulator >/dev/null 2>&1; do sleep 0.5; done

log "Simulator 已退出, 还原 HDR"
restore_display
log "launcher 结束"
