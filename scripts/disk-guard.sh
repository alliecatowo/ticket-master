#!/bin/sh
# disk-guard.sh -- keep this Mac's disk from ballooning from ticket-master build/scratch output.
#
# Removes, in this repo's primary checkout and every git worktree of it:
#   1. idle `target/` build directories (cargo/rustc/rust-analyzer output only -- never anything
#      else in a checkout);
#   2. worktrees under .claude/worktrees/ that are unlocked, clean, merged, not busy, and not
#      exempt by name (odw-*, tm-integrate);
#   3. old tm scratch directories under $TMPDIR and /tmp.
#
# Idempotent and safe to run unattended every 30 minutes from launchd (see
# `disk:guard:install` in mise.toml). Intended to be run with a minimal launchd PATH, so every
# external command below is invoked by its absolute path and PATH is set explicitly up front.
#
# Flags:
#   --dry-run     print what WOULD be removed; remove nothing.
#   --verbose     print progress and per-item decisions (kept vs removed) as it runs.
#   --aggressive  treat every target dir workspace-wide as eligible after only 15 minutes idle,
#                 same as the automatic behavior when free disk is already below 30GB.
#
# Exits 0 always (even when nothing was freed / nothing to do), so a launchd run is never
# reported as a failure for routine "nothing needed cleaning" runs.

set -u
# Deliberately not `set -e`: several commands here (lsof, git worktree remove, git branch -d,
# find with no matches) are expected to return non-zero in the common case, and the recovery
# from that is "log it and move on to the next item", not "abort the whole run".

PATH="/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

DRY_RUN=0
VERBOSE=0
AGGRESSIVE=0

for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=1 ;;
        --verbose) VERBOSE=1 ;;
        --aggressive) AGGRESSIVE=1 ;;
        *)
            echo "disk-guard.sh: unknown argument: $arg" >&2
            exit 1
            ;;
    esac
done

# ---------------------------------------------------------------------------
# Fixed paths. REPO_ROOT is the primary checkout, pinned to its known absolute path rather than
# derived from $0 or $PWD: a launchd run has no meaningful $PWD, `git worktree list` returns the
# same absolute worktree paths no matter which checkout it's run from (one shared .git), and a
# manual run of a worktree's own copy of this script must still operate on the primary checkout,
# never on that worktree's own repo state.
# ---------------------------------------------------------------------------

REPO_ROOT="/Users/allie/Develop/ticket-master"

WORKTREES_DIR="$REPO_ROOT/.claude/worktrees"
LOG_DIR="$HOME/Library/Logs"
LOG_FILE="$LOG_DIR/tm-disk-guard.log"

GIT="/usr/bin/git"
FIND="/usr/bin/find"
LSOF="/usr/sbin/lsof"
DF="/bin/df"
RM="/bin/rm"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Removal bookkeeping is written to a temp file, not accumulated in a shell variable: several
# of the loops below read their input via a pipe (`... | while read ...`), and on this
# script's target shells (bash/dash as /bin/sh) the last stage of a pipeline runs in a subshell,
# so a plain variable assignment inside one of those loops would be silently lost once the pipe
# finishes. A file survives across subshells.
SUMMARY_FILE="$("/usr/bin/mktemp" -t "disk-guard-summary")" || SUMMARY_FILE="/tmp/tm-disk-guard-summary.$$"
: >"$SUMMARY_FILE"
trap 'rm -f "$SUMMARY_FILE"' EXIT INT TERM

note_removed() {
    # $1: "kind:name", $2: size in KB (0 if unknown)
    echo "$1 $2" >>"$SUMMARY_FILE"
}

vlog() {
    if [ "$VERBOSE" -eq 1 ]; then
        echo "$@"
    fi
}

# free_kb PATH -- KB free on the filesystem containing PATH (falls back to REPO_ROOT's fs if
# PATH doesn't exist yet).
free_kb() {
    _p="$1"
    [ -e "$_p" ] || _p="$REPO_ROOT"
    "$DF" -k "$_p" 2>/dev/null | awk 'NR==2 {print $4}'
}

