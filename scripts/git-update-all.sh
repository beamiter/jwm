#!/usr/bin/env bash
# 一键更新目录下所有 git 仓库，并按各自的方式重新编译已知项目。
# 默认行为：fetch --prune 后对当前分支做 fast-forward，只要有任何风险就跳过而不是硬来；
# 更新完成后，对本次真的有新提交（或产物缺失）的已知项目跑一次 release 构建。
set -uo pipefail

ROOT="."
JOBS=4
MODE="ff"        # ff | rebase | merge
PRUNE=1
STASH=0
DRY_RUN=0
QUIET=0
PUSH=0
BUILD=1          # 更新后是否编译
BUILD_ALL=0      # 1 = 已知项目全部编译，不管有没有更新
BUILD_JOBS=1     # 同时编译几个项目；cargo 内部已经并行，默认串行
ONLY=""          # 逗号分隔的项目名白名单

# 传给各仓库安装脚本的额外参数，例如 JWM_INSTALL_ARGS='-b xcb_bar --skip-bar'
# 或 ANVIL_INSTALL_ARGS='--backend cargo --no-desktop'
JWM_INSTALL_ARGS="${JWM_INSTALL_ARGS:-}"
ANVIL_INSTALL_ARGS="${ANVIL_INSTALL_ARGS:-}"
EMBER_INSTALL_ARGS="${EMBER_INSTALL_ARGS:-}"
FORGE_INSTALL_ARGS="${FORGE_INSTALL_ARGS:-}"
FROST_INSTALL_ARGS="${FROST_INSTALL_ARGS:-}"
# jsh 在 Linux 上的安装形态是静态 musl 二进制（jsh 仓库 scripts/install-jsh.sh 的原话：
# 一个动态链接的 jsh 没法被 bind-mount 进容器、也没法推到 ssh 主机上）。
# 默认 target 给的是 glibc 动态版本，所以这里显式指定三元组；想要 glibc 就
# 明确写 JSH_INSTALL_TARGET=x86_64-unknown-linux-gnu，和 install-jsh.sh 一致。
JSH_TRIPLE=""
JSH_CC=""

usage() {
    cat <<'EOF'
用法: git-update-all.sh [选项] [目录]

更新选项:
  -j N          并发数 (默认 4)
  -r            用 git pull --rebase 代替 fast-forward
  -m            用 git pull --no-rebase 代替 fast-forward (允许产生 merge commit)
  -s            工作区有改动时自动 stash，更新后再 stash pop
  -n            dry-run: 只 fetch 和汇报，不改动工作区也不编译
  -P            不加 --prune
  -u            双向更新：拉取之后，如本地领先则自动 push 到 upstream
  -q            只输出汇总表

构建选项 (只作用于已知项目: jsh anvil ember frost forge jwm):
  -B            全部重新构建，不管本次有没有拉到新提交
  -N            不构建，只更新仓库
  -T 列表       只处理这些项目，逗号分隔，如 -T anvil,jwm
  -J N          同时构建几个项目 (默认 1；cargo 内部已经并行)
  -h            显示本帮助

默认目录为当前目录，扫描其下一层的子目录。
不在已知项目表里的仓库只更新、不构建。默认只处理本次真的有新提交、
或者产物不存在的项目。

各项目的构建方式并不相同，脚本里按名字分派:
  anvil, ember,    scripts/install.sh —— 各仓库自带的安装脚本，构建加安装一步到位。
  forge, frost     后端 (nix / cargo) 由脚本自己按 --backend auto 挑，二进制默认装到
                   ~/.cargo/bin，装在 $HOME 下，不需要 sudo。
  jwm              scripts/install_jwm_scripts.sh —— jwm 没有只编不装的中间态：
                   裸 cargo build 只出根 package 的四个二进制，不编 bar 和
                   jwm-bridge，也不同步到 /usr/local/bin，更新完跑的还是旧的
                   那份。所以这里直接跑安装脚本，它自带构建。需要 sudo。
  jsh              cargo build --target <arch>-unknown-linux-musl (静态 musl)

只有 jsh 是纯编译不安装；要装它请运行 jsh 仓库的 scripts/install-jsh.sh。
产物检测看的是各项目真正落地的位置: jwm 看 /usr/local/bin/jwm，四个终端看
安装脚本的目标目录 (默认 ~/.cargo/bin/<名字>，跟随 --prefix/--bin-dir/DESTDIR)，
jsh 看 target 下的 release 产物。

环境变量:
  ANVIL_INSTALL_ARGS 追加给 anvil/scripts/install.sh 的参数，如 '--backend cargo'
  EMBER_INSTALL_ARGS 同上，作用于 ember
  FORGE_INSTALL_ARGS 同上，作用于 forge
  FROST_INSTALL_ARGS 同上，作用于 frost
  JWM_INSTALL_ARGS   追加给 install_jwm_scripts.sh 的参数，如 '-b xcb_bar'
  JSH_INSTALL_TARGET 覆盖 jsh 的 target 三元组，例如 x86_64-unknown-linux-gnu
                     (即明确要求动态 glibc 版本；install-jsh.sh 也认这个变量)
  CARGO_TARGET_DIR   若设置，产物检测也跟着走这个目录

退出码: 0 全部成功；1 有仓库更新失败或编译失败。
EOF
}

