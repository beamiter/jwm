#!/usr/bin/env bash
# Record a continuous WaterLily showcase: hot-switch every simulation case,
# cycle the fan-requested palettes (fluent / sith / mica), and drive the mouse
# through the interactive cases — the pointer-chasing stylus and the waltz
# finale, whose heaving body trails dance's braided wake behind the cursor.
#
# One take, one .mp4, plus a .chapters.csv (offset seconds, label) for editing
# and subtitles. Everything is driven over jwm IPC — no manual hotkeys.
#
# Requirements: a running jwm session with the compositor active, jwm-tool
# built, julia with the waterlily project instantiated, xdotool for the mouse
# segments (skipped with a warning if missing).
#
# Usage: record_waterlily_showcase.sh [acts]
#   acts  comma-separated subset of: cases,palettes,stylus,waltz (default: all)
#   e.g. `record_waterlily_showcase.sh stylus` records only the stylus act.
#
# Environment overrides:
#   OUT_DIR       output directory       (default: ~/Videos/jwm-waterlily)
#   SIM_SIZE      worker --sim-size      (default: 1280x800)
#   WORKER_FPS    worker --fps           (default: 30)
#   CASE_DWELL    seconds per case       (default: 12)
#   PALETTE_DWELL seconds per palette    (default: 10)
#   STYLUS_DWELL  seconds per stylus palette (fluent/sith/mica) (default: 40)
#   WALTZ_DWELL   seconds per waltz palette (fluent/sith/mica) (default: 40)

set -euo pipefail

ACTS="${1:-all}"
wants() { [[ "$ACTS" == "all" || ",$ACTS," == *",$1,"* ]]; }
for act in ${ACTS//,/ }; do
    case "$act" in
    all | cases | palettes | stylus | waltz) ;;
    *)
        echo "unknown act '$act'; valid: cases,palettes,stylus,waltz (or all)" >&2
        exit 1
        ;;
    esac
done

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${OUT_DIR:-$HOME/Videos/jwm-waterlily}"
SIM_SIZE="${SIM_SIZE:-1280x800}"
# 轨迹速度在仿真网格坐标系里才有意义(像素→网格的 x/y 比例不同)。
GRID_W="${SIM_SIZE%x*}"
GRID_H="${SIM_SIZE#*x}"
WORKER_FPS="${WORKER_FPS:-30}"
CASE_DWELL="${CASE_DWELL:-12}"
PALETTE_DWELL="${PALETTE_DWELL:-10}"
STYLUS_DWELL="${STYLUS_DWELL:-40}"
WALTZ_DWELL="${WALTZ_DWELL:-40}"

STAMP="$(date +%Y%m%d-%H%M%S)"
OUT_FILE="$OUT_DIR/waterlily-showcase-$STAMP.mp4"
CHAPTERS="$OUT_DIR/waterlily-showcase-$STAMP.chapters.csv"
WORKER_LOG="$OUT_DIR/waterlily-worker-$STAMP.log"
# 冷启动的 GPU 探测/编译可能要几分钟;已有进程未连接时给 STALE_GRACE 秒机会。
WORKER_WAIT="${WORKER_WAIT:-240}"
STALE_GRACE="${STALE_GRACE:-45}"
# CUDA 连上下文都建不起来时抛的是 OOM 堆栈;起飞前先确认有这么多空闲显存(MiB)。
GPU_MIN_FREE_MB="${GPU_MIN_FREE_MB:-3000}"

# 展示顺序:经典卡门涡街开场,游走类推进,交互 stylus 承接,waltz 压轴。
CASES=(cylinder tandem diamond dance flap orbit hover wander)
# 粉丝点名的三个配色,在涡街最丰富的 cylinder 上循环展示。
PALETTES=(mica sith fluent)
# 交互两幕(stylus/waltz)的配色轮换:每种一段。第二幕以 fluent 收尾,
# stylus 以 fluent 开场,幕间无切换,丝滑衔接。
INTERACTIVE_PALETTES=(fluent sith mica)

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

# Play "x y" sample lines from stdin as 30 Hz pointer motion.
play_samples() {
    local x y
    while read -r x y; do
        xdotool mousemove "$x" "$y"
        sleep 0.033
    done
}