# du_kb PATH -- size of PATH in KB, 0 if it doesn't exist.
du_kb() {
    if [ -d "$1" ]; then
        /usr/bin/du -sk "$1" 2>/dev/null | awk '{print $1}'
    else
        echo 0
    fi
}

# realpath_p PATH -- resolve PATH to an absolute, symlink-free path via cd -P, without
# following a final path component that is itself a symlink (so callers can refuse a symlinked
# target dir instead of silently deleting whatever it points at).
realpath_p() {
    _dir="$(dirname "$1")"
    _base="$(basename "$1")"
    if [ -d "$1" ] && [ ! -L "$1" ]; then
        ( cd -P "$_dir" 2>/dev/null && printf '%s/%s\n' "$(pwd -P)" "$_base" )
    else
        return 1
    fi
}

# is_under CHILD PARENT -- true if CHILD is PARENT itself or a path beneath it, comparing as
# whole path segments (so "wt-1" never matches a prefix of "wt-10").
is_under() {
    _child="$1"
    _parent="$2"
    case "$_child" in
        "$_parent") return 0 ;;
        "$_parent"/*) return 0 ;;
        *) return 1 ;;
    esac
}

# ---------------------------------------------------------------------------
# safe_rm PATH KIND -- the one guarded deletion path. Refuses anything that isn't a real,
# non-symlinked directory whose resolved path matches one of the shapes this script is allowed
# to delete: a worktree's `target` dir, a worktree directory itself under
# .claude/worktrees/<name>, or a direct child of a resolved scratch root with an allowed name.
# Every actual `rm -rf` in this script goes through here so the distrustful re-read has exactly
# one place to check.
# ---------------------------------------------------------------------------

safe_rm() {
    _path="$1"
    _kind="$2"

    if [ -z "$_path" ] || [ "$_path" = "/" ] || [ "$_path" = "$HOME" ]; then
        echo "disk-guard.sh: refusing unsafe path for $_kind: '$_path'" >&2
        return 1
    fi

    _resolved="$(realpath_p "$_path" 2>/dev/null)" || {
        vlog "  skip (not a real directory or is a symlink): $_path"
        return 1
    }
    [ "$_resolved" = "/" ] && return 1
    [ "$_resolved" = "$HOME" ] && return 1

    case "$_kind" in
        target)
            case "$_resolved" in
                */target) : ;;
                *)
                    echo "disk-guard.sh: refusing non-target path for target deletion: '$_resolved'" >&2
                    return 1
                    ;;
            esac
            ;;
        worktree)
            if ! is_under "$_resolved" "$WORKTREES_DIR"; then
                echo "disk-guard.sh: refusing worktree deletion outside .claude/worktrees: '$_resolved'" >&2
                return 1
            fi
            [ "$_resolved" = "$WORKTREES_DIR" ] && return 1
            ;;
        scratch)
            _parent_resolved="$(dirname "$_resolved")"
            _ok=0
            for _root in $SCRATCH_ROOTS; do
                [ "$_parent_resolved" = "$_root" ] && _ok=1
            done
            if [ "$_ok" -ne 1 ]; then
                echo "disk-guard.sh: refusing scratch path outside resolved scratch roots: '$_resolved'" >&2
                return 1
            fi
            ;;
        *)
            echo "disk-guard.sh: unknown safe_rm kind: '$_kind'" >&2
            return 1
            ;;
    esac

    _before_kb="$(du_kb "$_resolved")"
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "  WOULD REMOVE $_kind: $_resolved"
    else
        vlog "  removing $_kind: $_resolved"
        "$RM" -rf -- "$_resolved"
        note_removed "$_kind:$(basename "$_resolved")" "$_before_kb"
    fi
    return 0
}

