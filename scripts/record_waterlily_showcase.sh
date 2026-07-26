#!/usr/bin/env bash
# Record a continuous WaterLily showcase: hot-switch every simulation case,
# cycle the fan-requested palettes (fluent / sith / mica), and drive the mouse
# through the stylus case so the pointer-chasing cylinder shows itself.
#
# One take, one .mp4, plus a .chapters.csv (offset seconds, label) for editing
# and subtitles. Everything is driven over jwm IPC — no manual hotkeys.
#
# Requirements: a running jwm session with the compositor active, jwm-tool
# built, julia with the waterlily project instantiated, xdotool for the mouse
# segments (skipped with a warning if missing).
#
# Environment overrides:
#   OUT_DIR       output directory       (default: ~/Videos/jwm-waterlily)
#   SIM_SIZE      worker --sim-size      (default: 1280x800)
#   WORKER_FPS    worker --fps           (default: 30)
#   CASE_DWELL    seconds per case       (default: 12)
#   PALETTE_DWELL seconds per palette    (default: 10)
#   STYLUS_DWELL  seconds of mouse play  (default: 20)

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${OUT_DIR:-$HOME/Videos/jwm-waterlily}"
SIM_SIZE="${SIM_SIZE:-1280x800}"
WORKER_FPS="${WORKER_FPS:-30}"
CASE_DWELL="${CASE_DWELL:-12}"
PALETTE_DWELL="${PALETTE_DWELL:-10}"
STYLUS_DWELL="${STYLUS_DWELL:-20}"

STAMP="$(date +%Y%m%d-%H%M%S)"
OUT_FILE="$OUT_DIR/waterlily-showcase-$STAMP.mp4"
CHAPTERS="$OUT_DIR/waterlily-showcase-$STAMP.chapters.csv"
WORKER_LOG="$OUT_DIR/waterlily-worker-$STAMP.log"
# 冷启动的 GPU 探测/编译可能要几分钟;已有进程未连接时给 STALE_GRACE 秒机会。
WORKER_WAIT="${WORKER_WAIT:-240}"
STALE_GRACE="${STALE_GRACE:-45}"

# 展示顺序:经典卡门涡街开场,游走类推进,最后进入交互 stylus。
CASES=(cylinder tandem diamond dance flap orbit hover wander)
# 粉丝点名的三个配色,在涡街最丰富的 cylinder 上循环展示。
PALETTES=(fluent sith mica)

log() { printf '[showcase %s] %s\n' "$(date +%H:%M:%S)" "$*"; }

# --- jwm-tool resolution -----------------------------------------------------

find_jwm_tool() {
    local candidate
    for candidate in "$REPO/target/release/jwm-tool" "$REPO/target/debug/jwm-tool"; do
        [[ -x "$candidate" ]] && { printf '%s' "$candidate"; return; }
    done
    command -v jwm-tool || {
        echo "jwm-tool not found; build it with: cargo build --release --bin jwm-tool" >&2
        exit 1
    }
}
# JWM_TOOL 环境变量可覆盖(测试桩或非常规安装路径)。
JWM_TOOL="${JWM_TOOL:-$(find_jwm_tool)}"

ipc() { "$JWM_TOOL" msg "$@" >/dev/null; }
ipc_query() { "$JWM_TOOL" msg "$1" --raw 2>/dev/null || true; }

waterlily_flag() { # waterlily_flag enabled|active|worker_connected
    ipc_query get_waterlily_status | grep -q "\"$1\"[[:space:]]*:[[:space:]]*true"
}

# --- chapter timeline --------------------------------------------------------

RECORD_EPOCH=""
chapter() {
    local offset
    offset="$(awk -v now="$(date +%s.%N)" -v start="$RECORD_EPOCH" 'BEGIN { printf "%.1f", now - start }')"
    printf '%s,%s\n' "$offset" "$1" >>"$CHAPTERS"
    log "chapter @${offset}s: $1"
}

# --- mouse trajectories (stylus case) ----------------------------------------

HAVE_XDOTOOL=0
command -v xdotool >/dev/null && HAVE_XDOTOOL=1

screen_geometry() { xdotool getdisplaygeometry; }