# Sample a parametric path at ~30 Hz for a given number of seconds.
# trace <seconds> <awk-statements printing "x y\n" from t in [0,1]>
trace() {
    local seconds="$1" expr="$2" width height
    read -r width height < <(screen_geometry)
    awk -v W="$width" -v H="$height" -v N="$((seconds * 30))" \
        "BEGIN { for (i = 0; i < N; i++) { t = i / N; $expr } }" | play_samples
}

# 海螺线:光标从屏幕中心旋转扩散到外缘,目标是让圆柱的尾迹以最快速度铺满
# 画布。三个铺满效率的设计,都以圆柱直径 D(= 速度/长度尺度 L)为标尺:
#   1. 速度恒定 1.35U——弧长上限被弹簧 1.5U 焊死,恒速即整段预算全在铺新水,
#      靠单遍细步长扫描 + 弧长反演做到严格恒速(解析参数化在角上会超速 60%);
#   2. p=4 超椭圆环,贴合屏幕长宽比——圆环螺线永远够不到四角;
#   3. 环距 ≈2D,即卡门涡街的横向宽度——环距再密就是在重复搅已有涡的水。
# 同预算下尾迹覆盖率从椭圆螺线的 45% 提到 ~78%。每段开头光标瞬移回中心,
# 弹簧拖一笔径向直线穿过上一圈螺线,像换页笔画。
stylus_performance() {
    local seconds="$1" width height
    ((HAVE_XDOTOOL)) || { log "xdotool missing; stylus segment plays without mouse motion"; sleep "$seconds"; return; }
    read -r width height < <(screen_geometry)
    awk -v W="$width" -v H="$height" -v N="$((seconds * 30))" \
        -v GW="$GRID_W" -v GH="$GRID_H" '
    function boundary(th,    c, s, ac, as) {
        # 沿方向 th 从中心到 p=4 超椭圆边界的距离(网格坐标)。
        c = cos(th); s = sin(th)
        ac = (c < 0 ? -c : c); as = (s < 0 ? -s : s)
        return 1 / ((ac / Ax)^4 + (as / Ay)^4)^0.25
    }
    BEGIN {
        D = 0.10 * GH                # 圆柱直径 = 仿真的长度尺度 L
        Ax = 0.42 * GW; Ay = 0.42 * GH
        B = 1.35 * D * N / 30.0      # 弧长预算:1.35U 匀速走 N/30 秒
        dth = 0.001
        # 第一遍:未归一化螺线 Q(th) = th·boundary(th)·(cos th, sin th) 的
        # 弧长 L_Q;真实螺线 P = Q/thmax,故 thmax 满足 L_Q(thmax)/thmax = B。
        A = 0; th = dth; pqx = 0; pqy = 0
        while (1) {
            dd = boundary(th)
            qx = th * dd * cos(th); qy = th * dd * sin(th)
            A += sqrt((qx - pqx)^2 + (qy - pqy)^2); pqx = qx; pqy = qy
            if (A / th >= B) break
            th += dth
        }
        thmax = th
        # 第二遍:同一扫描,弧长每跨过总长的 1/N 就发一个采样点——严格恒速。
        A = 0; th = dth; pqx = 0; pqy = 0
        print int(W / 2), int(H / 2); k = 1
        while (k < N) {
            dd = boundary(th)
            qx = th * dd * cos(th); qy = th * dd * sin(th)
            A += sqrt((qx - pqx)^2 + (qy - pqy)^2); pqx = qx; pqy = qy
            while (k < N && A / thmax >= k * B / N) {
                r = th / thmax
                print int(W * (GW / 2 + r * dd * cos(th)) / GW), \
                      int(H * (GH / 2 + r * dd * sin(th)) / GH)
                k++
            }
            th += dth
        }
    }' | play_samples
}

# 慢速华尔兹:大椭圆一圈,再顺流横渡。waltz 的身体自带横向摆动,均匀来流把
# 编织尾迹从光标向下游拖出,所以轨迹要比 stylus 慢,给尾迹留出铺开的时间。
waltz_performance() {
    local seconds="$1"
    local half=$((seconds / 2))
    ((HAVE_XDOTOOL)) || { log "xdotool missing; waltz segment plays without mouse motion"; sleep "$seconds"; return; }
    trace "$half" 'a = 2 * 3.14159265 * t; printf "%d %d\n", W/2 + W*0.30*cos(a), H/2 + H*0.22*sin(a)'
    trace "$half" 'printf "%d %d\n", W*0.15 + W*0.70*t, H/2 + H*0.18*sin(2*3.14159265*t)'
}

