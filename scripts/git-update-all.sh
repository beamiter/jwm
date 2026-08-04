#!/usr/bin/env bash
# 一键更新目录下所有 git 仓库。
# 默认行为：fetch --prune 后对当前分支做 fast-forward，只要有任何风险就跳过而不是硬来。
set -uo pipefail

ROOT="."
JOBS=4
MODE="ff"        # ff | rebase | merge
PRUNE=1
STASH=0
DRY_RUN=0
QUIET=0
PUSH=0

usage() {
    cat <<'EOF'
用法: git-update-all.sh [选项] [目录]

选项:
  -j N          并发数 (默认 4)
  -r            用 git pull --rebase 代替 fast-forward
  -m            用 git pull --no-rebase 代替 fast-forward (允许产生 merge commit)
  -s            工作区有改动时自动 stash，更新后再 stash pop
  -n            dry-run: 只 fetch 和汇报，不改动工作区
  -P            不加 --prune
  -u            双向更新：拉取之后，如本地领先则自动 push 到 upstream
  -q            只输出汇总表
  -h            显示本帮助

默认目录为当前目录，扫描其下一层的子目录。
退出码: 0 全部成功；1 有仓库更新失败。
EOF
}

while getopts ":j:rmsnPuqh" opt; do
    case "$opt" in
        j) JOBS="$OPTARG" ;;
        r) MODE="rebase" ;;
        m) MODE="merge" ;;
        s) STASH=1 ;;
        n) DRY_RUN=1 ;;
        P) PRUNE=0 ;;
        u) PUSH=1 ;;
        q) QUIET=1 ;;
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

if [ -t 1 ]; then
    C_RST=$'\033[0m'; C_GRN=$'\033[32m'; C_YLW=$'\033[33m'
    C_RED=$'\033[31m'; C_DIM=$'\033[2m'; C_BLD=$'\033[1m'
else
    C_RST=''; C_GRN=''; C_YLW=''; C_RED=''; C_DIM=''; C_BLD=''
fi

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/git-update-all.XXXXXX")"
trap 'rm -rf "$WORKDIR"' EXIT

# 收集仓库：ROOT 本身若是仓库也算上，外加一层子目录
repos=()
[ -e "$ROOT/.git" ] && repos+=("$ROOT")
for d in "$ROOT"/*/; do
    [ -e "${d%/}/.git" ] && repos+=("${d%/}")
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
for i in "${!repos[@]}"; do
    [ "$QUIET" -eq 0 ] && [ -s "$WORKDIR/$i.log" ] && cat "$WORKDIR/$i.log"
    line="$(cat "$WORKDIR/$i.status" 2>/dev/null || printf 'FAIL|%s|无结果\n' "$(basename "${repos[$i]}")")"
    summary+=("$line")
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

[ "$n_fail" -gt 0 ] && exit 1
exit 0