while getopts ":j:J:T:rmsnPuqBNh" opt; do
    case "$opt" in
        j) JOBS="$OPTARG" ;;
        r) MODE="rebase" ;;
        m) MODE="merge" ;;
        s) STASH=1 ;;
        n) DRY_RUN=1 ;;
        P) PRUNE=0 ;;
        u) PUSH=1 ;;
        q) QUIET=1 ;;
        B) BUILD_ALL=1; BUILD=1 ;;
        N) BUILD=0 ;;
        T) ONLY="$OPTARG" ;;
        J) BUILD_JOBS="$OPTARG" ;;
        h) usage; exit 0 ;;
        \?) echo "未知选项: -$OPTARG" >&2; usage >&2; exit 2 ;;
        :)  echo "选项 -$OPTARG 需要参数" >&2; exit 2 ;;
    esac
done
shift $((OPTIND - 1))
[ $# -gt 0 ] && ROOT="$1"

if [ ! -d "$ROOT" ]; then
    echo "目录不存在: $ROOT" >&2
    exit 2
fi

case "$JOBS" in ''|*[!0-9]*|0) echo "-j 需要正整数" >&2; exit 2 ;; esac
case "$BUILD_JOBS" in ''|*[!0-9]*|0) echo "-J 需要正整数" >&2; exit 2 ;; esac

# dry-run 只汇报，不碰工作区，自然也不编译
[ "$DRY_RUN" -eq 1 ] && BUILD=0

# 编译要用到 cargo；非登录 shell 里 PATH 未必带上 ~/.cargo/bin
if [ "$BUILD" -eq 1 ] && ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
    # shellcheck source=/dev/null
    . "$HOME/.cargo/env"
fi

if [ -t 1 ]; then
    C_RST=$'\033[0m'; C_GRN=$'\033[32m'; C_YLW=$'\033[33m'
    C_RED=$'\033[31m'; C_DIM=$'\033[2m'; C_BLD=$'\033[1m'
else
    C_RST=''; C_GRN=''; C_YLW=''; C_RED=''; C_DIM=''; C_BLD=''
fi

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/git-update-all.XXXXXX")"
trap 'rm -rf "$WORKDIR"' EXIT

# 一个目录算不算仓库，以 git 自己的判断为准：光看 .git 存不存在会被空的 .git
# 目录（残留、误建）骗过去，一路走到 update_one 里才报"不是有效仓库"，还占着
# 一格计数。先要求 .git 存在，是为了挡住"父目录是仓库"导致的向上发现。
is_git_repo() {
    [ -e "$1/.git" ] || return 1
    git -C "$1" rev-parse --git-dir >/dev/null 2>&1
}

