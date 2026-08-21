#!/usr/bin/env bash
# Install, roll back, or uninstall a versioned JWM release bundle.
set -euo pipefail
umask 022

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)

usage() {
    printf '%s\n' \
        "Usage:" \
        "  $(basename -- "$0") install [--destdir DIR] [--replace]" \
        "  $(basename -- "$0") rollback [--destdir DIR]" \
        "  $(basename -- "$0") uninstall [--destdir DIR] [--version VERSION | --all]" \
        "" \
        "install places immutable payloads below /usr/local/lib/jwm/versions and" \
        "updates stable relative symlinks. A first install never replaces an" \
        "existing destination unless --replace is explicit; replaced objects are" \
        "restored by the final uninstall." \
        "" \
        "--destdir prefixes every filesystem path and is intended for packaging" \
        "and lifecycle tests. uninstall without --version is the same as --all."
}

die() {
    printf 'install-release: %s\n' "$*" >&2
    exit 1
}

[[ $# -gt 0 ]] || {
    usage >&2
    exit 2
}

ACTION=$1
shift
DESTDIR=""
REPLACE=0
REMOVE_VERSION=""
REMOVE_ALL=0

case "$ACTION" in
    install|rollback|uninstall) ;;
    -h|--help)
        usage
        exit 0
        ;;
    *)
        usage >&2
        die "unknown action: $ACTION"
        ;;
esac

