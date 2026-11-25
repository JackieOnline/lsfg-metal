#!/bin/zsh
# 卡死/闪退现场取证: 采样 wine/yysls 进程栈 + GPU 状态, 存桌面
ts=$(date +%H%M%S)
out="$HOME/Desktop/yanyun-freeze-$ts"
mkdir -p "$out"
echo ">> 抓取中 → $out"
ps aux | grep -iE "wine|yysls|FeverGames" | grep -v grep > "$out/ps.txt"
for pid in $(pgrep -fi "yysls.exe|FeverGamesLauncher|preloader"); do
  sample "$pid" 3 -file "$out/sample-$pid.txt" 2>/dev/null
done
ps -A -o pid,pcpu,pmem,comm | grep -iE "WindowServer|wine" > "$out/gpu-procs.txt"
powermetrics --samplers gpu_power -i 1 -n 1 2>/dev/null | head -40 > "$out/gpu-power.txt" 2>/dev/null
echo ">> 完成: $out"