# 收集仓库：ROOT 本身若是仓库也算上，外加一层子目录
repos=()
# ROOT 存绝对路径：默认的 "." 会让汇总里的仓库名也变成 "."
is_git_repo "$ROOT" && repos+=("$(cd "$ROOT" && pwd)")
for d in "$ROOT"/*/; do
    is_git_repo "${d%/}" && repos+=("${d%/}")
done

if [ ${#repos[@]} -eq 0 ]; then
    echo "在 $ROOT 下没有找到 git 仓库" >&2
    exit 0
fi

# 单个仓库的更新逻辑。结果写入 $WORKDIR/<idx>.status ("状态|仓库名|说明")，
# 详细日志写入 $WORKDIR/<idx>.log，主进程按顺序回放，避免并发输出交错。
update_one() {
    local idx="$1" repo="$2"
    local name log status detail
    name="$(basename "$repo")"
    log="$WORKDIR/$idx.log"
    exec 3>"$log"

    report() {
        status="$1"; detail="$2"
        printf '%s|%s|%s\n' "$status" "$name" "$detail" >"$WORKDIR/$idx.status"
        exec 3>&-
        return 0
    }
    say() { printf '  %s\n' "$*" >&3; }

    printf '%s\n' "${C_BLD}==> $name${C_RST}" >&3

    if ! git -C "$repo" rev-parse --git-dir >/dev/null 2>&1; then
        say "${C_RED}不是有效的 git 仓库${C_RST}"
        report SKIP "不是有效仓库"; return
    fi

    local branch
    branch="$(git -C "$repo" symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
    if [ -z "$branch" ]; then
        say "${C_YLW}HEAD 处于 detached 状态，跳过${C_RST}"
        report SKIP "detached HEAD"; return
    fi

    if [ -z "$(git -C "$repo" remote)" ]; then
        say "${C_DIM}没有配置 remote，跳过${C_RST}"
        report SKIP "无 remote"; return
    fi

    # fetch
    local fetch_args=(--tags --quiet)
    [ "$PRUNE" -eq 1 ] && fetch_args+=(--prune)
    if ! git -C "$repo" fetch "${fetch_args[@]}" --all 2>>"$log"; then
        say "${C_RED}fetch 失败${C_RST}"
        report FAIL "fetch 失败"; return
    fi

    local upstream
    upstream="$(git -C "$repo" rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null || true)"
    if [ -z "$upstream" ]; then
        say "${C_YLW}分支 $branch 没有 upstream，跳过${C_RST}"
        report SKIP "$branch 无 upstream"; return
    fi

    local local_sha remote_sha
    local_sha="$(git -C "$repo" rev-parse HEAD)"
    remote_sha="$(git -C "$repo" rev-parse "$upstream")"

    if [ "$local_sha" = "$remote_sha" ]; then
        say "${C_DIM}已是最新 ($branch)${C_RST}"
        report OK "已最新"; return
    fi

    local ahead behind counts
    counts="$(git -C "$repo" rev-list --left-right --count "$upstream...HEAD" 2>/dev/null || echo "0	0")"
    behind="${counts%%	*}"; ahead="${counts##*	}"

    if [ "$DRY_RUN" -eq 1 ]; then
        say "落后 $behind / 领先 $ahead 个提交 (dry-run，未改动)"
        report DRY "落后 $behind 领先 $ahead"; return
    fi

    # 分叉且 ff 模式：既落后又领先，fast-forward 无法同时处理两个方向，交给用户决定
    if [ "$MODE" = "ff" ] && [ "$ahead" -gt 0 ] && [ "$behind" -gt 0 ]; then
        say "${C_YLW}本地领先 $ahead 个提交且落后 $behind 个，fast-forward 不适用，跳过${C_RST}"
        say "${C_DIM}如需合并请用 -r (rebase) 或 -m (merge)${C_RST}"
        report SKIP "本地领先 $ahead 落后 $behind"; return
    fi

    # 只领先、不落后：不需要拉取，直接跳到下面的 push 判断
    local need_pull=1
    if [ "$behind" -eq 0 ] && [ "$ahead" -gt 0 ]; then
        need_pull=0
    fi

    # 脏工作区处理（只有真的需要拉取时，脏工作区才是问题；纯 push 不受影响）
    local stashed=0
    if [ "$need_pull" -eq 1 ] && [ -n "$(git -C "$repo" status --porcelain 2>/dev/null)" ]; then
        if [ "$STASH" -eq 1 ]; then
            if git -C "$repo" stash push --include-untracked -m "git-update-all $(date +%F_%T)" >>"$log" 2>&1; then
                stashed=1
                say "已 stash 本地改动"
            else
                say "${C_RED}stash 失败${C_RST}"
                report FAIL "stash 失败"; return
            fi
        else
            say "${C_YLW}工作区有未提交改动，跳过（加 -s 可自动 stash）${C_RST}"
            report SKIP "工作区脏"; return
        fi
    fi

    if [ "$need_pull" -eq 1 ]; then
        local rc=0
        case "$MODE" in
            ff)     git -C "$repo" merge --ff-only "$upstream" >>"$log" 2>&1 || rc=$? ;;
            rebase) git -C "$repo" pull --rebase --quiet >>"$log" 2>&1 || rc=$? ;;
            merge)  git -C "$repo" pull --no-rebase --quiet >>"$log" 2>&1 || rc=$? ;;
        esac

        if [ "$rc" -ne 0 ]; then
            [ "$MODE" = "rebase" ] && git -C "$repo" rebase --abort >/dev/null 2>&1
            [ "$MODE" = "merge" ]  && git -C "$repo" merge --abort  >/dev/null 2>&1
            say "${C_RED}更新失败，已回到更新前状态${C_RST}"
            if [ "$stashed" -eq 1 ]; then
                git -C "$repo" stash pop >>"$log" 2>&1 && say "已恢复 stash"
            fi
            report FAIL "更新失败"; return
        fi

        if [ "$stashed" -eq 1 ]; then
            if git -C "$repo" stash pop >>"$log" 2>&1; then
                say "已恢复 stash"
            else
                # 保留冲突现场，stash 条目也仍在，由用户决定怎么合
                say "${C_YLW}stash pop 冲突：工作区有冲突标记，stash 条目未删除，请手动处理${C_RST}"
                report WARN "已更新但 stash 冲突待处理"
                git -C "$repo" log --oneline "$local_sha..HEAD" >>"$log" 2>&1
                return
            fi
        fi
    fi

    local new_sha shortstat pull_msg=""
    new_sha="$(git -C "$repo" rev-parse HEAD)"
    if [ "$need_pull" -eq 1 ]; then
        shortstat="$(git -C "$repo" diff --shortstat "$local_sha" "$new_sha" 2>/dev/null | sed 's/^ *//')"
        say "${C_GRN}拉取 $behind 个提交${C_RST} $(git -C "$repo" rev-parse --short "$local_sha")..$(git -C "$repo" rev-parse --short "$new_sha")${shortstat:+  ($shortstat)}"
        git -C "$repo" log --oneline --no-decorate "$local_sha..$new_sha" 2>/dev/null | head -10 | sed 's/^/    /' >&3
        pull_msg="拉取 ${behind} 个提交${shortstat:+, $shortstat}"
    fi

    # 双向更新：拉取完成后，如本地仍领先 upstream 则 push 上去
    if [ "$PUSH" -eq 1 ]; then
        local push_ahead
        push_ahead="$(git -C "$repo" rev-list --count "${upstream}..HEAD" 2>/dev/null || echo 0)"
        if [ "$push_ahead" -gt 0 ]; then
            if git -C "$repo" push >>"$log" 2>&1; then
                say "${C_GRN}已推送 $push_ahead 个提交到 $upstream${C_RST}"
                if [ -n "$pull_msg" ]; then
                    report SYNC "${pull_msg}；推送 ${push_ahead} 个提交"
                else
                    report PUSH "推送 ${push_ahead} 个提交"
                fi
                return
            else
                say "${C_RED}push 失败（可能远端有新提交，或无推送权限），本地改动仍保留${C_RST}"
                if [ -n "$pull_msg" ]; then
                    report WARN "${pull_msg}；push 失败"
                else
                    report FAIL "push 失败"
                fi
                return
            fi
        fi
    fi

    if [ -n "$pull_msg" ]; then
        report UPD "$pull_msg"
    elif [ "$ahead" -gt 0 ]; then
        say "${C_DIM}本地领先 $ahead 个提交，未推送（加 -u 可自动 push）${C_RST}"
        report SKIP "本地领先 $ahead，未推送"
    else
        say "${C_DIM}已是最新 ($branch)${C_RST}"
        report OK "已最新"
    fi
}

