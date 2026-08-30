#!/usr/bin/env bash
# Offline contract tests for git-update-all.sh.  Fixtures have no network
# remotes; the one fetch exercised below targets a local bare repository.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
UPDATER="${SCRIPT_DIR}/git-update-all.sh"
TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/test-git-update-all.XXXXXX")"
trap 'rm -rf -- "$TEST_ROOT"' EXIT

fail() {
    printf 'test-git-update-all: %s\n' "$*" >&2
    exit 1
}

assert_contains() {
    local file="$1" expected="$2"
    grep -Fq -- "$expected" "$file" || fail "missing output: $expected"
}

assert_log_empty() {
    local log="$1"
    [ ! -s "$log" ] || fail "a build command ran unexpectedly: $(tr '\n' ' ' <"$log")"
}

init_repo() {
    local path="$1"
    mkdir -p -- "$path"
    git init -q "$path"
    git -C "$path" config user.name updater-test
    git -C "$path" config user.email updater-test@example.invalid
    printf 'fixture\n' >"$path/tracked.txt"
    git -C "$path" add tracked.txt
    git -C "$path" commit -q -m initial
}

make_tool_stubs() {
    local bin="$1" tool
    mkdir -p -- "$bin"
    for tool in cargo bash julia; do
        # The generated stub expands these variables when it runs.
        # shellcheck disable=SC2016
        printf '%s\n' \
            '#!/bin/sh' \
            'printf '\''%s:%s\n'\'' "${0##*/}" "$*" >>"${GIT_UPDATE_TEST_LOG:?}"' \
            >"$bin/$tool"
        chmod +x "$bin/$tool"
    done
}

run_with_stubs() {
    local log="$1"
    shift
    GIT_UPDATE_TEST_LOG="$log" PATH="$STUB_BIN:$PATH" /bin/bash "$UPDATER" "$@"
}

add_install_stub() {
    local repo="$1" name="$2"
    mkdir -p -- "$repo/scripts"
    # The generated fixture expands these variables when the updater runs it.
    # shellcheck disable=SC2016
    printf '%s\n' \
        '#!/usr/bin/env bash' \
        'set -euo pipefail' \
        'command -v nix >/dev/null' \
        '! command -v cargo >/dev/null' \
        'printf '\''%s:nix-only\n'\'' '"$name"' >>"${GIT_UPDATE_TEST_LOG:?}"' \
        >"$repo/scripts/install.sh"
    chmod +x "$repo/scripts/install.sh"
    git -C "$repo" add scripts/install.sh
    git -C "$repo" commit -q -m 'add installer fixture'
}

# Argument errors must fail before repository discovery or any fetch.
mkdir -p "$TEST_ROOT/empty"
if /bin/bash "$UPDATER" -T unknown "$TEST_ROOT/empty" >"$TEST_ROOT/invalid.out" 2>&1; then
    fail "unknown -T target was accepted"
fi
assert_contains "$TEST_ROOT/invalid.out" "-T 包含未知项目"
if /bin/bash "$UPDATER" -j 65 "$TEST_ROOT/empty" >"$TEST_ROOT/jobs.out" 2>&1; then
    fail "excessive -j was accepted"
fi
assert_contains "$TEST_ROOT/jobs.out" "1..64"
if /bin/bash "$UPDATER" "$TEST_ROOT/empty" "$TEST_ROOT/empty" >"$TEST_ROOT/roots.out" 2>&1; then
    fail "multiple roots were accepted"
fi
assert_contains "$TEST_ROOT/roots.out" "只能指定一个扫描目录"

# -T scopes updates as well as builds and accepts the LifeAI convenience alias.
SCOPE_ROOT="$TEST_ROOT/scope"
init_repo "$SCOPE_ROOT/jagent"
init_repo "$SCOPE_ROOT/jwm"
init_repo "$SCOPE_ROOT/LifeAI.jl"
/bin/bash "$UPDATER" -N -T ' jagent, LifeAI ' "$SCOPE_ROOT" >"$TEST_ROOT/scope.out"
assert_contains "$TEST_ROOT/scope.out" "共 2 个"
assert_contains "$TEST_ROOT/scope.out" "jagent"
assert_contains "$TEST_ROOT/scope.out" "LifeAI.jl"
if grep -Eq '^.*jwm.*无 remote' "$TEST_ROOT/scope.out"; then
    fail "-T still processed an omitted repository"
fi

# Heterogeneous known targets dispatch to their own build entry points without
# running a real compiler, installer, network request, or sudo command.
STUB_BIN="$TEST_ROOT/stub-bin"
make_tool_stubs "$STUB_BIN"
DISPATCH_ROOT="$TEST_ROOT/dispatch"
for name in jagent jterm_core cplus LifeAI.jl; do
    init_repo "$DISPATCH_ROOT/$name"
