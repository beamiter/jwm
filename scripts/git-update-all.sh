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
MAX_PARALLEL_JOBS=64

# 名字必须与扫描到的仓库 basename 一致。LifeAI 是给命令行使用的便捷别名，
# 规范名仍是实际目录 LifeAI.jl。
KNOWN_TARGETS="jagent jsh jterm_core anvil ember frost forge jwm cplus LifeAI.jl"
INSTALL_TARGETS="anvil ember forge frost jwm"

is_known_target() {
    case " $KNOWN_TARGETS " in *" $1 "*) return 0 ;; *) return 1 ;; esac
}

is_install_target() {
    case " $INSTALL_TARGETS " in *" $1 "*) return 0 ;; *) return 1 ;; esac
}

canonical_target_name() {
    case "$1" in
        LifeAI) printf '%s\n' 'LifeAI.jl' ;;
        *)      printf '%s\n' "$1" ;;
    esac
}

# -T 白名单；未指定时全部仓库都在范围内。ONLY 在参数预检时已规范化。
in_scope() {
    local name="$1" item
    local -a _only=()
    [ -z "$ONLY" ] && return 0
    IFS=',' read -r -a _only <<<"$ONLY"
    for item in "${_only[@]}"; do
        [ "$item" = "$name" ] && return 0
    done
    return 1
}

normalize_only() {
    local raw item normalized="" seen=" "
    local -a requested=()
    [ -z "$ONLY" ] && return 0
    IFS=',' read -r -a requested <<<"$ONLY"
    for raw in "${requested[@]}"; do
        # Trim only at the edges; embedded whitespace remains invalid instead
        # of silently changing a mistyped project name.
        item="${raw#"${raw%%[![:space:]]*}"}"
        item="${item%"${item##*[![:space:]]}"}"
        [ -n "$item" ] || { echo "-T 包含空项目名" >&2; return 1; }
        item="$(canonical_target_name "$item")"
        is_known_target "$item" || {
            echo "-T 包含未知项目: $(sanitize_text "$item")" >&2
            return 1
        }
        case "$seen" in
            *" $item "*) continue ;;
        esac
        seen+="$item "
        normalized+="${normalized:+,}$item"
    done
    ONLY="$normalized"
}