# Sample a parametric path at ~30 Hz for a given number of seconds.
# trace <seconds> <awk-expression producing "x y" from t in [0,1]>
trace() {
    local seconds="$1" expr="$2" width height steps i
    read -r width height < <(screen_geometry)
    steps=$((seconds * 30))
    for ((i = 0; i < steps; i++)); do
        # 末尾 print "" 补换行:read 在无换行的 EOF 上返回非零,会触发 set -e。
        read -r x y < <(awk -v t="$(awk -v i="$i" -v n="$steps" 'BEGIN { printf "%.6f\n", i / n }')" \
            -v W="$width" -v H="$height" "BEGIN { $expr; print \"\" }")
        xdotool mousemove "$x" "$y"
        sleep 0.033
    done
}

# 圆周 → 8 字 → 横扫:先甩出稳定涡环,再写个 8,最后拖出一条长尾迹。
stylus_performance() {
    # 注意:不能与 seconds 同一条 local——同条声明里 $(()) 先于赋值展开。
    local seconds="$1"
    local third=$((seconds / 3))
    ((HAVE_XDOTOOL)) || { log "xdotool missing; stylus segment plays without mouse motion"; sleep "$seconds"; return; }
    trace "$third" 'a = 2 * 3.14159265 * 2 * t; printf "%d %d", W/2 + W*0.28*cos(a), H/2 + H*0.30*sin(a)'
    trace "$third" 'a = 2 * 3.14159265 * 2 * t; printf "%d %d", W/2 + W*0.32*sin(a), H/2 + H*0.28*sin(2*a)'
    trace "$third" 'printf "%d %d", W*0.08 + W*0.84*t, H/2 + H*0.10*sin(2*3.14159265*3*t)'
}

# --- worker lifecycle --------------------------------------------------------

worker_pids() { pgrep -f "julia.*waterlily/runner" || true; }

# TERM first, KILL after a grace period: a worker stuck in shutdown (blocked
# exit-time task) would otherwise linger for hours and shadow future runs.
kill_workers() {
    local pids elapsed
    pids="$(worker_pids)"
    [[ -z "$pids" ]] && return
    # shellcheck disable=SC2086
    kill $pids 2>/dev/null || true
    for elapsed in {1..10}; do
        sleep 0.5
        [[ -z "$(worker_pids)" ]] && return
    done
    log "worker ignored SIGTERM; escalating to SIGKILL"
    # shellcheck disable=SC2086
    kill -9 $(worker_pids) 2>/dev/null || true
}

# 唯一可信的健康信号是压缩器视角的 worker_connected;pgrep 只能证明有进程,
# 证明不了它还活着(卡死的退出中进程照样被 pgrep 抓到)。
wait_for_worker() {
    local deadline=$((SECONDS + $1))
    while ((SECONDS < deadline)); do
        waterlily_flag worker_connected && return 0
        sleep 1
    done
    return 1
}

SPAWNED_WORKER=0
ensure_worker() {
    # 任一控制命令都会触发压缩器懒重绑 wake socket(重启竞态自愈)。
    ipc waterlily_case --args '"cylinder"' || true
    if waterlily_flag worker_connected; then
        log "WaterLily worker already connected"
        return
    fi
    if [[ -n "$(worker_pids)" ]]; then
        log "worker process exists but is not connected; waiting up to ${STALE_GRACE}s"
        if wait_for_worker "$STALE_GRACE"; then
            log "worker connected"
            return
        fi
        log "replacing unresponsive worker"
        kill_workers
    fi
    log "starting WaterLily worker (--sim-size $SIM_SIZE --fps $WORKER_FPS, device auto)"
    nohup julia --project="$REPO/waterlily" "$REPO/waterlily/runner.jl" \
        --device auto --fps "$WORKER_FPS" --sim-size "$SIM_SIZE" \
        >"$WORKER_LOG" 2>&1 &
    SPAWNED_WORKER=1
    # GPU probe + package load can take minutes on a cold cache.
    local deadline=$((SECONDS + WORKER_WAIT))
    until waterlily_flag worker_connected; do
        if ((SECONDS >= deadline)); then
            echo "worker did not connect within ${WORKER_WAIT}s; log tail:" >&2
            tail -20 "$WORKER_LOG" >&2
            exit 1
        fi
        if [[ -z "$(worker_pids)" ]]; then
            echo "worker exited early; log tail:" >&2
            tail -20 "$WORKER_LOG" >&2
            exit 1
        fi
        sleep 1
    done
    log "worker connected and publishing"
}