done
: >"$TEST_ROOT/dispatch.log"
run_with_stubs "$TEST_ROOT/dispatch.log" -B -T jagent,jterm_core,cplus,LifeAI "$DISPATCH_ROOT" \
    >"$TEST_ROOT/dispatch.out"
[ "$(grep -Fc 'cargo:build --release --locked' "$TEST_ROOT/dispatch.log")" -eq 2 ] \
    || fail "Rust library targets did not use the locked release build"
assert_contains "$TEST_ROOT/dispatch.log" "bash:./build.sh"
assert_contains "$TEST_ROOT/dispatch.log" "julia:--project=. --startup-file=no --history-file=no -e"

# Install targets own their backend selection. In particular anvil/forge's
# default `--backend auto` must be allowed to choose Nix when Cargo is absent.
NIX_ROOT="$TEST_ROOT/nix-only"
NIX_BIN="$TEST_ROOT/nix-bin"
mkdir -p "$NIX_BIN" "$TEST_ROOT/nix-home"
printf '#!/bin/sh\nexit 0\n' >"$NIX_BIN/nix"
chmod +x "$NIX_BIN/nix"
for name in anvil forge; do
    init_repo "$NIX_ROOT/$name"
    add_install_stub "$NIX_ROOT/$name" "$name"
done
: >"$TEST_ROOT/nix-only.log"
HOME="$TEST_ROOT/nix-home" GIT_UPDATE_TEST_LOG="$TEST_ROOT/nix-only.log" \
    PATH="$NIX_BIN:/usr/bin:/bin" /bin/bash "$UPDATER" -B -T anvil,forge "$NIX_ROOT" \
    >"$TEST_ROOT/nix-only.out"
assert_contains "$TEST_ROOT/nix-only.log" "anvil:nix-only"
assert_contains "$TEST_ROOT/nix-only.log" "forge:nix-only"

# A dirty tree is never built or installed, even under explicit -B.
DIRTY_ROOT="$TEST_ROOT/dirty"
init_repo "$DIRTY_ROOT/jagent"
printf 'local edit\n' >>"$DIRTY_ROOT/jagent/tracked.txt"
: >"$TEST_ROOT/dirty.log"
run_with_stubs "$TEST_ROOT/dirty.log" -B -T jagent "$DIRTY_ROOT" >"$TEST_ROOT/dirty.out"
assert_log_empty "$TEST_ROOT/dirty.log"
assert_contains "$TEST_ROOT/dirty.out" "工作区脏，未编译"

# Artifact probes mirror each installer's real defaults and Cargo interprets a
# relative CARGO_TARGET_DIR from inside the repository.
ARTIFACT_ROOT="$TEST_ROOT/artifacts"
FAKE_HOME="$TEST_ROOT/home"
mkdir -p "$FAKE_HOME/.local/bin" "$FAKE_HOME/.cargo/bin"
for name in anvil ember frost forge; do
    init_repo "$ARTIFACT_ROOT/$name"
done
for name in anvil ember frost; do
    printf '#!/bin/sh\n' >"$FAKE_HOME/.local/bin/$name"
    chmod +x "$FAKE_HOME/.local/bin/$name"
done
printf '#!/bin/sh\n' >"$FAKE_HOME/.cargo/bin/forge"
chmod +x "$FAKE_HOME/.cargo/bin/forge"
: >"$TEST_ROOT/artifacts.log"
HOME="$FAKE_HOME" run_with_stubs "$TEST_ROOT/artifacts.log" \
    -T anvil,ember,frost,forge "$ARTIFACT_ROOT" >"$TEST_ROOT/artifacts.out"
assert_log_empty "$TEST_ROOT/artifacts.log"

init_repo "$ARTIFACT_ROOT/jagent"
mkdir -p "$ARTIFACT_ROOT/jagent/target/release"
printf 'rlib\n' >"$ARTIFACT_ROOT/jagent/target/release/libjagent.rlib"
: >"$TEST_ROOT/default-target.log"
run_with_stubs "$TEST_ROOT/default-target.log" -T jagent "$ARTIFACT_ROOT" \
    >"$TEST_ROOT/default-target.out"
assert_log_empty "$TEST_ROOT/default-target.log"
mkdir -p "$ARTIFACT_ROOT/jagent/shared-target/release"
printf 'rlib\n' >"$ARTIFACT_ROOT/jagent/shared-target/release/libjagent.rlib"
: >"$TEST_ROOT/relative-target.log"
CARGO_TARGET_DIR=shared-target run_with_stubs "$TEST_ROOT/relative-target.log" \
    -T jagent "$ARTIFACT_ROOT" >"$TEST_ROOT/relative-target.out"
