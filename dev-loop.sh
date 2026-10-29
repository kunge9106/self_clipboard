#!/bin/sh
# 开发环境守护：崩溃后自动拉起；用户主动退出（15 秒内结束）则不再重启。
cd /e/Self-Project/self_clipboard || exit 1
while true; do
  start=$(date +%s)
  npm run tauri:dev
  end=$(date +%s)
  if [ $((end - start)) -lt 15 ]; then
    echo "[dev-loop] 进程很快结束（$((end - start))s），视为用户主动退出，不再重启。"
    break
  fi
  echo "[dev-loop] 进程异常退出，2 秒后重启。"
  sleep 2
done
