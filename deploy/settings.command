#!/bin/zsh
# 燕云设置中心 — 统一管理后端 / 插帧 / 显示开关
# 配置存 ~/lsfg-metal/yanyun.conf, 与 launcher 共用
# v2: [4] 插帧直接翻转 (原 4/5 合并); [7] 原生节奏一键开关 (原 8 的子菜单取消)

CFG="$HOME/lsfg-metal"
CONF="$CFG/yanyun.conf"
LOG="$HOME/Library/Logs/yanyun-game.log"

c_reset=$'\e[0m'; c_g=$'\e[32m'; c_y=$'\e[33m'; c_r=$'\e[31m'; c_b=$'\e[36m'

get() { sed -n "s/^$1=//p" "$CONF" 2>/dev/null | tail -1 | tr -d ' ' | head -c 20; }
setv() { sed -i '' "s/^$1=.*/$1=$2/" "$CONF" 2>/dev/null; }

show_status() {
  local backend=$(get backend) lsfg=$(get lsfg) hdr=$(get hdr) db=$(get deadband) res=$(get resolution) pac=$(get pacer)
  echo ""
  echo "${c_b}════════ 燕云设置中心 ════════${c_reset}"
  echo ""
  echo "  ${c_b}渲染后端${c_reset}"
  case "$backend" in
    d3dmetal) echo "    [1] d3dmetal   ${c_g}← 当前 (GPTK 直译, 性能最好)${c_reset}" ;;
    *)        echo "    [1] d3dmetal   (GPTK 直译, 性能最好)" ;;
  esac
  case "$backend" in
    dxmt)     echo "    [2] dxmt       ${c_y}← 当前 (Mesa 译 DX11, 慢一些)${c_reset}" ;;
    *)        echo "    [2] dxmt       (Mesa 译 DX11, 慢一些)" ;;
  esac
  case "$backend" in
    wined3d)  echo "    [3] wined3d    ${c_y}← 当前 (最保守, 诊断用)${c_reset}" ;;
    *)        echo "    [3] wined3d    (最保守, 诊断用)" ;;
  esac
  echo ""
  echo "  ${c_b}lsfg-metal 插帧${c_reset}"
  if [ "$lsfg" = "on" ]; then
    echo "    [4] 插帧         ${c_g}← 当前: 开${c_reset}  (30→60fps, 2x)"
  else
    echo "    [4] 插帧         ${c_r}← 当前: 关${c_reset}  (原生帧率)"
  fi
  if [ "$db" = "on" ]; then
    echo "    [11] HDR 去噪    ${c_g}← 当前: 开${c_reset}  (deadband, 10-bit 直线)"
  else
    echo "    [11] HDR 去噪    ${c_r}← 当前: 关${c_reset}"
  fi
  echo ""
  echo "  ${c_b}显示${c_reset}"
  if [ "$hdr" = "on" ]; then
    echo "    [5] 启动切 HDR   ${c_g}← 当前: 开${c_reset}  (进游戏自动开, 退出自动关)"
  else
    echo "    [5] 启动切 HDR   ${c_r}← 当前: 关${c_reset}"
  fi
  case "$res" in
    1080) echo "    [6] 游戏分辨率  ${c_g}← 当前: 1080 无HiDPI${c_reset}  (退出还原 1440 HiDPI)" ;;
    *)    echo "    [6] 游戏分辨率  ${c_g}← 当前: 1440 无HiDPI${c_reset}  (退出还原 1440 HiDPI)" ;;
  esac
  echo ""
  echo "  ${c_b}原生模式 (lsfg=off 时生效)${c_reset}"
  if [ "$pac" = "30" ]; then
    echo "    [7] 原生30帧节奏 ${c_g}← 当前: 开${c_reset}  (fork 槽位锁相, 游戏内不限帧)"
  else
    echo "    [7] 原生30帧节奏 ${c_r}← 当前: 关${c_reset}  (原生自由跑)"
  fi
  echo ""
  echo "  ${c_b}其他${c_reset}"
  echo "    [8] 查看插帧统计 (source→total fps)"
  echo "    [9] 卡死取证     (抓进程栈+GPU 状态到桌面)"
  echo "    [10] 看启动日志  (最近 15 行)"
  echo "    [q] 退出"
  echo ""
}

act() {
  case "$1" in
    1) setv backend d3dmetal; echo "${c_g}✓ 后端 → d3dmetal${c_reset}" ;;
    2) setv backend dxmt;     echo "${c_g}✓ 后端 → dxmt${c_reset}" ;;
    3) setv backend wined3d;  echo "${c_g}✓ 后端 → wined3d${c_reset}" ;;
    4)
      local cur=$(get lsfg)
      if [ "$cur" = "on" ]; then
        setv lsfg off; echo "${c_g}✓ 插帧 → OFF${c_reset} (原生帧率)"
      else
        setv lsfg on;  echo "${c_g}✓ 插帧 → ON${c_reset} (30→60fps, 2x)"
      fi ;;
    5)
      local cur=$(get hdr)
      if [ "$cur" = "on" ]; then
        setv hdr off; echo "${c_g}✓ 启动切 HDR → 关${c_reset}"
      else
        setv hdr on;  echo "${c_g}✓ 启动切 HDR → 开${c_reset}"
      fi ;;
    6)
      local cur=$(get resolution)
      if [ "$cur" = "1080" ]; then
        setv resolution 1440; echo "${c_g}✓ 游戏分辨率 → 1440 无 HiDPI${c_reset}"
      else
        setv resolution 1080; echo "${c_g}✓ 游戏分辨率 → 1080 无 HiDPI${c_reset}"
      fi ;;
    7)
      local cur=$(get pacer)
      if [ "$cur" = "30" ]; then
        setv pacer off; echo "${c_g}✓ 原生30帧节奏 → 关${c_reset}"
      else
        setv pacer 30;  echo "${c_g}✓ 原生30帧节奏 → 开${c_reset} (游戏内不限帧, pacer 独管辖)"
      fi ;;
    11)
      local cur=$(get deadband)
      if [ "$cur" = "on" ]; then
        setv deadband off; echo "${c_g}✓ HDR 去噪 → 关${c_reset}"
      else
        setv deadband on;  echo "${c_g}✓ HDR 去噪 → 开${c_reset} (10-bit 直线实验)"
      fi ;;
    8) echo ""; tail -5 "$CFG/lsfg.log" 2>/dev/null | grep -E "source|total|fps" | tail -3 ;;
    9) zsh "$CFG/freeze-capture.command" ;;
    10) echo ""; tail -15 "$LOG" 2>/dev/null ;;
  esac
}

while true; do
  show_status
  printf "  选择: "
  read -r k
  [ "$k" = "q" ] && exit 0
  act "$k"
done