assert_log_empty "$TEST_ROOT/relative-target.log"

# Repository names and fetched commit subjects cannot inject terminal control
# sequences or forge the pipe-delimited summary protocol.
DISPLAY_ROOT="$TEST_ROOT/display"
malicious_name=$'bad\\path|repo\033[31m\u2028\u2029\u202E\u2066\u200B'
init_repo "$DISPLAY_ROOT/$malicious_name"
/bin/bash "$UPDATER" -N "$DISPLAY_ROOT" >"$TEST_ROOT/display.out"
if LC_ALL=C grep -q $'\033' "$TEST_ROOT/display.out"; then
    fail "repository name emitted a raw terminal escape"
fi
assert_contains "$TEST_ROOT/display.out" 'bad\\path\x7Crepo\u{001B}[31m\u{2028}\u{2029}\u{202E}\u{2066}\u{200B}'
LC_ALL=C /bin/bash "$UPDATER" -N "$DISPLAY_ROOT" >"$TEST_ROOT/display-c-locale.out"
for unsafe in $'\u2028' $'\u2029' $'\u202E' $'\u2066' $'\u200B'; do
    if LC_ALL=C grep -Fq -- "$unsafe" "$TEST_ROOT/display-c-locale.out"; then
        fail "repository name emitted a raw Unicode display control under the C locale"
    fi
done

REMOTE_ROOT="$TEST_ROOT/local-remote"
git init -q --bare "$REMOTE_ROOT/remote.git"
init_repo "$REMOTE_ROOT/seed"
git -C "$REMOTE_ROOT/seed" remote add origin "$REMOTE_ROOT/remote.git"
git -C "$REMOTE_ROOT/seed" push -q -u origin HEAD
git clone -q "$REMOTE_ROOT/remote.git" "$REMOTE_ROOT/jagent"
printf 'upgrade\n' >>"$REMOTE_ROOT/seed/tracked.txt"
git -C "$REMOTE_ROOT/seed" add tracked.txt
git -C "$REMOTE_ROOT/seed" commit -q -m $'upgrade\033[2J|forged\u2028\u2029\u202E\u2066'
git -C "$REMOTE_ROOT/seed" push -q
/bin/bash "$UPDATER" -N -T jagent "$REMOTE_ROOT" >"$TEST_ROOT/commit.out"
if LC_ALL=C grep -q $'\033' "$TEST_ROOT/commit.out"; then
    fail "commit subject emitted a raw terminal escape"
fi
assert_contains "$TEST_ROOT/commit.out" 'upgrade\u{001B}[2J\x7Cforged\u{2028}\u{2029}\u{202E}\u{2066}'

# A remote can write arbitrary push sideband. It must be rendered as data, not
# replayed as terminal control sequences or summary-looking delimiters.
SIDEBAND_ROOT="$TEST_ROOT/sideband"
git init -q --bare "$SIDEBAND_ROOT/remote.git"
init_repo "$SIDEBAND_ROOT/jagent"
git -C "$SIDEBAND_ROOT/jagent" remote add origin "$SIDEBAND_ROOT/remote.git"
git -C "$SIDEBAND_ROOT/jagent" push -q -u origin HEAD
printf '%s\n' \
    '#!/bin/sh' \
    'cat >/dev/null' \
    'printf '\''remote\033[31m|forged  ‮⁦\n'\'' >&2' \
    >"$SIDEBAND_ROOT/remote.git/hooks/pre-receive"
chmod +x "$SIDEBAND_ROOT/remote.git/hooks/pre-receive"
printf 'ahead\n' >>"$SIDEBAND_ROOT/jagent/tracked.txt"
git -C "$SIDEBAND_ROOT/jagent" add tracked.txt
git -C "$SIDEBAND_ROOT/jagent" commit -q -m ahead
/bin/bash "$UPDATER" -N -u -T jagent "$SIDEBAND_ROOT" >"$TEST_ROOT/sideband.out"
if LC_ALL=C grep -q $'\033' "$TEST_ROOT/sideband.out"; then
    fail "remote sideband emitted a raw terminal escape"
fi
assert_contains "$TEST_ROOT/sideband.out" 'remote\u{001B}[31m\x7Cforged\u{2028}\u{2029}\u{202E}\u{2066}'
for unsafe in $'\u2028' $'\u2029' $'\u202E' $'\u2066'; do
    if LC_ALL=C grep -Fq -- "$unsafe" "$TEST_ROOT/sideband.out"; then
        fail "remote sideband emitted a raw Unicode display control"
    fi
done

printf 'test-git-update-all: ok\n'