# ——— 构建：每个项目的方式都不一样，差异全部集中在下面几个函数里 ———

KNOWN_TARGETS="jsh anvil ember frost forge jwm"

# 这些项目由各自仓库的安装脚本负责“构建 + 安装”，这里不再重复拼 cargo 命令：
# 后端选择、--locked、feature、desktop 文件等细节都在那些脚本里，抄一份必然会漂。
INSTALL_TARGETS="anvil ember forge frost jwm"

is_known_target() {
    case " $KNOWN_TARGETS " in *" $1 "*) return 0 ;; *) return 1 ;; esac
}

is_install_target() {
    case " $INSTALL_TARGETS " in *" $1 "*) return 0 ;; *) return 1 ;; esac
}

# 四个终端共用同一套 install.sh 接口，额外参数按 <大写名字>_INSTALL_ARGS 取。
install_args_for() {
    local var
    var="$(printf '%s' "$1" | tr '[:lower:]' '[:upper:]')_INSTALL_ARGS"
    printf '%s' "${!var:-}"
}

# 复刻 anvil/ember/forge/frost install.sh 里 BIN_DIR 的解析：显式 --bin-dir 优先，
# 其次 --prefix/bin，都没给就是 ~/.cargo/bin；DESTDIR 作为打包用的暂存前缀。
install_bin_dir() {
    local prefix="" bindir=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --bin-dir)   bindir="${2:-}"; shift 2 || break ;;
            --bin-dir=*) bindir="${1#*=}"; shift ;;
            --prefix)    prefix="${2:-}"; shift 2 || break ;;
            --prefix=*)  prefix="${1#*=}"; shift ;;
            *)           shift ;;
        esac
    done
    if [ -n "$bindir" ]; then
        printf '%s%s\n' "${DESTDIR:-}" "$bindir"
    elif [ -n "$prefix" ]; then
        printf '%s%s/bin\n' "${DESTDIR:-}" "$prefix"
    else
        printf '%s%s/.cargo/bin\n' "${DESTDIR:-}" "$HOME"
    fi
}