# --- worker lifecycle --------------------------------------------------------

worker_pids() { pgrep -f "julia.*waterlily/runner" || true; }

# TERM first, KILL after a grace period: a worker stuck in shutdown (blocked
# exit-time task) would otherwise linger for hours and shadow future runs.
kill_workers() {
    local -a pids=()
    mapfile -t pids < <(worker_pids)
    ((${#pids[@]} == 0)) && return
    kill -- "${pids[@]}" 2>/dev/null || true
    for _ in {1..10}; do
        sleep 0.5
        mapfile -t pids < <(worker_pids)
        ((${#pids[@]} == 0)) && return
    done
    log "worker ignored SIGTERM; escalating to SIGKILL"
    kill -9 -- "${pids[@]}" 2>/dev/null || true
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

# 显存被别的进程占满时,worker 会在 cuDevicePrimaryCtxRetain 就 OOM 崩掉;
# 与其让用户读 Julia 堆栈,不如起飞前点名占显存的进程。
gpu_preflight() {
    command -v nvidia-smi >/dev/null 2>&1 || return 0
    local free_mb
    free_mb="$(nvidia-smi --query-gpu=memory.free --format=csv,noheader,nounits 2>/dev/null | head -1)" || return 0
    [[ "$free_mb" =~ ^[0-9]+$ ]] || return 0
    if ((free_mb < GPU_MIN_FREE_MB)); then
        echo "GPU has only ${free_mb}MiB free (need ${GPU_MIN_FREE_MB}MiB); current consumers:" >&2
        nvidia-smi --query-compute-apps=pid,process_name,used_memory --format=csv >&2 || true
        echo "free GPU memory (or lower GPU_MIN_FREE_MB) and re-run" >&2
        exit 1
    fi
    log "GPU preflight OK (${free_mb}MiB free)"
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
    gpu_preflight
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
# 开场就切到第一幕会用的 case,避免录进一段无关的开机画面。
OPENING_CASE=cylinder
if ! wants cases && ! wants palettes; then
    if wants stylus; then OPENING_CASE=stylus; else OPENING_CASE=waltz; fi
fi
ipc waterlily_palette --args '"auto"'
ipc waterlily_case --args "\"$OPENING_CASE\""
# 非交互开场把光标藏进角落;交互开场停在中心,海螺线正是从那里出发。
if ((HAVE_XDOTOOL)); then
    read -r W H < <(screen_geometry)
    if [[ "$OPENING_CASE" == cylinder ]]; then
        xdotool mousemove "$((W - 4))" "$((H - 4))"
    else
        xdotool mousemove "$((W / 2))" "$((H / 2))"
    fi
fi

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
if wants cases; then
    for case_name in "${CASES[@]}"; do
        ipc waterlily_case --args "\"$case_name\""
        chapter "case:$case_name"
        sleep "$CASE_DWELL"
    done
fi

# 第二幕:回到卡门涡街,循环粉丝配色
if wants palettes; then
    ipc waterlily_case --args '"cylinder"'
    sleep 2
    for palette in "${PALETTES[@]}"; do
        ipc waterlily_palette --args "\"$palette\""
        chapter "palette:$palette"
        sleep "$PALETTE_DWELL"
    done
    ipc waterlily_palette --args '"auto"'
fi

# 第三幕:stylus 交互长段——三种粉丝配色各 STYLUS_DWELL 秒,每种画一幅
# 完整海螺。
if wants stylus; then
    ipc waterlily_case --args '"stylus"'
    sleep 2
    for palette in "${INTERACTIVE_PALETTES[@]}"; do
        ipc waterlily_palette --args "\"$palette\""
        chapter "stylus:$palette (mouse-driven)"
        stylus_performance "$STYLUS_DWELL"
    done
fi

# 终幕:waltz——dance 的横摆骑在追踪弹簧上,编织尾迹追着鼠标跑;同一套
# 配色轮换,每种 WALTZ_DWELL 秒完整跳一支(椭圆一圈 + 顺流横渡)。
if wants waltz; then
    ipc waterlily_case --args '"waltz"'
    sleep 2
    for palette in "${INTERACTIVE_PALETTES[@]}"; do
        ipc waterlily_palette --args "\"$palette\""
        chapter "waltz:$palette (mouse-led braided wake)"
        waltz_performance "$WALTZ_DWELL"
    done
fi

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