# ---------------------------------------------------------------------------
# One lsof snapshot for the whole run: absolute cwd of every process, cached so we don't spawn
# lsof once per checkout. `-d cwd` limits it to the cwd file descriptor; `-Fn` emits one `n<path>`
# line per matching process, filtered below to just the path.
# ---------------------------------------------------------------------------

LSOF_CWDS="$("$LSOF" -nP -d cwd -Fn 2>/dev/null | /usr/bin/sed -n 's/^n//p')"

# any_process_cwd_under PATH -- true if some process's cwd is PATH or beneath it.
any_process_cwd_under() {
    _target="$1"
    printf '%s\n' "$LSOF_CWDS" | while IFS= read -r _cwd; do
        [ -z "$_cwd" ] && continue
        if is_under "$_cwd" "$_target"; then
            echo "hit"
            break
        fi
    done | /usr/bin/grep -q hit
}

# ---------------------------------------------------------------------------
# target_is_busy CHECKOUT_ROOT IDLE_MINUTES -- true (busy) if a cargo/rustc/rust-analyzer
# process's cwd is under CHECKOUT_ROOT (excluding, for the primary checkout, any cwd that is
# itself under WORKTREES_DIR -- the primary's path is a prefix of every worktree's path, so
# without this exclusion a build running in any worktree would pin the primary's target
# forever), or if target/*/.cargo-lock is held open, or if target/*/.fingerprint or
# target/*/deps changed within IDLE_MINUTES.
# ---------------------------------------------------------------------------