# -T 白名单；未指定时全部已知项目都在范围内
in_scope() {
    local name="$1" item
    [ -z "$ONLY" ] && return 0
    IFS=',' read -r -a _only <<<"$ONLY"
    for item in "${_only[@]}"; do
        [ "${item// /}" = "$name" ] && return 0
    done
    return 1
}

# jsh 要编译成哪个 target。空 = 交给 cargo 的默认 target（非 Linux/未知架构）。
jsh_target() {
    local arch
    [ -n "${JSH_INSTALL_TARGET:-}" ] && { printf '%s\n' "$JSH_INSTALL_TARGET"; return; }
    [ "$(uname -s)" = "Linux" ] || return 0
    case "$(uname -m)" in
        x86_64|amd64)  arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) return 0 ;;
    esac
    printf '%s-unknown-linux-musl\n' "$arch"
}

# musl 需要两样东西：musl 版 std（rustup 能补）和一个 musl C 编译器（给
# TLS 依赖的 C 代码用）。缺了就直接失败，不退回宿主工具链——那样会装上一个
# 看着正常的动态二进制，等到它被推进容器里才暴露。设置 JSH_CC 供 build_cmd 用。
jsh_preflight() {
    local triple="$1" candidate
    JSH_CC=""
    case "$triple" in *-musl) ;; *) return 0 ;; esac

    for candidate in "${triple%%-*}-linux-musl-gcc" musl-gcc; do
        if command -v "$candidate" >/dev/null 2>&1; then JSH_CC="$candidate"; break; fi
    done
    if [ -z "$JSH_CC" ]; then
        printf '%s\n' "${C_RED}jsh 需要 musl C 编译器（Debian/Ubuntu: sudo apt install musl-tools）${C_RST}" >&2
        printf '%s\n' "${C_DIM}要按宿主 glibc 编译请显式指定 JSH_INSTALL_TARGET=${triple%%-*}-unknown-linux-gnu${C_RST}" >&2
        return 1
    fi
    if ! command -v rustup >/dev/null 2>&1; then
        printf '%s\n' "${C_RED}jsh 静态构建需要 rustup 来安装 $triple 的 std${C_RST}" >&2
        return 1
    fi
    if ! rustup target list --installed 2>/dev/null | grep -qx "$triple"; then
        printf '%s\n' "${C_DIM}rustup target add $triple${C_RST}"
        rustup target add "$triple" || {
            printf '%s\n' "${C_RED}无法安装 $triple 的 std${C_RST}" >&2
            return 1
        }
    fi
    # cc-rs 认的变量名，和 jsh 的 release workflow 用的是同一个
    export CC_x86_64_unknown_linux_musl="$JSH_CC"
    export CC_aarch64_unknown_linux_musl="$JSH_CC"
    return 0
}

# 产物路径，用来判断“没更新但还没构建过”的情况。走安装脚本的项目看安装位置：
# target/ 里躺着一个二进制不代表它已经装到 PATH 上，看装好的那份才有意义。
artifact_path() {
    local name="$1" repo="$2" target_dir
    if [ "$name" = "jwm" ]; then
        printf '/usr/local/bin/jwm\n'
        return
    fi
    if is_install_target "$name"; then
        # 分词是故意的：额外参数按空白拆成一个个 argv，和 JWM_INSTALL_ARGS 一致
        # shellcheck disable=SC2046
        printf '%s/%s\n' "$(install_bin_dir $(install_args_for "$name"))" "$name"
        return
    fi
    target_dir="${CARGO_TARGET_DIR:-$repo/target}"
    # 指定了 --target 的话，cargo 会多一层三元组目录
    if [ "$name" = "jsh" ] && [ -n "$JSH_TRIPLE" ]; then
        printf '%s/%s/release/%s\n' "$target_dir" "$JSH_TRIPLE" "$name"
        return
    fi
    printf '%s/release/%s\n' "$target_dir" "$name"
}