while (($#)); do
    case "$1" in
        --destdir)
            [[ $# -ge 2 && -n $2 ]] || die "--destdir requires a value"
            DESTDIR=$2
            shift 2
            ;;
        --destdir=*)
            DESTDIR=${1#*=}
            [[ -n $DESTDIR ]] || die "--destdir requires a value"
            shift
            ;;
        --replace)
            [[ $ACTION == install ]] || die "--replace is only valid with install"
            REPLACE=1
            shift
            ;;
        --version)
            [[ $ACTION == uninstall ]] || die "--version is only valid with uninstall"
            [[ $# -ge 2 && -n $2 ]] || die "--version requires a value"
            REMOVE_VERSION=$2
            shift 2
            ;;
        --version=*)
            [[ $ACTION == uninstall ]] || die "--version is only valid with uninstall"
            REMOVE_VERSION=${1#*=}
            [[ -n $REMOVE_VERSION ]] || die "--version requires a value"
            shift
            ;;
        --all)
            [[ $ACTION == uninstall ]] || die "--all is only valid with uninstall"
            REMOVE_ALL=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *) die "unknown option: $1" ;;
    esac
done

[[ -z $REMOVE_VERSION || $REMOVE_ALL == 0 ]] || die "--version and --all are mutually exclusive"
if [[ $ACTION == uninstall && -z $REMOVE_VERSION ]]; then
    REMOVE_ALL=1
fi

valid_version() {
    [[ $1 =~ ^[0-9A-Za-z][0-9A-Za-z._+-]{0,127}$ && $1 != *..* ]]
}

valid_relative_path() {
    local value=$1 component
    local -a components=()
    [[ -n $value && $value != /* && $value != *$'\n'* && $value != *$'\r'* && $value != *$'\t'* ]] || return 1
    [[ $value =~ ^[0-9A-Za-z._+/@-]+$ ]] || return 1
    IFS=/ read -r -a components <<< "$value"
    for component in "${components[@]}"; do
        [[ -n $component && $component != . && $component != .. ]] || return 1
    done
}

valid_stable_path() {
    local value=$1
    valid_relative_path "$value" || return 1
    case "$value" in
        usr/local/bin/jwm|\
        usr/local/bin/jwm-tool|\
        usr/local/bin/jwm-support|\
        usr/local/bin/jwm-bridge|\
        usr/local/bin/tao_glow_bar|\
        usr/local/bin/tao_pixels_bar|\
        usr/share/xsessions/jwm-x11rb.desktop|\
        usr/share/xsessions/jwm-x11rb-debug.desktop|\
        usr/share/xsessions/jwm-xcb.desktop|\
        usr/share/xsessions/jwm-xcb-debug.desktop|\
        usr/share/wayland-sessions/jwm-wayland.desktop|\
        usr/share/wayland-sessions/jwm-wayland-debug.desktop|\
        usr/share/dbus-1/services/org.freedesktop.Notifications.service|\
        usr/local/share/doc/jwm)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

normalize_destdir() {
    local value=$1 component
    local -a components=()
    [[ $value == /* && $value != *$'\n'* && $value != *$'\r'* && $value != *$'\t'* ]] || die "--destdir must be an absolute path without control characters"
    [[ $value != *//* ]] || die "--destdir must not contain empty path components"
    IFS=/ read -r -a components <<< "${value#/}"
    for component in "${components[@]}"; do
        [[ -n $component && $component != . && $component != .. ]] || die "--destdir must not contain '.' or '..' components"
    done
    if [[ -e $value || -L $value ]]; then
        [[ -d $value && ! -L $value ]] || die "--destdir is not a real directory: $value"
    else
        mkdir -p -- "$value"
    fi
    (cd -- "$value" && pwd -P)
}

if [[ -n $DESTDIR ]]; then
    DESTDIR=$(normalize_destdir "$DESTDIR")
    [[ $DESTDIR != / ]] || DESTDIR=""
elif ((EUID != 0)); then
    die "root privileges are required without --destdir"
fi

root_path() {
    if [[ -n $DESTDIR ]]; then
        printf '%s/%s\n' "$DESTDIR" "$1"
    else
        printf '/%s\n' "$1"
    fi
}

BASE=$(root_path usr/local/lib/jwm)
STATE="$BASE/.release-state"
VERSIONS="$BASE/versions"
CURRENT="$BASE/current"

# Validate every existing parent before mkdir can follow it. On merged-/usr
# systems /usr itself is still a real directory; symlinked managed parents are
# deliberately rejected because they could escape DESTDIR.
check_parent_chain() {
    local relative=$1 include_leaf=${2:-0} cursor component limit index
    local -a components=()
    valid_relative_path "$relative" || die "unsafe managed path: $relative"
    IFS=/ read -r -a components <<< "$relative"
    limit=$((${#components[@]} - (include_leaf == 1 ? 0 : 1)))
    cursor=${DESTDIR:-}
    for ((index = 0; index < limit; index++)); do
        component=${components[$index]}
        cursor="$cursor/$component"
        if [[ -L $cursor ]]; then
            die "managed path has a symlink parent: $cursor"
        fi
        if [[ -e $cursor && ! -d $cursor ]]; then
            die "managed path has a non-directory parent: $cursor"
        fi
    done
}

make_parent_chain() {
    local relative=$1 include_leaf=${2:-0} cursor component limit index
    local -a components=()
    check_parent_chain "$relative" "$include_leaf"
    IFS=/ read -r -a components <<< "$relative"
    limit=$((${#components[@]} - (include_leaf == 1 ? 0 : 1)))
    cursor=${DESTDIR:-}
    for ((index = 0; index < limit; index++)); do
        component=${components[$index]}
        cursor="$cursor/$component"
        [[ -d $cursor ]] || mkdir -- "$cursor"
    done
}

relative_link_target() {
    local destination=$1 version_path=$2 from target index common result="" component
    local -a from_parts=() target_parts=()
    from=${destination%/*}
    target="usr/local/lib/jwm/current/$version_path"
    IFS=/ read -r -a from_parts <<< "$from"
    IFS=/ read -r -a target_parts <<< "$target"
    common=0
    while ((common < ${#from_parts[@]} && common < ${#target_parts[@]})) &&
          [[ ${from_parts[$common]} == "${target_parts[$common]}" ]]; do
        ((common += 1))
    done
    for ((index = common; index < ${#from_parts[@]}; index++)); do
        result+="../"
    done
    for ((index = common; index < ${#target_parts[@]}; index++)); do
        component=${target_parts[$index]}
        result+="$component"
        ((index + 1 == ${#target_parts[@]})) || result+="/"
    done
    [[ -n $result && $result != /* ]] || die "could not form a relative link for $destination"
    printf '%s\n' "$result"
}

declare -a MANIFEST_KINDS=()
declare -a MANIFEST_SOURCES=()
declare -a MANIFEST_VERSION_PATHS=()
declare -a MANIFEST_STABLE_PATHS=()
declare -a MANIFEST_MODES=()
declare -a STABLE_VERSION_PATHS=()
declare -a STABLE_DESTINATIONS=()

load_manifest() {
    local manifest=$1 line=0 kind source version_path stable_path mode extra index
    local -A seen_version=() seen_stable=()
    [[ -f $manifest && ! -L $manifest ]] || die "missing regular release manifest: $manifest"
    while IFS=$'\t' read -r kind source version_path stable_path mode extra || [[ -n ${kind:-} ]]; do
        ((line += 1))
        [[ -n ${kind:-} ]] || continue
        [[ $kind == \#* ]] && continue
        [[ -z ${extra:-} ]] || die "manifest line $line has too many columns"
        case "$kind" in
            built|repo-file|repo-tree|stable-link) ;;
            *) die "manifest line $line has unknown kind: $kind" ;;
        esac
        valid_relative_path "$version_path" || die "manifest line $line has an unsafe version path"
        [[ -z ${seen_version[$version_path]+set} ]] || die "duplicate manifest version path: $version_path"
        seen_version[$version_path]=1
        if [[ $kind == stable-link ]]; then
            [[ $source == - && $mode == - && $stable_path != - ]] || die "manifest line $line has an invalid stable-link entry"
        else
            valid_relative_path "$source" || die "manifest line $line has an unsafe source path"
            [[ $mode == 0644 || $mode == 0755 ]] || die "manifest line $line has an invalid mode"
        fi
        if [[ $stable_path != - ]]; then
            valid_stable_path "$stable_path" || die "manifest line $line has an unsafe stable path"
            [[ -z ${seen_stable[$stable_path]+set} ]] || die "duplicate manifest stable path: $stable_path"
            seen_stable[$stable_path]=1
            STABLE_VERSION_PATHS+=("$version_path")
            STABLE_DESTINATIONS+=("$stable_path")
        fi
        MANIFEST_KINDS+=("$kind")
        MANIFEST_SOURCES+=("$source")
        MANIFEST_VERSION_PATHS+=("$version_path")
        MANIFEST_STABLE_PATHS+=("$stable_path")
        MANIFEST_MODES+=("$mode")
    done < "$manifest"
    ((${#MANIFEST_KINDS[@]} > 0 && ${#STABLE_DESTINATIONS[@]} > 0)) || die "release manifest is empty"

    local -a required=(
        usr/local/bin/jwm
        usr/local/bin/jwm-tool
        usr/local/bin/jwm-support
        usr/local/bin/jwm-bridge
        usr/local/bin/tao_glow_bar
        usr/local/bin/tao_pixels_bar
        usr/share/xsessions/jwm-x11rb.desktop
        usr/share/xsessions/jwm-x11rb-debug.desktop
        usr/share/xsessions/jwm-xcb.desktop
        usr/share/xsessions/jwm-xcb-debug.desktop
        usr/share/wayland-sessions/jwm-wayland.desktop
        usr/share/wayland-sessions/jwm-wayland-debug.desktop
        usr/share/dbus-1/services/org.freedesktop.Notifications.service
        usr/local/share/doc/jwm
    )
    local required_path found
    for required_path in "${required[@]}"; do
        found=0
        for stable_path in "${STABLE_DESTINATIONS[@]}"; do
            [[ $stable_path == "$required_path" ]] && found=1
        done
        ((found == 1)) || die "manifest omits required destination: $required_path"
    done
}

validate_payload() {
    local payload=$1 index path kind mode
    [[ -d $payload && ! -L $payload ]] || die "missing release payload: $payload"
    if find "$payload" \( -type l -o ! -type d ! -type f \) -print -quit | grep -q .; then
        die "release payload contains a symlink or special file"
    fi
    for ((index = 0; index < ${#MANIFEST_KINDS[@]}; index++)); do
        kind=${MANIFEST_KINDS[$index]}
        mode=${MANIFEST_MODES[$index]}
        path="$payload/${MANIFEST_VERSION_PATHS[$index]}"
        case "$kind" in
            built|repo-file)
                [[ -f $path && ! -L $path ]] || die "payload file is missing: ${MANIFEST_VERSION_PATHS[$index]}"
                if [[ $mode == 0755 ]]; then
                    [[ -x $path ]] || die "payload file is not executable: ${MANIFEST_VERSION_PATHS[$index]}"
                fi
                ;;
            repo-tree|stable-link)
                [[ -d $path && ! -L $path ]] || die "payload directory is missing: ${MANIFEST_VERSION_PATHS[$index]}"
                ;;
        esac
    done
}

copy_payload_normalized() {
    local payload=$1 staging=$2 index kind mode source destination directory relative file
    for ((index = 0; index < ${#MANIFEST_KINDS[@]}; index++)); do
        kind=${MANIFEST_KINDS[$index]}
        mode=${MANIFEST_MODES[$index]}
        source="$payload/${MANIFEST_VERSION_PATHS[$index]}"
        destination="$staging/${MANIFEST_VERSION_PATHS[$index]}"
        case "$kind" in
            built|repo-file)
                mkdir -p -- "$(dirname -- "$destination")"
                install -m "$mode" -- "$source" "$destination"
                ;;
            repo-tree)
                mkdir -p -- "$destination"
                while IFS= read -r -d '' directory; do
                    relative=${directory#"$source"/}
                    [[ $directory == "$source" ]] && relative=""
                    [[ -z $relative ]] || mkdir -p -- "$destination/$relative"
                done < <(find "$source" -type d -print0)
                while IFS= read -r -d '' file; do
                    relative=${file#"$source"/}
                    mkdir -p -- "$(dirname -- "$destination/$relative")"
                    install -m "$mode" -- "$file" "$destination/$relative"
                done < <(find "$source" -type f -print0)
                ;;
            stable-link)
                mkdir -p -- "$destination"
                ;;
        esac
    done
    # Never preserve archive ownership, set-id bits, or writable directory
    # modes. In a real install this makes every immutable version object
    # root-owned even when an unprivileged user extracted the bundle.
    find "$staging" -type d -exec chmod 0755 {} +
}

state_file() {
    printf '%s/%s\n' "$STATE" "$1"
}

validate_state() {
    local file version version_path destination extra
    [[ -d $STATE && ! -L $STATE ]] || die "no managed JWM release is installed"
    [[ -d $VERSIONS && ! -L $VERSIONS ]] || die "managed versions directory is missing or unsafe"
    [[ -d $STATE/backups && ! -L $STATE/backups ]] || die "managed backup directory is missing or unsafe"
    for file in format installed history stable-paths.tsv backups.tsv base-preexisting; do
        [[ -f $STATE/$file && ! -L $STATE/$file ]] || die "installation state is incomplete: $file"
    done
    [[ $(<"$STATE/format") == 1 ]] || die "unsupported installation state format"
    [[ $(<"$STATE/base-preexisting") == 0 || $(<"$STATE/base-preexisting") == 1 ]] || die "invalid base-preexisting state"
    while IFS= read -r version || [[ -n $version ]]; do
        [[ -n $version ]] || continue
        valid_version "$version" || die "unsafe version in installation state"
    done < "$STATE/installed"
    while IFS= read -r version || [[ -n $version ]]; do
        [[ -n $version ]] || continue
        valid_version "$version" || die "unsafe version in rollback history"
    done < "$STATE/history"
    while IFS=$'\t' read -r version_path destination extra || [[ -n ${version_path:-} ]]; do
        [[ -n ${version_path:-} && -z ${extra:-} ]] || die "invalid stable-paths state"
        valid_relative_path "$version_path" || die "unsafe version path in installation state"
        valid_stable_path "$destination" || die "unsafe stable path in installation state"
    done < "$STATE/stable-paths.tsv"
    while IFS= read -r destination || [[ -n $destination ]]; do
        [[ -n $destination ]] || continue
        if [[ $destination != usr/local/lib/jwm/current ]]; then
            valid_stable_path "$destination" || die "unsafe backup path in installation state"
        fi
    done < "$STATE/backups.tsv"
}

load_stable_state() {
    local version_path destination extra
    STABLE_VERSION_PATHS=()
    STABLE_DESTINATIONS=()
    while IFS=$'\t' read -r version_path destination extra || [[ -n ${version_path:-} ]]; do
        [[ -n ${version_path:-} && -z ${extra:-} ]] || die "invalid stable-paths state"
        valid_relative_path "$version_path" || die "unsafe version path in installation state"
        valid_stable_path "$destination" || die "unsafe stable path in installation state"
        STABLE_VERSION_PATHS+=("$version_path")
        STABLE_DESTINATIONS+=("$destination")
    done < "$STATE/stable-paths.tsv"
}

assert_stable_links() {
    local index destination expected actual
    for ((index = 0; index < ${#STABLE_DESTINATIONS[@]}; index++)); do
        check_parent_chain "${STABLE_DESTINATIONS[$index]}" 0
        destination=$(root_path "${STABLE_DESTINATIONS[$index]}")
        expected=$(relative_link_target "${STABLE_DESTINATIONS[$index]}" "${STABLE_VERSION_PATHS[$index]}")
        [[ -L $destination ]] || die "managed destination was changed: $destination"
        actual=$(readlink -- "$destination")
        [[ $actual == "$expected" ]] || die "managed destination has an unexpected link target: $destination"
    done
}

assert_current_link() {
    local target version value found=0
    local -a installed=() history=()
    [[ -L $CURRENT ]] || die "managed current link is missing"
    target=$(readlink -- "$CURRENT")
    [[ $target == versions/* ]] || die "managed current link is invalid"
    version=${target#versions/}
    valid_version "$version" || die "managed current link has an unsafe version"
    [[ $target == "versions/$version" ]] || die "managed current link is not canonical"
    [[ -d $VERSIONS/$version && ! -L $VERSIONS/$version ]] || die "managed current version is missing or unsafe"
    read_versions "$STATE/installed" installed
    read_versions "$STATE/history" history
    for value in "${installed[@]}"; do
        [[ $value == "$version" ]] && found=1
    done
    ((found == 1)) || die "managed current version is absent from installation state"
    ((${#history[@]} > 0)) || die "rollback history is empty"
    [[ ${history[-1]} == "$version" ]] || die "managed current link disagrees with rollback history"
}

atomic_lines() {
    local destination=$1
    shift
    local temporary="$destination.tmp.$$" value
    : > "$temporary"
    for value in "$@"; do
        printf '%s\n' "$value" >> "$temporary"
    done
    mv -f -- "$temporary" "$destination"
}

set_current() {
    local version=$1 temporary="$BASE/.current.$$"
    valid_version "$version" || die "unsafe current version"
    [[ -d $VERSIONS/$version && ! -L $VERSIONS/$version ]] || die "installed version is missing: $version"
    rm -f -- "$temporary"
    ln -s -- "versions/$version" "$temporary"
    mv -Tf -- "$temporary" "$CURRENT"
}

backup_existing() {
    local relative=$1 source backup
    source=$(root_path "$relative")
    [[ -e $source || -L $source ]] || return 0
    backup="$STATE/backups/$relative"
    [[ ! -e $backup && ! -L $backup ]] || die "backup destination already exists: $relative"
    mkdir -p -- "$(dirname -- "$backup")"
    mv -- "$source" "$backup"
    printf '%s\n' "$relative" >> "$STATE/backups.tsv"
}

restore_backups() {
    local relative backup destination
    while IFS= read -r relative || [[ -n $relative ]]; do
        [[ -n $relative ]] || continue
        backup="$STATE/backups/$relative"
        destination=$(root_path "$relative")
        [[ -e $backup || -L $backup ]] || die "recorded backup is missing: $relative"
        [[ ! -e $destination && ! -L $destination ]] || die "cannot restore occupied backup destination: $relative"
        make_parent_chain "$relative" 0
        mv -- "$backup" "$destination"
    done < "$STATE/backups.tsv"
}

read_versions() {
    local filename=$1 destination_name=$2 value
    local -n destination_ref=$destination_name
    destination_ref=()
    while IFS= read -r value || [[ -n $value ]]; do
        [[ -n $value ]] || continue
        valid_version "$value" || die "unsafe version in state"
        destination_ref+=("$value")
    done < "$filename"
}

install_release() {
    local manifest="$SCRIPT_DIR/release-manifest.tsv" version_file="$SCRIPT_DIR/VERSION"
    local version payload index destination expected collision base_preexisting new_install=0
    local -a collisions=() installed=() history=() stored_map=() bundle_map=()

    [[ -f $version_file && ! -L $version_file ]] || die "missing regular VERSION file"
    IFS= read -r version < "$version_file" || true
    valid_version "$version" || die "unsafe bundle version: $version"
    [[ $(wc -l < "$version_file") -eq 1 ]] || die "VERSION must contain exactly one line"
    payload="$SCRIPT_DIR/payload/usr/local/lib/jwm/versions/$version"
    load_manifest "$manifest"
    validate_payload "$payload"

    check_parent_chain usr/local/lib/jwm 1
    check_parent_chain usr/local/lib/jwm/versions 1
    for destination in "${STABLE_DESTINATIONS[@]}"; do
        check_parent_chain "$destination" 0
    done

    if [[ -e $STATE || -L $STATE ]]; then
        validate_state
        load_stable_state
        mapfile -t stored_map < "$STATE/stable-paths.tsv"
        for ((index = 0; index < ${#MANIFEST_STABLE_PATHS[@]}; index++)); do
            if [[ ${MANIFEST_STABLE_PATHS[$index]} != - ]]; then
                bundle_map+=("${MANIFEST_VERSION_PATHS[$index]}"$'\t'"${MANIFEST_STABLE_PATHS[$index]}")
            fi
        done
        [[ ${#stored_map[@]} -eq ${#bundle_map[@]} ]] || die "bundle stable paths do not match the installed release"
        for ((index = 0; index < ${#stored_map[@]}; index++)); do
            [[ ${stored_map[$index]} == "${bundle_map[$index]}" ]] || die "bundle stable paths do not match the installed release"
        done
        load_stable_state
        assert_stable_links
        assert_current_link
        read_versions "$STATE/installed" installed
        for collision in "${installed[@]}"; do
            [[ $collision != "$version" ]] || die "version is already installed: $version"
        done
        [[ ! -e $VERSIONS/$version && ! -L $VERSIONS/$version ]] || die "version directory already exists outside installation state: $version"
    else
        [[ ! -e $STATE && ! -L $STATE ]] || die "unsafe installation state path"
        new_install=1
        if [[ -e $CURRENT || -L $CURRENT ]]; then
            collisions+=(usr/local/lib/jwm/current)
        fi
        [[ ! -e $VERSIONS/$version && ! -L $VERSIONS/$version ]] || die "version directory already exists: $VERSIONS/$version"
        for destination in "${STABLE_DESTINATIONS[@]}"; do
            if [[ -e $(root_path "$destination") || -L $(root_path "$destination") ]]; then
                collisions+=("$destination")
            fi
        done
        if ((${#collisions[@]} > 0 && REPLACE == 0)); then
            printf 'install-release: existing destinations require --replace:\n' >&2
            printf '  /%s\n' "${collisions[@]}" >&2
            exit 1
        fi
        base_preexisting=0
        [[ -d $BASE && ! -L $BASE ]] && base_preexisting=1
        make_parent_chain usr/local/lib/jwm 1
        [[ -d $BASE ]] || die "failed to create the JWM library directory"
        mkdir -- "$STATE"
        mkdir -- "$STATE/backups"
        printf '1\n' > "$STATE/format"
        printf '%s\n' "$base_preexisting" > "$STATE/base-preexisting"
        : > "$STATE/installed"
        : > "$STATE/history"
        : > "$STATE/backups.tsv"
        : > "$STATE/stable-paths.tsv"
        for ((index = 0; index < ${#STABLE_DESTINATIONS[@]}; index++)); do
            printf '%s\t%s\n' "${STABLE_VERSION_PATHS[$index]}" "${STABLE_DESTINATIONS[$index]}" >> "$STATE/stable-paths.tsv"
        done
        for collision in "${collisions[@]}"; do
            backup_existing "$collision"
        done
    fi

    make_parent_chain usr/local/lib/jwm/versions 1
    [[ -d $VERSIONS ]] || die "failed to create versions directory"
    local staging="$VERSIONS/.install-$version.$$"
    [[ ! -e $staging && ! -L $staging ]] || die "temporary version path already exists"
    mkdir -- "$staging"
    copy_payload_normalized "$payload" "$staging"
    install -m 0644 -- "$manifest" "$staging/.release-manifest.tsv"
    mv -- "$staging" "$VERSIONS/$version"

    if ((new_install == 0)); then
        read_versions "$STATE/installed" installed
        read_versions "$STATE/history" history
    fi
    installed+=("$version")
    history+=("$version")
    atomic_lines "$STATE/installed" "${installed[@]}"
    atomic_lines "$STATE/history" "${history[@]}"
    set_current "$version"

    for ((index = 0; index < ${#STABLE_DESTINATIONS[@]}; index++)); do
        destination=$(root_path "${STABLE_DESTINATIONS[$index]}")
        expected=$(relative_link_target "${STABLE_DESTINATIONS[$index]}" "${STABLE_VERSION_PATHS[$index]}")
        if [[ -L $destination ]]; then
            [[ $(readlink -- "$destination") == "$expected" ]] || die "managed destination changed during install: $destination"
            continue
        fi
        [[ ! -e $destination ]] || die "managed destination became occupied during install: $destination"
        make_parent_chain "${STABLE_DESTINATIONS[$index]}" 0
        ln -s -- "$expected" "$destination"
    done

    printf 'Installed JWM %s\n' "$version"
}

rollback_release() {
    local -a history=()
    local removed target
    validate_state
    load_stable_state
    assert_stable_links
    assert_current_link
    read_versions "$STATE/history" history
    ((${#history[@]} >= 2)) || die "no previous JWM release is available"
    removed=${history[-1]}
    unset 'history[-1]'
    target=${history[-1]}
    set_current "$target"
    atomic_lines "$STATE/history" "${history[@]}"
    printf 'Rolled back JWM from %s to %s\n' "$removed" "$target"
}

final_uninstall() {
    local -a installed=()
    local index destination expected version base_preexisting
    validate_state
    load_stable_state
    assert_stable_links
    assert_current_link
    read_versions "$STATE/installed" installed
    for ((index = 0; index < ${#STABLE_DESTINATIONS[@]}; index++)); do
        destination=$(root_path "${STABLE_DESTINATIONS[$index]}")
        expected=$(relative_link_target "${STABLE_DESTINATIONS[$index]}" "${STABLE_VERSION_PATHS[$index]}")
        [[ -L $destination && $(readlink -- "$destination") == "$expected" ]] || die "refusing to remove changed destination: $destination"
    done
    for destination in "${STABLE_DESTINATIONS[@]}"; do
        unlink -- "$(root_path "$destination")"
    done
    unlink -- "$CURRENT"
    for version in "${installed[@]}"; do
        valid_version "$version" || die "unsafe installed version"
        [[ $VERSIONS/$version == "$BASE/versions/$version" ]] || die "unsafe version removal path"
        if [[ -e $VERSIONS/$version || -L $VERSIONS/$version ]]; then
            [[ -d $VERSIONS/$version && ! -L $VERSIONS/$version ]] || die "installed version path was changed: $version"
            rm -rf -- "${VERSIONS:?}/$version"
        fi
    done
    restore_backups
    base_preexisting=$(<"$STATE/base-preexisting")
    rm -rf -- "$STATE"
    rmdir -- "$VERSIONS" 2>/dev/null || true
    if [[ $base_preexisting == 0 ]]; then
        rmdir -- "$BASE" 2>/dev/null || true
    fi
    printf 'Uninstalled all managed JWM releases\n'
}

uninstall_one() {
    local requested=$1 version active="" replacement=""
    local -a installed=() history=() kept_installed=() kept_history=()
    local found=0 value
    valid_version "$requested" || die "unsafe uninstall version: $requested"
    validate_state
    load_stable_state
    assert_stable_links
    assert_current_link
    read_versions "$STATE/installed" installed
    read_versions "$STATE/history" history
    for value in "${installed[@]}"; do
        if [[ $value == "$requested" ]]; then
            found=1
        else
            kept_installed+=("$value")
        fi
    done
    ((found == 1)) || die "version is not installed: $requested"
    if ((${#kept_installed[@]} == 0)); then
        final_uninstall
        return
    fi
    ((${#history[@]} > 0)) && active=${history[-1]}
    for value in "${history[@]}"; do
        [[ $value == "$requested" ]] || kept_history+=("$value")
    done
    if [[ $active == "$requested" ]]; then
        if ((${#kept_history[@]} > 0)); then
            replacement=${kept_history[-1]}
        else
            replacement=${kept_installed[-1]}
            kept_history+=("$replacement")
        fi
        set_current "$replacement"
    fi
    [[ $VERSIONS/$requested == "$BASE/versions/$requested" ]] || die "unsafe version removal path"
    [[ -d $VERSIONS/$requested && ! -L $VERSIONS/$requested ]] || die "installed version path was changed: $requested"
    rm -rf -- "${VERSIONS:?}/$requested"
    atomic_lines "$STATE/installed" "${kept_installed[@]}"
    atomic_lines "$STATE/history" "${kept_history[@]}"
    printf 'Uninstalled JWM %s\n' "$requested"
}

for tool in chmod dirname find grep install ln mkdir mv readlink rm rmdir unlink wc; do
    command -v "$tool" >/dev/null 2>&1 || die "required command not found: $tool"
done

check_parent_chain usr/local/lib/jwm 1

case "$ACTION" in
    install) install_release ;;
    rollback) rollback_release ;;
    uninstall)
        if ((REMOVE_ALL == 1)); then
            final_uninstall
        else
            uninstall_one "$REMOVE_VERSION"
        fi
        ;;
esac