target_is_busy() {
    _root="$1"
    _idle_min="$2"
    _target="$_root/target"

    [ -d "$_target" ] || return 1

    # 1. Any process with its cwd inside this checkout (not just inside target/) counts as busy
    #    -- an editor, rust-analyzer or a shell sitting in the checkout while a build is about
    #    to start is reason enough to leave it alone this run.
    _hit=0
    printf '%s\n' "$LSOF_CWDS" | while IFS= read -r _cwd; do
        [ -z "$_cwd" ] && continue
        if [ "$_root" = "$REPO_ROOT" ] && is_under "$_cwd" "$WORKTREES_DIR"; then
            continue
        fi
        if is_under "$_cwd" "$_root"; then
            echo "hit"
            break
        fi
    done | /usr/bin/grep -q hit && _hit=1
    if [ "$_hit" -eq 1 ]; then
        return 0
    fi

    # 2. Any process holding a target/*/.cargo-lock file open.
    for _lock in "$_target"/*/.cargo-lock; do
        [ -e "$_lock" ] || continue
        if "$LSOF" -nP -- "$_lock" >/dev/null 2>&1; then
            return 0
        fi
    done

    # 3. Recent activity: target/*/.fingerprint or target/*/deps modified within IDLE_MINUTES.
    #    Only check these two subdirs' own mtimes plus one level of children -- never walk the
    #    whole tree.
    for _sub in "$_target"/*/.fingerprint "$_target"/*/deps; do
        [ -d "$_sub" ] || continue
        _recent="$("$FIND" "$_sub" -mindepth 0 -maxdepth 1 -mmin -"$_idle_min" -print -quit 2>/dev/null)"
        if [ -n "$_recent" ]; then
            return 0
        fi
    done

    return 1
}

# ---------------------------------------------------------------------------
# 1a. Target directories: primary checkout + every worktree.
# ---------------------------------------------------------------------------

FREE_KB_START="$(free_kb "$REPO_ROOT")"
FREE_GB_START=$((FREE_KB_START / 1024 / 1024))

WIDE_IDLE_MIN=15
[ "$AGGRESSIVE" -eq 1 ] && WIDE_IDLE_MIN=15
if [ "$FREE_GB_START" -lt 30 ]; then
    WIDE_IDLE_MIN=15
fi
LOW_DISK=0
[ "$FREE_GB_START" -lt 30 ] && LOW_DISK=1

vlog "== target dirs =="

clean_target_for() {
    _root="$1"
    _label="$2"
    _idle_min="$3"

    [ -d "$_root/target" ] || return 0

    if target_is_busy "$_root" "$_idle_min"; then
        vlog "  keep target ($_label): busy or active within ${_idle_min}m -- $_root/target"
        return 0
    fi

    vlog "  eligible target ($_label), idle >= ${_idle_min}m: $_root/target"
    safe_rm "$_root/target" target
}

# Primary checkout: 6h idle normally, 15m if low-disk or --aggressive.
PRIMARY_IDLE_MIN=$((6 * 60))
if [ "$AGGRESSIVE" -eq 1 ] || [ "$LOW_DISK" -eq 1 ]; then
    PRIMARY_IDLE_MIN=$WIDE_IDLE_MIN
fi
clean_target_for "$REPO_ROOT" "primary" "$PRIMARY_IDLE_MIN"

# Every worktree: 2h idle normally, 15m if low-disk or --aggressive.
WT_IDLE_MIN=$((2 * 60))
if [ "$AGGRESSIVE" -eq 1 ] || [ "$LOW_DISK" -eq 1 ]; then
    WT_IDLE_MIN=$WIDE_IDLE_MIN
fi

WT_PORCELAIN="$("$GIT" -C "$REPO_ROOT" worktree list --porcelain 2>/dev/null)"

printf '%s\n' "$WT_PORCELAIN" | /usr/bin/awk '/^worktree /{print substr($0,10)}' | while IFS= read -r _wt; do
    [ "$_wt" = "$REPO_ROOT" ] && continue
    is_under "$_wt" "$WORKTREES_DIR" || continue
    clean_target_for "$_wt" "$(basename "$_wt")" "$WT_IDLE_MIN"
done

# ---------------------------------------------------------------------------
# 1b. Worktree removal.
# ---------------------------------------------------------------------------

vlog "== worktrees =="

process_worktree_entry() {
    _wt="$1"
    _locked="$2"
    _prunable="$3"
    _branch="$4"

    [ -z "$_wt" ] && return
    [ "$_wt" = "$REPO_ROOT" ] && return
    is_under "$_wt" "$WORKTREES_DIR" || return

    _name="$(basename "$_wt")"

    case "$_name" in
        odw-*)
            vlog "  keep worktree $_name: exempt by name (odw-*)"
            return
            ;;
        tm-integrate)
            vlog "  keep worktree $_name: exempt by name (long-lived integration worktree)"
            return
            ;;
    esac

    if [ "$_prunable" -eq 1 ]; then
        vlog "  keep worktree $_name: prunable (missing on disk), left for 'git worktree prune'"
        return
    fi

    if [ ! -d "$_wt" ]; then
        vlog "  keep worktree $_name: not present on disk"
        return
    fi

    if [ "$_locked" -eq 1 ]; then
        vlog "  keep worktree $_name: locked"
        return
    fi

    # Not busy: no process cwd anywhere under it (covers an agent still running there even
    # without a lock file, and covers a build in progress via the target-busy check).
    if any_process_cwd_under "$_wt"; then
        vlog "  keep worktree $_name: a process has its cwd inside it"
        return
    fi
    if [ -d "$_wt/target" ] && target_is_busy "$_wt" 15; then
        vlog "  keep worktree $_name: target busy"
        return
    fi

    # Clean: no uncommitted or untracked changes, ignoring target/ and node_modules/ (already
    # covered by the repo's own .gitignore in the normal case; --ignored=no plus explicit
    # status is used so a generated-but-gitignored dir never counts as "dirty").
    _status="$("$GIT" -C "$_wt" status --porcelain --ignore-submodules 2>/dev/null)"
    if [ -n "$_status" ]; then
        vlog "  keep worktree $_name: has uncommitted or untracked changes"
        return
    fi

    # HEAD contained in main or integrate, checked from the primary checkout.
    _head="$("$GIT" -C "$_wt" rev-parse HEAD 2>/dev/null)"
    if [ -z "$_head" ]; then
        vlog "  keep worktree $_name: could not resolve HEAD"
        return
    fi
    _in_main="$("$GIT" -C "$REPO_ROOT" branch --list main --contains "$_head" 2>/dev/null)"
    _in_integrate="$("$GIT" -C "$REPO_ROOT" branch --list integrate --contains "$_head" 2>/dev/null)"
    if [ -z "$_in_main" ] && [ -z "$_in_integrate" ]; then
        vlog "  keep worktree $_name: HEAD not contained in main or integrate"
        return
    fi

    vlog "  eligible worktree: $_name"
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "  WOULD REMOVE worktree: $_wt (branch: $_branch)"
    else
        if "$GIT" -C "$REPO_ROOT" worktree remove "$_wt" 2>>"$LOG_FILE"; then
            note_removed "worktree:$_name" 0
            if [ -n "$_branch" ]; then
                _branch_short="${_branch#refs/heads/}"
                if ! "$GIT" -C "$REPO_ROOT" branch -d "$_branch_short" >>"$LOG_FILE" 2>&1; then
                    vlog "  note: git branch -d $_branch_short failed (left in place)"
                fi
            fi
        else
            vlog "  note: git worktree remove failed for $_wt (left in place)"
        fi
    fi
}

# Parse the porcelain output into per-entry blocks (blank-line separated), tracking locked/
# prunable/branch fields, and dispatch each entry once its "worktree" line and any following
# attribute lines have all been read.
_wt_path=""
_wt_locked=0
_wt_prunable=0
_wt_branch=""
# Fed via a heredoc, not a pipe: a piped `while read` runs its body as the last stage of the
# pipeline, which is a subshell on this script's target shells, and this loop's whole point is
# accumulating _wt_path/_wt_locked/_wt_prunable/_wt_branch across lines in the *current* shell.
while IFS= read -r _line; do
    case "$_line" in
        worktree\ *)
            if [ -n "$_wt_path" ]; then
                process_worktree_entry "$_wt_path" "$_wt_locked" "$_wt_prunable" "$_wt_branch"
            fi
            _wt_path="${_line#worktree }"
            _wt_locked=0
            _wt_prunable=0
            _wt_branch=""
            ;;
        locked*) _wt_locked=1 ;;
        prunable*) _wt_prunable=1 ;;
        branch\ *) _wt_branch="${_line#branch }" ;;
        "")
            if [ -n "$_wt_path" ]; then
                process_worktree_entry "$_wt_path" "$_wt_locked" "$_wt_prunable" "$_wt_branch"
            fi
            _wt_path=""
            ;;
    esac
done <<EOF
$WT_PORCELAIN
EOF
if [ -n "$_wt_path" ]; then
    process_worktree_entry "$_wt_path" "$_wt_locked" "$_wt_prunable" "$_wt_branch"
fi

if [ "$DRY_RUN" -eq 1 ]; then
    vlog "  (dry-run: git worktree prune -n)"
    "$GIT" -C "$REPO_ROOT" worktree prune -n -v 2>>"$LOG_FILE" | while IFS= read -r _l; do
        vlog "  $_l"
    done
else
    "$GIT" -C "$REPO_ROOT" worktree prune >>"$LOG_FILE" 2>&1
fi

# ---------------------------------------------------------------------------
# 1c. Scratch dirs.
# ---------------------------------------------------------------------------

vlog "== scratch dirs =="

resolve_root() {
    _p="$1"
    [ -d "$_p" ] || return 1
    ( cd -P "$_p" 2>/dev/null && pwd -P )
}

TMPDIR_RESOLVED="$(resolve_root "${TMPDIR:-}")"
DARWIN_TMP="$(/usr/bin/getconf DARWIN_USER_TEMP_DIR 2>/dev/null)"
DARWIN_TMP_RESOLVED="$(resolve_root "$DARWIN_TMP")"
SLASH_TMP_RESOLVED="$(resolve_root /tmp)"

SCRATCH_ROOTS=""
for _r in "$TMPDIR_RESOLVED" "$DARWIN_TMP_RESOLVED" "$SLASH_TMP_RESOLVED"; do
    [ -z "$_r" ] && continue
    case " $SCRATCH_ROOTS " in
        *" $_r "*) : ;;
        *) SCRATCH_ROOTS="$SCRATCH_ROOTS $_r" ;;
    esac
done

# looks_like_tm_scratch DIR -- true if DIR contains a .tm dir, a projects/ dir, a `tm` binary,
# or a git repo whose only commits are from the last day.
looks_like_tm_scratch() {
    _d="$1"
    [ -d "$_d/.tm" ] && return 0
    [ -d "$_d/projects" ] && return 0
    [ -f "$_d/tm" ] && [ -x "$_d/tm" ] && return 0
    if [ -d "$_d/.git" ]; then
        _first_ct="$("$GIT" --git-dir="$_d/.git" log --reverse --format=%ct 2>/dev/null | /usr/bin/head -1)"
        if [ -n "$_first_ct" ]; then
            _now="$(/bin/date +%s)"
            _age=$((_now - _first_ct))
            if [ "$_age" -lt $((24 * 60 * 60)) ]; then
                return 0
            fi
        fi
    fi
    return 1
}

SCRATCH_IDLE_MIN=$((6 * 60))

scan_scratch_root_glob() {
    _root="$1"
    _name_glob="$2"
    [ -d "$_root" ] || return 0

    "$FIND" "$_root" -mindepth 1 -maxdepth 1 -type d -name "$_name_glob" -print 2>/dev/null | while IFS= read -r _d; do
        [ -L "$_d" ] && continue

        _mtime_recent="$("$FIND" "$_d" -maxdepth 0 -mmin -"$SCRATCH_IDLE_MIN" -print 2>/dev/null)"
        if [ -n "$_mtime_recent" ]; then
            vlog "  keep scratch: too recent -- $_d"
            continue
        fi

        if any_process_cwd_under "$_d"; then
            vlog "  keep scratch: a process has its cwd inside it -- $_d"
            continue
        fi

        if ! looks_like_tm_scratch "$_d"; then
            vlog "  keep scratch: does not look like tm scratch -- $_d"
            continue
        fi

        vlog "  eligible scratch: $_d"
        safe_rm "$_d" scratch
    done
}

for _root in $SCRATCH_ROOTS; do
    scan_scratch_root_glob "$_root" "tmp.*"
    scan_scratch_root_glob "$_root" "tm-trials"
    scan_scratch_root_glob "$_root" "tm-wide"
    scan_scratch_root_glob "$_root" "tm-accidental-*"
done

# ---------------------------------------------------------------------------
# Logging.
# ---------------------------------------------------------------------------

/bin/mkdir -p "$LOG_DIR"

FREE_KB_END="$(free_kb "$REPO_ROOT")"
FREE_GB_END_H="$(awk -v kb="$FREE_KB_END" 'BEGIN{printf "%.1f", kb/1024/1024}')"
FREE_GB_START_H="$(awk -v kb="$FREE_KB_START" 'BEGIN{printf "%.1f", kb/1024/1024}')"

TS="$(/bin/date -u +%Y-%m-%dT%H:%M:%SZ)"

if [ "$DRY_RUN" -eq 1 ]; then
    MODE_TAG="dry-run"
else
    MODE_TAG="run"
fi

SUMMARY_TRIMMED="$(/usr/bin/awk '{printf "%s%s", (NR==1?"":" "), $1}' "$SUMMARY_FILE")"
[ -z "$SUMMARY_TRIMMED" ] && SUMMARY_TRIMMED="(nothing removed)"

echo "$TS [$MODE_TAG] free_before=${FREE_GB_START_H}G free_after=${FREE_GB_END_H}G removed: $SUMMARY_TRIMMED" >>"$LOG_FILE"

exit 0