# --- cleanup -----------------------------------------------------------------

RECORDING=0
EFFECT_TOGGLED=0
cleanup() {
    set +e
    ((RECORDING)) && ipc stop_recording
    ipc waterlily_palette --args '"auto"'
    ((EFFECT_TOGGLED)) && ipc toggle_waterlily
    ((SPAWNED_WORKER)) && kill_workers
}
trap cleanup EXIT

# --- showcase ----------------------------------------------------------------

mkdir -p "$OUT_DIR"

# 一次探测覆盖三件事:jwm 在跑、compositor 激活、构建带 waterlily 状态查询。
if ! ipc_query get_waterlily_status | grep -q '"success"[[:space:]]*:[[:space:]]*true'; then
    echo "get_waterlily_status failed — jwm 未运行、compositor 未激活或二进制过旧:" >&2
    ipc_query get_waterlily_status >&2
    exit 1
fi

ensure_worker

# 按查询到的真实状态决定是否开启特效,结束时恢复原状。
if waterlily_flag enabled; then
    log "WaterLily effect already enabled"
else
    ipc toggle_waterlily
    EFFECT_TOGGLED=1
fi
ipc waterlily_palette --args '"auto"'
ipc waterlily_case --args '"cylinder"'
((HAVE_XDOTOOL)) && read -r W H < <(screen_geometry) && xdotool mousemove "$((W - 4))" "$((H - 4))"

# 等 worker 的帧真正上屏(active 含义:特效开 + worker 连接 + 纹理在屏)。
deadline=$((SECONDS + 60))
until waterlily_flag active; do
    if ((SECONDS >= deadline)); then
        echo "WaterLily layer did not become active within 60s" >&2
        ipc_query get_waterlily_status >&2
        exit 1
    fi
    sleep 0.5
done
sleep 2 # 让第一帧涡街铺开

log "recording to $OUT_FILE"
: >"$CHAPTERS"
ipc start_recording --args "{\"path\": \"$OUT_FILE\"}"
RECORDING=1
RECORD_EPOCH="$(date +%s.%N)"

# 第一幕:八种经典流场巡礼
for case_name in "${CASES[@]}"; do
    ipc waterlily_case --args "\"$case_name\""
    chapter "case:$case_name"
    sleep "$CASE_DWELL"
done

# 第二幕:回到卡门涡街,循环粉丝配色
ipc waterlily_case --args '"cylinder"'
sleep 2
for palette in "${PALETTES[@]}"; do
    ipc waterlily_palette --args "\"$palette\""
    chapter "palette:$palette"
    sleep "$PALETTE_DWELL"
done
ipc waterlily_palette --args '"auto"'

# 第三幕:stylus 交互——鼠标画圆、写 8、拖尾迹
ipc waterlily_case --args '"stylus"'
chapter "case:stylus (fluent, mouse-driven)"
sleep 2
stylus_performance "$STYLUS_DWELL"

# 终幕:云母珠光里的 stylus
ipc waterlily_palette --args '"mica"'
chapter "finale:stylus + mica shimmer"
sleep 2
stylus_performance "$STYLUS_DWELL"

# --- finalize ----------------------------------------------------------------

ipc stop_recording
RECORDING=0
log "waiting for the encoder to finalize"
deadline=$((SECONDS + 30))
until ipc_query get_recording_status | grep -q '"finalized"[[:space:]]*:[[:space:]]*true'; do
    if ((SECONDS >= deadline)); then
        echo "recording was not finalized within 30s" >&2
        exit 1
    fi
    sleep 0.3
done

log "done: $OUT_FILE"
log "chapters: $CHAPTERS"
if command -v ffprobe >/dev/null; then
    ffprobe -v error -show_entries format=duration -of default=nw=1 "$OUT_FILE"
fi