# 构建命令，一行一个参数（build_one 用 mapfile 读回数组）。
build_cmd() {
    local name="$1"
    case "$name" in
        # 四个终端各自带 scripts/install.sh，构建加安装一步到位：后端 (nix/cargo)
        # 由 --backend auto 自己挑，还会装 desktop/AppStream/图标并检查 PATH 遮挡。
        # 目标是 ~/.cargo/bin，全程在 $HOME 下，不需要 sudo。
        anvil|ember|forge|frost)
            # shellcheck disable=SC2046  # 额外参数按空白拆成一个个 argv，是故意的
            printf '%s\n' ./scripts/install.sh $(install_args_for "$name")
            ;;
        # jwm 只有“安装”这一种做法：根 workspace 的 cargo build 只产出
        # jwm/jwm-tool/jwm-support/jwm-remote，bar 和 jwm-bridge 都不在里面，产物也不会
        # 进 /usr/local/bin，编完了跑的还是旧的那份。所以直接跑安装脚本，
        # 它自带构建（装 bar、bridge、desktop 文件，要 sudo）。
        jwm)
            printf '%s\n' ./scripts/install_jwm_scripts.sh ${JWM_INSTALL_ARGS}
            ;;
        # jsh 只编不装：安装形态是静态 musl 二进制，装到哪由 install-jsh.sh 决定。
        jsh)
            if [ -n "$JSH_TRIPLE" ]; then
                printf '%s\n' cargo build --release --locked --target "$JSH_TRIPLE"
            else
                printf '%s\n' cargo build --release --locked
            fi
            ;;
    esac
}

# 单个项目的构建。结果写 $WORKDIR/b<idx>.status，日志写 $WORKDIR/b<idx>.log。
build_one() {
    local idx="$1" repo="$2" name="$3" reason="$4" live="$5"
    local log="$WORKDIR/b$idx.log" cmd=() rc=0 start elapsed verb="编译" done_verb="已编译"

    mapfile -t cmd < <(build_cmd "$name")
    # 跑的是安装脚本的项目，别在日志里谎称只是“编译”
    if is_install_target "$name"; then
        verb="安装"; done_verb="已安装"
    fi
    if [ "${#cmd[@]}" -eq 0 ]; then
        printf 'SKIP|%s|没有定义构建方式\n' "$name" >"$WORKDIR/b$idx.status"
        return
    fi

    {
        printf '%s\n' "${C_BLD}==> $verb $name${C_RST} ${C_DIM}($reason)${C_RST}"
        printf '  %s\n' "${C_DIM}\$ ${cmd[*]}${C_RST}"
    } >"$log"

    start=$SECONDS
    if [ "$live" -eq 1 ]; then
        cat "$log"
        ( cd "$repo" && "${cmd[@]}" ) 2>&1 | tee -a "$log" | sed 's/^/  /'
        rc=${PIPESTATUS[0]}
    else
        ( cd "$repo" && "${cmd[@]}" ) >>"$log" 2>&1
        rc=$?
    fi
    elapsed=$((SECONDS - start))

    if [ "$rc" -eq 0 ]; then
        printf 'OK|%s|%s，耗时 %ds\n' "$name" "$done_verb" "$elapsed" >"$WORKDIR/b$idx.status"
    else
        printf 'FAIL|%s|退出码 %d，耗时 %ds\n' "$name" "$rc" "$elapsed" >"$WORKDIR/b$idx.status"
    fi
}

printf '%s\n' "${C_BLD}扫描 $ROOT: ${#repos[@]} 个仓库，并发 $JOBS，模式 $MODE$([ $DRY_RUN -eq 1 ] && echo ' (dry-run)')${C_RST}"

running=0
for i in "${!repos[@]}"; do
    update_one "$i" "${repos[$i]}" &
    running=$((running + 1))
    if [ "$running" -ge "$JOBS" ]; then
        wait -n 2>/dev/null || wait
        running=$((running - 1))
    fi
done
wait