# Remote commit subjects and filesystem names are not trusted terminal text.
# Keep ordinary Unicode readable, but make delimiters, backslashes, C0/C1,
# bidi overrides/isolates and common zero-width/default-ignorable display
# controls explicit before they enter logs or the pipe-delimited status files.
# The latter also prevents a repository named `a|FAIL|b` from forging summary
# columns or visually reordering adjacent trusted text.
sanitize_text() {
    local input="$1" output="" character escaped
    local code index
    for ((index = 0; index < ${#input}; index++)); do
        character="${input:index:1}"
        case "$character" in
            \\)   output="${output}\\\\"; continue ;;
            '|')  output+='\x7C'; continue ;;
        esac
        printf -v code '%d' "'$character"
        if ((code < 32 || (code >= 127 && code <= 159) \
            || code == 0x00AD || code == 0x034F || code == 0x061C \
            || (code >= 0x115F && code <= 0x1160) \
            || (code >= 0x17B4 && code <= 0x17B5) \
            || (code >= 0x180B && code <= 0x180F) \
            || (code >= 0x200B && code <= 0x200F) \
            || (code >= 0x2028 && code <= 0x202E) \
            || (code >= 0x2060 && code <= 0x206F) \
            || code == 0x3164 || (code >= 0xFE00 && code <= 0xFE0F) \
            || code == 0xFEFF || code == 0xFFA0 \
            || (code >= 0x1BCA0 && code <= 0x1BCA3) \
            || (code >= 0x1D173 && code <= 0x1D17A) \
            || (code >= 0xE0000 && code <= 0xE0FFF))); then
            printf -v escaped '\\u{%04X}' "$code"
            output+="$escaped"
        else
            output+="$character"
        fi
    done
    printf '%s' "$output"
}

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

构建选项 (只作用于已知项目: jagent jsh jterm_core anvil ember frost forge jwm cplus LifeAI.jl):
  -B            全部重新构建，不管本次有没有拉到新提交
  -N            不构建，只更新仓库
  -T 列表       只更新和构建这些项目，逗号分隔，如 -T jagent,jwm,LifeAI
  -J N          同时构建几个项目 (默认 1；cargo 内部已经并行)
  -h            显示本帮助

默认目录为当前目录，扫描其下一层的子目录。
不在已知项目表里的仓库只更新、不构建。默认只处理本次真的有新提交、
或者产物不存在的项目。

各项目的构建方式并不相同，脚本里按名字分派:
  anvil,forge       scripts/install.sh —— 后端 (nix / cargo) 由 --backend auto 挑；
                    anvil 默认装到 ~/.local/bin，forge 默认装到 ~/.cargo/bin。
  ember,frost       scripts/install.sh —— 默认用 cargo 构建，也可用 --binary 安装
                    预构建产物；默认装到 ~/.local/bin。这四个目标都不需要 sudo。
  jwm              scripts/install_jwm_scripts.sh —— jwm 没有只编不装的中间态：
                   裸 cargo build 只出根 package 的四个二进制，不编 bar 和
                   jwm-bridge，也不同步到 /usr/local/bin，更新完跑的还是旧的
                   那份。所以这里直接跑安装脚本，它自带构建。需要 sudo。
  jsh              cargo build --target <arch>-unknown-linux-musl (静态 musl)
  jagent,jterm_core cargo build --release --locked（库 crate）
  cplus            bash build.sh（仓库自己的 CMake 构建入口）
  LifeAI.jl        锁定项目环境下 instantiate/precompile 并加载 LifeAI

上述 jsh/jagent/jterm_core/cplus/LifeAI.jl 路径只构建或预编译、不安装；
要装 jsh 请运行其仓库的 scripts/install-jsh.sh。
产物检测看的是各项目真正落地的位置: jwm 看 /usr/local/bin/jwm，四个终端看
安装脚本的目标目录（anvil/ember/frost 默认 ~/.local/bin，forge 默认
~/.cargo/bin；都跟随 --prefix/--bin-dir/DESTDIR），
jsh/jagent/jterm_core 看 Cargo target 下的 release 产物，cplus 看 build/m；
LifeAI.jl 的 Julia 预编译缓存没有稳定的仓库内路径，因此只在更新或 -B 时执行。

环境变量:
  ANVIL_INSTALL_ARGS 追加给 anvil/scripts/install.sh 的参数，如 '--backend cargo'
  EMBER_INSTALL_ARGS 追加给 ember/scripts/install.sh，如 '--no-desktop'
  FORGE_INSTALL_ARGS 追加给 forge/scripts/install.sh，如 '--backend nix'
  FROST_INSTALL_ARGS 追加给 frost/scripts/install.sh，如 '--binary /path/to/frost'
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
if [ $# -gt 1 ]; then
    echo "只能指定一个扫描目录" >&2
    exit 2
fi
[ $# -eq 1 ] && ROOT="$1"

if [ ! -d "$ROOT" ]; then
    echo "目录不存在: $(sanitize_text "$ROOT")" >&2
    exit 2
fi

case "$JOBS" in ''|*[!0-9]*) echo "-j 需要正整数" >&2; exit 2 ;; esac
case "$BUILD_JOBS" in ''|*[!0-9]*) echo "-J 需要正整数" >&2; exit 2 ;; esac
if [ "${#JOBS}" -gt 2 ]; then
    echo "-j 必须在 1..$MAX_PARALLEL_JOBS 之间" >&2
    exit 2
fi
if [ "${#BUILD_JOBS}" -gt 2 ]; then
    echo "-J 必须在 1..$MAX_PARALLEL_JOBS 之间" >&2
    exit 2
fi
JOBS=$((10#$JOBS))
BUILD_JOBS=$((10#$BUILD_JOBS))
if [ "$JOBS" -lt 1 ] || [ "$JOBS" -gt "$MAX_PARALLEL_JOBS" ]; then
    echo "-j 必须在 1..$MAX_PARALLEL_JOBS 之间" >&2
    exit 2
fi
if [ "$BUILD_JOBS" -lt 1 ] || [ "$BUILD_JOBS" -gt "$MAX_PARALLEL_JOBS" ]; then
    echo "-J 必须在 1..$MAX_PARALLEL_JOBS 之间" >&2
    exit 2
fi
normalize_only || exit 2

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
    is_git_repo "${d%/}" && repos+=("$(cd "${d%/}" && pwd)")
done

# `-T` is an update scope, not merely a build filter.  Filtering before any
# worker starts guarantees an omitted repository is not even fetched.
if [ -n "$ONLY" ]; then
    declare -a selected_repos=()
    for repo in "${repos[@]}"; do
        in_scope "$(basename "$repo")" && selected_repos+=("$repo")
    done
    repos=("${selected_repos[@]}")
fi

if [ ${#repos[@]} -eq 0 ]; then
    echo "在 $(sanitize_text "$ROOT") 下没有找到 git 仓库" >&2
    exit 0
fi

# 单个仓库的更新逻辑。结果写入 $WORKDIR/<idx>.status ("状态|仓库名|说明")，
# 详细日志写入 $WORKDIR/<idx>.log，主进程按顺序回放，避免并发输出交错。
update_one() {
    local idx="$1" repo="$2"
    local name log status detail branch_display upstream_display
    name="$(sanitize_text "$(basename "$repo")")"
    log="$WORKDIR/$idx.log"
    exec 3>"$log"

    report() {
        status="$1"; detail="$(sanitize_text "$2")"
        printf '%s|%s|%s\n' "$status" "$name" "$detail" >"$WORKDIR/$idx.status"
        exec 3>&-
        return 0
    }
    say() { printf '  %s\n' "$*" >&3; }
    # Fetch/pull/push sideband is controlled by the remote. Replay it as
    # ordinary text rather than letting a hostile server inject terminal
    # escapes or forge the updater's pipe-delimited diagnostics. Our own color
    # sequences never pass through this boundary and therefore remain intact.
    run_git_logged() {
        local raw="$WORKDIR/$idx.git-output" rc line
        : >"$raw"
        git -c color.ui=false -C "$repo" "$@" >"$raw" 2>&1
        rc=$?
        while IFS= read -r line || [ -n "$line" ]; do
            printf '%s\n' "$(sanitize_text "$line")" >&3
        done <"$raw"
        return "$rc"
    }
    append_commit_log() {
        local range="$1" commit_line
        while IFS= read -r commit_line; do
            printf '    %s\n' "$(sanitize_text "$commit_line")" >&3
        done < <(git -C "$repo" log --oneline --no-decorate --max-count=10 \
            "$range" 2>/dev/null)
    }

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
    branch_display="$(sanitize_text "$branch")"

    if [ -z "$(git -C "$repo" remote)" ]; then
        say "${C_DIM}没有配置 remote，跳过${C_RST}"
        report SKIP "无 remote"; return
    fi

    # fetch
    local fetch_args=(--tags --quiet)
    [ "$PRUNE" -eq 1 ] && fetch_args+=(--prune)
    if ! run_git_logged fetch "${fetch_args[@]}" --all; then
        say "${C_RED}fetch 失败${C_RST}"
        report FAIL "fetch 失败"; return
    fi

    local upstream
    upstream="$(git -C "$repo" rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null || true)"
    if [ -z "$upstream" ]; then
        say "${C_YLW}分支 $branch_display 没有 upstream，跳过${C_RST}"
        report SKIP "$branch 无 upstream"; return
    fi
    upstream_display="$(sanitize_text "$upstream")"

    local local_sha remote_sha
    local_sha="$(git -C "$repo" rev-parse HEAD)"
    remote_sha="$(git -C "$repo" rev-parse "$upstream")"

    if [ "$local_sha" = "$remote_sha" ]; then
        say "${C_DIM}已是最新 ($branch_display)${C_RST}"
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
            if run_git_logged stash push --include-untracked \
                -m "git-update-all $(date +%F_%T)"; then
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
            ff)     run_git_logged merge --ff-only "$upstream" || rc=$? ;;
            rebase) run_git_logged pull --rebase --quiet || rc=$? ;;
            merge)  run_git_logged pull --no-rebase --quiet || rc=$? ;;
        esac

        if [ "$rc" -ne 0 ]; then
            [ "$MODE" = "rebase" ] && git -C "$repo" rebase --abort >/dev/null 2>&1
            [ "$MODE" = "merge" ]  && git -C "$repo" merge --abort  >/dev/null 2>&1
            say "${C_RED}更新失败，已回到更新前状态${C_RST}"
            if [ "$stashed" -eq 1 ]; then
                run_git_logged stash pop && say "已恢复 stash"
            fi
            report FAIL "更新失败"; return
        fi

        if [ "$stashed" -eq 1 ]; then
            if run_git_logged stash pop; then
                say "已恢复 stash"
            else
                # 保留冲突现场，stash 条目也仍在，由用户决定怎么合
                say "${C_YLW}stash pop 冲突：工作区有冲突标记，stash 条目未删除，请手动处理${C_RST}"
                report WARN "已更新但 stash 冲突待处理"
                append_commit_log "$local_sha..HEAD"
                return
            fi
        fi
    fi

    local new_sha shortstat pull_msg=""
    new_sha="$(git -C "$repo" rev-parse HEAD)"
    if [ "$need_pull" -eq 1 ]; then
        shortstat="$(git -C "$repo" diff --shortstat "$local_sha" "$new_sha" 2>/dev/null | sed 's/^ *//')"
        say "${C_GRN}拉取 $behind 个提交${C_RST} $(git -C "$repo" rev-parse --short "$local_sha")..$(git -C "$repo" rev-parse --short "$new_sha")${shortstat:+  ($shortstat)}"
        append_commit_log "$local_sha..$new_sha"
        pull_msg="拉取 ${behind} 个提交${shortstat:+, $shortstat}"
    fi

    # 双向更新：拉取完成后，如本地仍领先 upstream 则 push 上去
    if [ "$PUSH" -eq 1 ]; then
        local push_ahead
        push_ahead="$(git -C "$repo" rev-list --count "${upstream}..HEAD" 2>/dev/null || echo 0)"
        if [ "$push_ahead" -gt 0 ]; then
            if run_git_logged push; then
                say "${C_GRN}已推送 $push_ahead 个提交到 $upstream_display${C_RST}"
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
        say "${C_DIM}已是最新 ($branch_display)${C_RST}"
        report OK "已最新"
    fi
}

# ——— 构建：每个项目的方式都不一样，差异全部集中在下面几个函数里 ———

# 四个终端共用同一套 install.sh 接口，额外参数按 <大写名字>_INSTALL_ARGS 取。
install_args_for() {
    local var
    var="$(printf '%s' "$1" | tr '[:lower:]' '[:upper:]')_INSTALL_ARGS"
    printf '%s' "${!var:-}"
}

# 复刻 anvil/ember/forge/frost install.sh 里 BIN_DIR 的解析：显式 --bin-dir 优先，
# 其次 --prefix/bin；anvil/ember/frost 默认 ~/.local/bin，forge 默认
# ~/.cargo/bin。DESTDIR 作为打包用的暂存前缀。
install_bin_dir() {
    local name="$1" prefix="" bindir=""
    shift
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
    elif [ "$name" = "forge" ]; then
        printf '%s%s/.cargo/bin\n' "${DESTDIR:-}" "$HOME"
    else
        printf '%s%s/.local/bin\n' "${DESTDIR:-}" "$HOME"
    fi
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
    local name="$1" repo="$2" target_dir raw_args
    local -a install_args=()
    if [ "$name" = "jwm" ]; then
        printf '/usr/local/bin/jwm\n'
        return
    fi
    if is_install_target "$name"; then
        # 环境变量的接口是按空白分 argv；read -a 保留这个契约，但不会像
        # 未加引号的展开那样把 `*` 再解释成当前目录的文件名。
        raw_args="$(install_args_for "$name")"
        [ -z "$raw_args" ] || read -r -a install_args <<<"$raw_args"
        printf '%s/%s\n' "$(install_bin_dir "$name" "${install_args[@]}")" "$name"
        return
    fi
    if [ "$name" = "cplus" ]; then
        printf '%s/build/m\n' "$repo"
        return
    fi
    if [ -n "${CARGO_TARGET_DIR:-}" ]; then
        target_dir="$CARGO_TARGET_DIR"
        case "$target_dir" in
            /*) ;;
            *) target_dir="$repo/$target_dir" ;;
        esac
    else
        target_dir="$repo/target"
    fi
    # 指定了 --target 的话，cargo 会多一层三元组目录
    if [ "$name" = "jsh" ] && [ -n "$JSH_TRIPLE" ]; then
        printf '%s/%s/release/%s\n' "$target_dir" "$JSH_TRIPLE" "$name"
        return
    fi
    if [ "$name" = "jagent" ] || [ "$name" = "jterm_core" ]; then
        printf '%s/release/lib%s.rlib\n' "$target_dir" "$name"
        return
    fi
    printf '%s/release/%s\n' "$target_dir" "$name"
}

artifact_is_ready() {
    local name="$1" repo="$2" artifact
    # Julia's precompile cache belongs to DEPOT_PATH, not the repository.  A
    # stable project-local path would be a fake signal, so LifeAI builds after
    # an update or explicit -B and does not invent an "artifact missing" run.
    [ "$name" = "LifeAI.jl" ] && return 0
    artifact="$(artifact_path "$name" "$repo")"
    case "$name" in
        jagent|jterm_core) [ -f "$artifact" ] ;;
        *)                 [ -x "$artifact" ] ;;
    esac
}

# 构建命令，一行一个参数（build_one 用 mapfile 读回数组）。
build_cmd() {
    local name="$1" raw_args
    local -a extra_args=()
    case "$name" in
        # 四个终端各自带 scripts/install.sh，构建加安装一步到位。anvil/forge
        # 自己选择 nix/cargo 后端，ember/frost 选择 cargo 或显式 --binary；脚本
        # 还负责 desktop/AppStream/图标和 PATH 遮挡检查。目标在用户 HOME 下
        #（forge 默认 ~/.cargo/bin，其余默认 ~/.local/bin），不需要 sudo。
        anvil|ember|forge|frost)
            raw_args="$(install_args_for "$name")"
            [ -z "$raw_args" ] || read -r -a extra_args <<<"$raw_args"
            printf '%s\n' ./scripts/install.sh
            [ "${#extra_args[@]}" -eq 0 ] || printf '%s\n' "${extra_args[@]}"
            ;;
        # jwm 只有“安装”这一种做法：根 workspace 的 cargo build 只产出
        # jwm/jwm-tool/jwm-support/jwm-remote，bar 和 jwm-bridge 都不在里面，产物也不会
        # 进 /usr/local/bin，编完了跑的还是旧的那份。所以直接跑安装脚本，
        # 它自带构建（装 bar、bridge、desktop 文件，要 sudo）。
        jwm)
            [ -z "$JWM_INSTALL_ARGS" ] || read -r -a extra_args <<<"$JWM_INSTALL_ARGS"
            printf '%s\n' ./scripts/install_jwm_scripts.sh
            [ "${#extra_args[@]}" -eq 0 ] || printf '%s\n' "${extra_args[@]}"
            ;;
        # jsh 只编不装：安装形态是静态 musl 二进制，装到哪由 install-jsh.sh 决定。
        jsh)
            if [ -n "$JSH_TRIPLE" ]; then
                printf '%s\n' cargo build --release --locked --target "$JSH_TRIPLE"
            else
                printf '%s\n' cargo build --release --locked
            fi
            ;;
        jagent|jterm_core)
            printf '%s\n' cargo build --release --locked
            ;;
        cplus)
            printf '%s\n' bash ./build.sh
            ;;
        LifeAI.jl)
            printf '%s\n' julia --project=. --startup-file=no --history-file=no \
                -e 'using Pkg; Pkg.instantiate(); Pkg.precompile(); using LifeAI'
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

printf '%s\n' "${C_BLD}扫描 $(sanitize_text "$ROOT"): ${#repos[@]} 个仓库，并发 $JOBS，模式 $MODE$([ $DRY_RUN -eq 1 ] && echo ' (dry-run)')${C_RST}"

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
    line="$(cat "$WORKDIR/$i.status" 2>/dev/null || printf 'FAIL|%s|无结果\n' "$(sanitize_text "$(basename "${repos[$i]}")")")"
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

        # Updating and building are separate trust boundaries.  A repository
        # can be SKIP/OK while already dirty (for example it has no upstream,
        # is only ahead, or -s restored the user's stash after a pull).  Never
        # turn that mixed source tree into an installed release implicitly.
        if ! worktree_state="$(git -C "${repos[$i]}" status --porcelain 2>/dev/null)"; then
            printf 'SKIP|%s|无法确认工作区状态，未编译\n' "$name" >"$WORKDIR/b$i.status"
            continue
        fi
        if [ -n "$worktree_state" ]; then
            printf 'SKIP|%s|工作区脏，未编译\n' "$name" >"$WORKDIR/b$i.status"
            continue
        fi

        reason=""
        case "$st" in
            UPD|SYNC) reason="有新提交" ;;
        esac
        if [ -z "$reason" ] && [ "$BUILD_ALL" -eq 1 ]; then
            reason="-B 强制"
        fi
        if [ -z "$reason" ] && ! artifact_is_ready "$name" "${repos[$i]}"; then
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

    # 逐项目只检查真正的入口工具。四个终端必须交给 install.sh 自己解析
    # --backend/--binary：anvil/forge 的 auto 会优先选 nix，而 ember/frost
    # 也可能安装预构建二进制；在这里一律要求 cargo 会提前否决合法调用。
    available_idx=()
    for i in "${run_idx[@]}"; do
        name="$(basename "${repos[$i]}")"
        case "$name" in
            anvil|ember|forge|frost) required_tool="bash" ;;
            cplus)      required_tool="bash" ;;
            LifeAI.jl)  required_tool="julia" ;;
            *)          required_tool="cargo" ;;
        esac
        if command -v "$required_tool" >/dev/null 2>&1; then
            available_idx+=("$i")
        else
            printf 'FAIL|%s|没有 %s\n' "$name" "$required_tool" >"$WORKDIR/b$i.status"
        fi
    done
    run_idx=("${available_idx[@]}")

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
