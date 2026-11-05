#!/usr/bin/env bash
# install-release.sh: install a tools/package.sh archive on this machine and keep the release
# directory tidy. Releases live under $PREFIX/lib/memetag/releases/VERSION-HASH; the three
# $PREFIX/bin links are repointed atomically; afterwards only the current release, the previous
# one and any pinned names remain. "Previous" is the release that ran before this install,
# except that a release installed less than MEMETAG_ITERATION_HOURS ago counts as an iteration
# and is dropped, so a rollback always lands on something that ran for a while. A release with a
# live process is never removed. Every action is appended to releases/install.log.
set -euo pipefail
program='memetag-install-release'
if [[ ${1:-} == --version ]]; then echo "$program 0.3.1"; exit 0; fi
if [[ $# -eq 0 || ${1:-} == --help ]]; then
    cat <<'USAGE'
Usage: tools/install-release.sh ARCHIVE.tar.gz [--dry-run]   install a full archive from package.sh
       tools/install-release.sh --rollback [--dry-run]       swap current and previous
       tools/install-release.sh --list                       show releases and their roles
       tools/install-release.sh --pin NAME | --unpin NAME    protect a release from pruning
Environment: MEMETAG_PREFIX (default ~/.local), MEMETAG_ITERATION_HOURS (default 24).
Only full archives (memetag, memetag-gui, memetag-infer) are installed, so a release never mixes builds.
The user units are not touched: they call $PREFIX/bin/memetag through the link.
USAGE
    exit 0
fi

prefix=${MEMETAG_PREFIX:-$HOME/.local}
iteration_hours=${MEMETAG_ITERATION_HOURS:-24}
releases="$prefix/lib/memetag/releases"
bindir="$prefix/bin"
programs=(memetag memetag-gui memetag-infer)
dry_run=''
for arg in "$@"; do [[ $arg == --dry-run ]] && dry_run=1; done
mkdir -p "$releases" "$bindir"

log() { printf '%s %s\n' "$(date -Is)" "$*" >> "$releases/install.log"; }
say() { printf '%s\n' "$*"; }
role_of() { # name -> current/previous/pinned marks
    local name=$1 marks=''
    [[ $(readlink "$releases/CURRENT" 2>/dev/null || true) == "$name" ]] && marks+=' current'
    [[ $(readlink "$releases/PREVIOUS" 2>/dev/null || true) == "$name" ]] && marks+=' previous'
    [[ -f $releases/PINNED ]] && grep -qxF "$name" "$releases/PINNED" && marks+=' pinned'
    printf '%s' "$marks"
}
in_use_by() { # name -> PIDs of our processes running from that release, if any
    local dir="$releases/$1" pid exe argv0
    for pid in /proc/[0-9]*; do
        exe=$(readlink "$pid/exe" 2>/dev/null || true)
        argv0=$(tr '\0' '\n' 2>/dev/null < "$pid/cmdline" | head -n1 || true)
        if [[ $exe == "$dir/"* || $argv0 == "$dir/"* ]]; then printf '%s ' "${pid#/proc/}"; fi
    done
}
set_link() { # name -> point CURRENT/PREVIOUS (relative) and the bin links at a release
    local mark=$1 name=$2
    ln -sfn "$name" "$releases/.$mark.tmp.$$" && mv -T "$releases/.$mark.tmp.$$" "$releases/$mark"
}
refuse_real_bins() { # the bin entries must be links or absent (memetag-update writes a real file on other machines)
    local p
    for p in "${programs[@]}"; do
        if [[ -e $bindir/$p && ! -L $bindir/$p ]]; then
            say "refusing to replace $bindir/$p: it is a file, not a link" >&2; exit 1
        fi
    done
}
point_bins_at() { # name -> repoint the three bin links atomically
    local name=$1 p
    refuse_real_bins
    for p in "${programs[@]}"; do
        ln -s "$releases/$name/bin/$p" "$bindir/.$p.tmp.$$" && mv -T "$bindir/.$p.tmp.$$" "$bindir/$p"
    done
}
release_names() { # real release directories only: never CURRENT/PREVIOUS (symlinks to directories
    local dir                # match a glob ending in /, and rm -rf on "link/" empties the target)
    for dir in "$releases"/*; do
        [[ -d $dir && ! -L $dir ]] && basename "$dir"
    done
}
pruned=''
prune_except() { # keep-names... -> remove every other release that is not pinned or in use; sets $pruned
    local keep=" $* " name pids
    pruned=''
    for name in $(release_names); do
        [[ $keep == *" $name "* ]] && continue
        [[ -f $releases/PINNED ]] && grep -qxF "$name" "$releases/PINNED" && continue
        pids=$(in_use_by "$name")
        if [[ -n $pids ]]; then say "kept $name: in use by PID $pids"; log "kept $name in-use pids=$pids"; continue; fi
        if [[ -n $dry_run ]]; then say "would remove $name"; else rm -rf -- "${releases:?}/${name:?}"; say "removed $name"; fi
        pruned+="$name "
    done
}

case ${1:-} in
--list)
    for name in $(release_names); do
        dir=$releases/$name
        at=$(cat "$dir/.installed-at" 2>/dev/null || echo 0)
        printf '%-22s installed %s%s%s\n' "$name" "$(date -d "@$at" '+%F %R' 2>/dev/null || echo unknown)" \
            "$(role_of "$name")" "$( [[ -n $(in_use_by "$name") ]] && printf ' in-use' )"
    done
    exit 0 ;;
--pin|--unpin)
    name=${2:?release name}
    [[ -d $releases/$name ]] || { say "no release $name" >&2; exit 1; }
    touch "$releases/PINNED"
    if [[ $1 == --pin ]]; then grep -qxF "$name" "$releases/PINNED" || echo "$name" >> "$releases/PINNED"
    else grep -vxF "$name" "$releases/PINNED" > "$releases/.PINNED.tmp" || true; mv -T "$releases/.PINNED.tmp" "$releases/PINNED"; fi
    log "$1 $name"; say "$1 $name"; exit 0 ;;
--rollback)
    current=$(readlink "$releases/CURRENT" 2>/dev/null || true)
    previous=$(readlink "$releases/PREVIOUS" 2>/dev/null || true)
    [[ -n $previous && -d $releases/$previous ]] || { say 'no previous release to roll back to' >&2; exit 1; }
    say "rollback: $current -> $previous"
    [[ -n $dry_run ]] && exit 0
    point_bins_at "$previous"
    set_link CURRENT "$previous"
    if [[ -n $current ]]; then set_link PREVIOUS "$current"; else rm -f "$releases/PREVIOUS"; fi
    log "rollback current=$previous previous=$current"
    "$bindir/memetag" --version; exit 0 ;;
esac

# Install an archive.
archive=$1
[[ -f $archive ]] || { say "no such archive: $archive" >&2; exit 1; }
incoming=$(mktemp -d "$releases/.incoming.XXXXXXXX")
trap 'rm -rf -- "$incoming"' EXIT
tar -xzf "$archive" -C "$incoming"
tops=("$incoming"/*/)
[[ ${#tops[@]} -eq 1 && -d ${tops[0]} ]] || { say 'archive must hold exactly one top-level directory' >&2; exit 1; }
top=${tops[0]%/}
for p in "${programs[@]}"; do
    [[ -x $top/bin/$p ]] || { say "not a full archive: bin/$p is missing (core and desktop archives are refused)" >&2; exit 1; }
done
[[ -f $top/SHA256SUMS ]] || { say 'archive has no SHA256SUMS' >&2; exit 1; }
(cd "$top" && sha256sum -c --quiet SHA256SUMS) || { say 'checksum mismatch inside the archive' >&2; exit 1; }
versions=()
for p in "${programs[@]}"; do versions+=("$("$top/bin/$p" --version | head -n1)"); done
# "memetag 0.3.1 (6e6dfcd, 2026-10-07)": the name takes the version and memetag's commit. Binaries
# whose crate did not change keep an older embedded commit after a warm build; that is expected.
[[ ${versions[0]} =~ ^memetag\ ([0-9][^[:space:]]*)\ \(([0-9a-f]+), ]] || { say "cannot parse: ${versions[0]}" >&2; exit 1; }
version=${BASH_REMATCH[1]} name="${BASH_REMATCH[1]}-${BASH_REMATCH[2]}"
for i in 1 2; do
    [[ ${versions[$i]} == "${programs[$i]} $version "* ]] || { say "version mismatch: ${versions[$i]} is not $version" >&2; exit 1; }
done

old_current=$(readlink "$releases/CURRENT" 2>/dev/null || true)
old_previous=$(readlink "$releases/PREVIOUS" 2>/dev/null || true)
if [[ -d $releases/$name ]]; then
    cmp -s "$top/SHA256SUMS" "$releases/$name/SHA256SUMS" || { say "$name exists with different binaries; remove it first" >&2; exit 1; }
    reinstall=1
else
    reinstall=''
fi
if [[ $old_current == "$name" || -z $old_current ]]; then
    previous=$old_previous
else
    installed_at=$(cat "$releases/$old_current/.installed-at" 2>/dev/null || echo 0)
    age=$(( $(date +%s) - installed_at ))
    if [[ -n $old_previous && $old_previous != "$old_current" && $old_previous != "$name" && $age -lt $(( iteration_hours * 3600 )) ]]; then
        say "$old_current ran $(( age / 60 )) min: an iteration, previous stays $old_previous"
        previous=$old_previous
    else
        previous=$old_current
    fi
fi
say "install $name${reinstall:+ (already present, relinking)}; previous: ${previous:-none}"
if [[ -n $dry_run ]]; then prune_except "$name" "$previous"; exit 0; fi

refuse_real_bins
if [[ -z $reinstall ]]; then
    date +%s > "$top/.installed-at"
    mv -T "$top" "$releases/$name"
fi
if [[ -f $releases/$name/share/applications/memetag.desktop ]]; then
    install -Dm644 "$releases/$name/share/applications/memetag.desktop" "$prefix/share/applications/memetag.desktop"
    install -Dm644 "$releases/$name/share/icons/hicolor/scalable/apps/memetag.svg" "$prefix/share/icons/hicolor/scalable/apps/memetag.svg"
fi
point_bins_at "$name"
set_link CURRENT "$name"
if [[ -n $previous ]]; then set_link PREVIOUS "$previous"; else rm -f "$releases/PREVIOUS"; fi
prune_except "$name" "$previous"
log "install $name previous=${previous:-none} pruned=${pruned:-none} archive=$(basename "$archive") | ${versions[*]}"
for p in "${programs[@]}"; do "$bindir/$p" --version | head -n1; done