# 按仓库顺序回放日志与汇总
n_ok=0; n_upd=0; n_push=0; n_sync=0; n_skip=0; n_fail=0; n_warn=0; n_dry=0
summary=()
declare -a git_st=()
for i in "${!repos[@]}"; do
    [ "$QUIET" -eq 0 ] && [ -s "$WORKDIR/$i.log" ] && cat "$WORKDIR/$i.log"
    line="$(cat "$WORKDIR/$i.status" 2>/dev/null || printf 'FAIL|%s|无结果\n' "$(basename "${repos[$i]}")")"
    summary+=("$line")
    git_st[$i]="${line%%|*}"
    case "${line%%|*}" in
        OK)   n_ok=$((n_ok+1)) ;;
        UPD)  n_upd=$((n_upd+1)) ;;
        PUSH) n_push=$((n_push+1)) ;;
        SYNC) n_sync=$((n_sync+1)) ;;
        SKIP) n_skip=$((n_skip+1)) ;;
        WARN) n_warn=$((n_warn+1)) ;;
        DRY)  n_dry=$((n_dry+1)) ;;
        *)    n_fail=$((n_fail+1)) ;;
    esac
done

printf '\n%s\n' "${C_BLD}—— 汇总 ——${C_RST}"
for line in "${summary[@]}"; do
    st="${line%%|*}"; rest="${line#*|}"; nm="${rest%%|*}"; msg="${rest#*|}"
    case "$st" in
        OK)   printf '  %s%-8s%s %-16s %s\n' "$C_DIM" "最新"   "$C_RST" "$nm" "$msg" ;;
        UPD)  printf '  %s%-8s%s %-16s %s\n' "$C_GRN" "已拉取" "$C_RST" "$nm" "$msg" ;;
        PUSH) printf '  %s%-8s%s %-16s %s\n' "$C_GRN" "已推送" "$C_RST" "$nm" "$msg" ;;
        SYNC) printf '  %s%-8s%s %-16s %s\n' "$C_GRN" "已同步" "$C_RST" "$nm" "$msg" ;;
        SKIP) printf '  %s%-8s%s %-16s %s\n' "$C_YLW" "跳过"   "$C_RST" "$nm" "$msg" ;;
        WARN) printf '  %s%-8s%s %-16s %s\n' "$C_YLW" "注意"   "$C_RST" "$nm" "$msg" ;;
        DRY)  printf '  %s%-8s%s %-16s %s\n' "$C_DIM" "待更新" "$C_RST" "$nm" "$msg" ;;
        *)    printf '  %s%-8s%s %-16s %s\n' "$C_RED" "失败"   "$C_RST" "$nm" "$msg" ;;
    esac
done
printf '%s\n' "${C_DIM}共 ${#repos[@]} 个: 拉取 $n_upd / 推送 $n_push / 双向同步 $n_sync / 最新 $n_ok / 待更新 $n_dry / 跳过 $n_skip / 注意 $n_warn / 失败 $n_fail${C_RST}"

n_bok=0; n_bfail=0; n_bskip=0
if [ "$BUILD" -eq 1 ]; then
    JSH_TRIPLE="$(jsh_target)"
    # 挑出要处理的项目：默认只编译本次拉到新提交的，产物不存在的补一次，-B 全编
    declare -a run_idx=() build_reason=()
    for i in "${!repos[@]}"; do
        name="$(basename "${repos[$i]}")"
        is_known_target "$name" || continue
        in_scope "$name" || continue
        st="${git_st[$i]:-FAIL}"

        # 更新失败或 stash 冲突未处理时，源码不在一个干净的已知状态，不碰它
        if [ "$st" = "FAIL" ] || [ "$st" = "WARN" ]; then
            printf 'SKIP|%s|仓库更新未成功，未编译\n' "$name" >"$WORKDIR/b$i.status"
            continue
        fi

        reason=""
        case "$st" in
            UPD|SYNC) reason="有新提交" ;;
        esac
        if [ -z "$reason" ] && [ "$BUILD_ALL" -eq 1 ]; then
            reason="-B 强制"
        fi
        if [ -z "$reason" ] && [ ! -x "$(artifact_path "$name" "${repos[$i]}")" ]; then
            reason="产物缺失"
        fi
        [ -n "$reason" ] || continue

        build_reason[$i]="$reason"
        run_idx+=("$i")
    done

    # jsh 的 musl 工具链检查放在开跑之前：缺东西就只把 jsh 摘掉，别的照编。
    for i in "${run_idx[@]}"; do
        [ "$(basename "${repos[$i]}")" = "jsh" ] || continue
        if ! jsh_preflight "$JSH_TRIPLE"; then
            printf 'FAIL|jsh|%s 工具链不全，未编译\n' "$JSH_TRIPLE" >"$WORKDIR/b$i.status"
            keep=()
            for j in "${run_idx[@]}"; do [ "$j" = "$i" ] || keep+=("$j"); done
            run_idx=(${keep[@]+"${keep[@]}"})
        fi
        break
    done

    if ! command -v cargo >/dev/null 2>&1 && [ "${#run_idx[@]}" -gt 0 ]; then
        printf '\n%s\n' "${C_RED}找不到 cargo，跳过全部编译（先装 Rust 工具链，或用 -N 关掉编译）${C_RST}"
        for i in "${run_idx[@]}"; do
            printf 'FAIL|%s|没有 cargo\n' "$(basename "${repos[$i]}")" >"$WORKDIR/b$i.status"
        done
        run_idx=()
    fi

    # jwm 的安装脚本要 sudo 往 /usr/local/bin 和 xsessions 里装东西。并发或 -q
    # 模式下子进程的输出全进日志文件，密码提示看不见，表现就是整个脚本卡死；
    # 所以开跑前先把凭据取到手，非交互环境只提醒不阻塞。
    for i in "${run_idx[@]}"; do
        [ "$(basename "${repos[$i]}")" = "jwm" ] || continue
        if command -v sudo >/dev/null 2>&1 && ! sudo -n true 2>/dev/null; then
            if [ -t 0 ]; then
                printf '\n%s\n' "${C_DIM}jwm 安装需要 sudo，先验证一次密码${C_RST}"
                sudo -v || printf '%s\n' "${C_YLW}sudo 验证失败，jwm 安装大概率会失败${C_RST}" >&2
            else
                printf '\n%s\n' "${C_YLW}jwm 安装需要 sudo，但当前不是交互终端；请先 sudo -v，或用 -T 把 jwm 排除${C_RST}" >&2
            fi
        fi
        break
    done

    live=0
    [ "$BUILD_JOBS" -le 1 ] && [ "$QUIET" -eq 0 ] && live=1

    if [ "${#run_idx[@]}" -gt 0 ]; then
        printf '\n%s\n' "${C_BLD}—— 构建: ${#run_idx[@]} 个项目，并发 $BUILD_JOBS ——${C_RST}"
        brunning=0
        for i in "${run_idx[@]}"; do
            if [ "$live" -eq 1 ]; then
                build_one "$i" "${repos[$i]}" "$(basename "${repos[$i]}")" "${build_reason[$i]}" 1
            else
                build_one "$i" "${repos[$i]}" "$(basename "${repos[$i]}")" "${build_reason[$i]}" 0 &
                brunning=$((brunning + 1))
                if [ "$brunning" -ge "$BUILD_JOBS" ]; then
                    wait -n 2>/dev/null || wait
                    brunning=$((brunning - 1))
                fi
            fi
        done
        wait
    fi

    bsummary=()
    for i in "${!repos[@]}"; do
        [ -f "$WORKDIR/b$i.status" ] || continue
        [ "$live" -eq 0 ] && [ "$QUIET" -eq 0 ] && [ -s "$WORKDIR/b$i.log" ] && cat "$WORKDIR/b$i.log"
        line="$(cat "$WORKDIR/b$i.status")"
        bsummary+=("$line")
        case "${line%%|*}" in
            OK)   n_bok=$((n_bok+1)) ;;
            SKIP) n_bskip=$((n_bskip+1)) ;;
            *)    n_bfail=$((n_bfail+1)) ;;
        esac
    done

    if [ "${#bsummary[@]}" -gt 0 ]; then
        printf '\n%s\n' "${C_BLD}—— 构建汇总 ——${C_RST}"
        for line in "${bsummary[@]}"; do
            st="${line%%|*}"; rest="${line#*|}"; nm="${rest%%|*}"; msg="${rest#*|}"
            case "$st" in
                OK)   printf '  %s%-8s%s %-16s %s\n' "$C_GRN" "成功" "$C_RST" "$nm" "$msg" ;;
                SKIP) printf '  %s%-8s%s %-16s %s\n' "$C_YLW" "跳过" "$C_RST" "$nm" "$msg" ;;
                *)    printf '  %s%-8s%s %-16s %s\n' "$C_RED" "失败" "$C_RST" "$nm" "$msg" ;;
            esac
        done
        printf '%s\n' "${C_DIM}成功 $n_bok / 跳过 $n_bskip / 失败 $n_bfail${C_RST}"
    elif [ "$QUIET" -eq 0 ]; then
        printf '%s\n' "${C_DIM}没有项目需要构建（本次没有新提交；加 -B 可强制全部处理）${C_RST}"
    fi
fi

if [ "$n_fail" -gt 0 ] || [ "$n_bfail" -gt 0 ]; then
    exit 1
fi
exit 0
