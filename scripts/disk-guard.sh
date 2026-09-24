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
        "") ;; # an empty positional arg (e.g. a shell's unquoted-empty-default passthrough) is a no-op, not an error
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

# Never follow a symlink out of these two roots: resolve them once with `cd -P` up front so every
# later comparison (safe_rm's target/worktree shape checks, is_under) is against the real,
# symlink-free path rather than whatever REPO_ROOT/WORKTREES_DIR happen to say literally.
REPO_ROOT_RESOLVED="$(cd -P "$REPO_ROOT" 2>/dev/null && pwd -P)" || REPO_ROOT_RESOLVED="$REPO_ROOT"
WORKTREES_DIR_RESOLVED="$(cd -P "$WORKTREES_DIR" 2>/dev/null && pwd -P)" || WORKTREES_DIR_RESOLVED="$WORKTREES_DIR"
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
# EXIT alone does cleanup-only (a normal `exit 0` already reaches it); INT/TERM (e.g. launchd's
# `bootout` sending SIGTERM) additionally force a real exit, so a caught signal can't leave the
# script resuming mid-deletion after its trap handler returns.
trap 'rm -f "$SUMMARY_FILE"' EXIT
trap 'rm -f "$SUMMARY_FILE"; exit 1' INT TERM

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
            _parent_resolved="$(dirname "$_resolved")"
            case "$_resolved" in
                */target) : ;;
                *)
                    echo "disk-guard.sh: refusing non-target path for target deletion: '$_resolved'" >&2
                    return 1
                    ;;
            esac
            # The target's parent must be the primary checkout itself, or a direct child of
            # .claude/worktrees/ (a worktree's own root) -- never an arbitrary "*/target" match.
            if [ "$_parent_resolved" != "$REPO_ROOT_RESOLVED" ] && [ "$(dirname "$_parent_resolved")" != "$WORKTREES_DIR_RESOLVED" ]; then
                echo "disk-guard.sh: refusing target outside the primary checkout or a worktree: '$_resolved'" >&2
                return 1
            fi
            ;;
        worktree)
            if ! is_under "$_resolved" "$WORKTREES_DIR_RESOLVED"; then
                echo "disk-guard.sh: refusing worktree deletion outside .claude/worktrees: '$_resolved'" >&2
                return 1
            fi
            [ "$_resolved" = "$WORKTREES_DIR_RESOLVED" ] && return 1
            ;;
        scratch)
            _parent_resolved="$(dirname "$_resolved")"
            _base_resolved="$(basename "$_resolved")"
            case "$_base_resolved" in
                tmp.* | tm-*) : ;;
                *)
                    echo "disk-guard.sh: refusing scratch path with an unrecognized name: '$_resolved'" >&2
                    return 1
                    ;;
            esac
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

    if [ "$DRY_RUN" -eq 1 ]; then
        echo "  WOULD REMOVE $_kind: $_resolved"
    else
        # Sized only when actually removing -- `du -sk` walking a 10GB tree just to print a
        # number nobody reads would be real, pointless I/O on every dry-run.
        _before_kb="$(du_kb "$_resolved")"
        vlog "  removing $_kind: $_resolved"
        # rm's own stderr is discarded, not logged: if something starts writing into $_resolved
        # mid-delete (a build kicking off between our busy check and this rm), rm prints errors
        # for every such file, which would blow through the "one line per run" log contract.
        # A non-zero exit here just means the removal was partial -- recorded as such below,
        # not treated as a reason to abort the rest of this run.
        if "$RM" -rf -- "$_resolved" 2>/dev/null; then
            note_removed "$_kind:$(basename "$_resolved")" "$_before_kb"
        else
            vlog "  note: rm -rf reported errors removing $_resolved (possibly partial)"
            note_removed "$_kind:$(basename "$_resolved")(partial)" "$_before_kb"
        fi
    fi
    return 0
}

# ---------------------------------------------------------------------------
# One lsof snapshot for the whole run, cached so we don't spawn lsof once per checkout. `-d cwd`
# limits it to the cwd file descriptor; `+c 0` disables lsof's default 9-character command-name
# truncation (without it "rust-analyzer" comes back as "rust-anal", breaking the name match
# below); `-Fcn` emits a `c<command>` line immediately followed by that process's `n<path>` line.
# LSOF_CWD_LINES holds "command<TAB>path" pairs, one per process with a cwd; LSOF_CWDS holds just
# the paths, for the "any process at all" checks.
# ---------------------------------------------------------------------------

# capture_lsof_snapshot -- (re)populate LSOF_CWD_LINES/LSOF_CWDS. Called once up front, and again
# right before the worktree-removal and scratch-removal passes below: this run scans the primary
# checkout, every worktree's target, every worktree's removal eligibility, and every scratch-dir
# candidate (100+ in a real run) sequentially, so a single snapshot taken only at the very start
# could already be stale by the time a later pass evaluates something that became busy in the
# meantime. Re-snapshotting immediately before each deletion pass narrows, without eliminating,
# that window -- a real fix would mean a fresh lsof call per item, which is the cost this caching
# exists to avoid.
capture_lsof_snapshot() {
    LSOF_CWD_LINES="$("$LSOF" -nP -d cwd +c 0 -Fcn 2>/dev/null | /usr/bin/awk '
        /^c/ { cmd = substr($0, 2) }
        /^n/ { print cmd "\t" substr($0, 2) }
    ')"
    LSOF_CWDS="$(printf '%s\n' "$LSOF_CWD_LINES" | /usr/bin/awk -F'\t' '{print $2}')"
}

capture_lsof_snapshot

# Every busy/cwd check below depends entirely on this one snapshot. lsof can't legitimately come
# back empty -- this script's own shell process always has a cwd, so lsof would report at least
# one line for itself -- so an empty snapshot means lsof itself failed or was blocked (permissions,
# a hung lsof, an unexpected sandbox), and every "is anything busy here" check below would then
# silently read as "nothing is busy", which is exactly backwards for a script whose first rule is
# never deleting something in use. Refuse to delete anything this run rather than risk that.
if [ -z "$LSOF_CWDS" ]; then
    echo "disk-guard.sh: lsof returned no cwd data (a hung/blocked lsof, or nothing else went wrong but this shouldn't be possible) -- refusing to delete anything this run" >&2
    /bin/mkdir -p "$LOG_DIR"
    TS_ABORT="$(/bin/date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "$TS_ABORT [abort] lsof returned nothing; skipped" >>"$LOG_FILE"
    exit 0
fi

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

# is_build_process COMMAND_NAME -- true for cargo/rustc (matches spec 1a's "busy" definition for
# a target dir specifically -- an editor, shell or unrelated tool sitting in the checkout does not
# itself count). Deliberately excludes rust-analyzer*: this repo enables the rust-analyzer-lsp
# plugin project-wide, so a live Claude Code session's own language server has its cwd in the
# primary checkout (and in tm-integrate) essentially all the time -- confirmed on this machine, a
# session left open over a day still has rust-analyzer sitting there. Counting that as "busy"
# would pin the primary checkout's target/ -- the exact 31GB directory that caused the original
# incident -- permanently, defeating "target clears periodically". rust-analyzer's actual target/
# writes go through a `cargo check` child process, which the cwd match on "cargo" below already
# catches, and target/*/.cargo-lock / target/*/.fingerprint,deps mtime checks (2 and 3 in
# target_is_busy) catch its flychecks too -- that's enough to avoid deleting a target mid-flycheck
# without pinning it forever just because an editor session exists.
# Deliberately a plain top-level function, not an inline `case`:
# macOS ships bash 3.2 as both /bin/bash and /bin/sh, and 3.2 has a real, confirmed parser bug
# where a `case ... esac` written directly inside a `$( ... )` command substitution fails with
# "syntax error near unexpected token `newline'" even for entirely valid syntax; calling a
# function that itself happens to contain a `case` from inside a substitution does not trigger it
# (only the literal keyword sequence appearing inside the substitution's own text does), so every
# `case` this script needs inside a `$( ... )` goes through a named function like this one.
is_build_process() {
    case "$1" in
        cargo | rustc) return 0 ;;
        *) return 1 ;;
    esac
}

# ---------------------------------------------------------------------------
# target_is_busy CHECKOUT_ROOT IDLE_MINUTES -- true (busy) if a cargo/rustc process's cwd is
# under CHECKOUT_ROOT (excluding, for the primary checkout, any such cwd that is
# itself under WORKTREES_DIR -- the primary's path is a prefix of every worktree's path, so
# without this exclusion a build running in any worktree would pin the primary's target forever),
# or if target/*/.cargo-lock is held open, or if target/*/.fingerprint or target/*/deps changed
# within IDLE_MINUTES. Deliberately narrower than "any process with a cwd here": an editor, an
# MCP daemon, or a shell sitting in the checkout is not itself a reason to keep a target -- this
# repo's own Claude Code session and orchestrator routinely leave exactly those in the primary
# checkout and in tm-integrate, and counting them would pin the two largest targets permanently.
# Sets TARGET_BUSY_REASON (for the caller's log line) whenever it returns true.
# ---------------------------------------------------------------------------

target_is_busy() {
    _root="$1"
    _idle_min="$2"
    _target="$_root/target"
    TARGET_BUSY_REASON=""

    [ -d "$_target" ] || return 1

    # 1. A cargo/rustc process with its cwd inside this checkout (not just inside
    #    target/) -- a build about to write to target/ is reason enough to leave it alone. `$(...)`
    #    around the pipeline captures the matching command's own stdout line even though the
    #    `while` body is the pipeline's last (subshelled) stage -- command substitution reads the
    #    subshell's stdout, it doesn't need the subshell's variables.
    _hit_cmd="$(
        printf '%s\n' "$LSOF_CWD_LINES" | while IFS= read -r _line; do
            [ -z "$_line" ] && continue
            _cmd="${_line%%	*}"
            _cwd="${_line#*	}"
            is_build_process "$_cmd" || continue
            if [ "$_root" = "$REPO_ROOT" ] && is_under "$_cwd" "$WORKTREES_DIR"; then
                continue
            fi
            if is_under "$_cwd" "$_root"; then
                printf '%s\n' "$_cmd"
                break
            fi
        done
    )"
    if [ -n "$_hit_cmd" ]; then
        TARGET_BUSY_REASON="busy: cwd of $_hit_cmd"
        return 0
    fi

    # 2. Any process holding a target/*/.cargo-lock file open.
    for _lock in "$_target"/*/.cargo-lock; do
        [ -e "$_lock" ] || continue
        if "$LSOF" -nP -- "$_lock" >/dev/null 2>&1; then
            TARGET_BUSY_REASON="busy: .cargo-lock held ($_lock)"
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
            TARGET_BUSY_REASON="active: mtime within ${_idle_min}m ($_sub)"
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
        vlog "  keep target ($_label): $TARGET_BUSY_REASON -- $_root/target"
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
    # odw-* worktrees are read-only from this guard's perspective, same as worktree removal
    # below (owner decision -- ODW's own work-in-progress state, target/ included, stays untouched
    # until it lands; not just the worktree directory itself).
    case "$(basename "$_wt")" in
        odw-*) continue ;;
    esac
    clean_target_for "$_wt" "$(basename "$_wt")" "$WT_IDLE_MIN"
done

# ---------------------------------------------------------------------------
# 1b. Worktree removal.
# ---------------------------------------------------------------------------

vlog "== worktrees =="

# Re-snapshot right before this pass (see capture_lsof_snapshot's own comment) -- if this refresh
# comes back empty (a transient lsof hiccup), keep the previous, already-validated-non-empty
# snapshot rather than switching every busy check into "nothing is busy" for the rest of this run.
_LSOF_CWD_LINES_PREV="$LSOF_CWD_LINES"
_LSOF_CWDS_PREV="$LSOF_CWDS"
capture_lsof_snapshot
if [ -z "$LSOF_CWDS" ]; then
    vlog "  note: lsof refresh before worktree pass came back empty, keeping prior snapshot"
    LSOF_CWD_LINES="$_LSOF_CWD_LINES_PREV"
    LSOF_CWDS="$_LSOF_CWDS_PREV"
fi

# Minimum age, in minutes, a workflow-dispatched worktree gets before it's even eligible for
# removal, regardless of lock/clean/merged status. Lock status alone doesn't reliably signal "an
# agent is still working here" for this harness's own workflow worktrees (only session-owned
# worktrees created via EnterWorktree/agent sessions show a `locked` line at all), and a worktree
# freshly branched from main/integrate's tip is, by construction, HEAD-contained with an empty
# `git status` for as long as its agent hasn't made its first commit yet -- ordinary orientation
# time (reading files, planning) well under this guard's 30-minute cadence. This gate protects
# exactly that window.
WT_MIN_AGE_MIN=90

# worktree_too_young WORKTREE -- true if the worktree's own git admin dir (HEAD/index/logs/HEAD,
# under the common repo's `worktrees/<name>/`, per `git rev-parse --git-dir`) shows any activity
# within WT_MIN_AGE_MIN: creation, a commit, a checkout, or any other git operation. A worktree
# with no such recent activity is old enough that "unlocked + clean + merged" is trustworthy;
# one that's merely quiet because its agent is only reading/planning still counts as young.
worktree_too_young() {
    _wt="$1"
    _admin_dir="$("$GIT" -C "$_wt" rev-parse --git-dir 2>/dev/null)"
    [ -z "$_admin_dir" ] && return 1
    for _f in "$_admin_dir/HEAD" "$_admin_dir/index" "$_admin_dir/logs/HEAD"; do
        [ -e "$_f" ] || continue
        _hit="$("$FIND" "$_f" -mindepth 0 -maxdepth 0 -mmin -"$WT_MIN_AGE_MIN" -print -quit 2>/dev/null)"
        [ -n "$_hit" ] && return 0
    done
    return 1
}

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

    if worktree_too_young "$_wt"; then
        vlog "  keep worktree $_name: created or active within ${WT_MIN_AGE_MIN}m (age gate)"
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

    # Clean: no uncommitted or untracked changes. `--ignored` is passed explicitly (not the
    # default) so a gitignored-but-stateful file -- a real ticket-state `.tm/` dir, a
    # `*.db`/`*.db-wal`/`*.db-shm` sqlite file, a symlinked `.env`, `.claude/settings.local.json`,
    # a `.zvec-grep/` index -- is still visible here and still blocks removal, even though it
    # would never show up in a plain `git status --porcelain`; without `--ignored`, `git worktree
    # remove` deletes those along with the worktree even though the status check reported nothing
    # (verified empirically: a gitignored `.env.local` is invisible to plain `--porcelain`, and a
    # subsequent `git worktree remove` without --force deletes it anyway). Only `target/` and
    # `node_modules/` -- this repo's own known, regenerable build-output dirs -- are allow-listed
    # to keep counting as clean; every other ignored path keeps the worktree instead.
    _status="$("$GIT" -C "$_wt" status --porcelain --ignore-submodules --ignored 2>/dev/null)"
    _dirty=0
    while IFS= read -r _sline; do
        [ -z "$_sline" ] && continue
        case "$_sline" in
            "!! "*)
                _ipath="${_sline#\!\! }"
                case "$_ipath" in
                    target/ | target | */target/ | */target) : ;;
                    node_modules/ | node_modules | */node_modules/ | */node_modules) : ;;
                    *) _dirty=1 ;;
                esac
                ;;
            *) _dirty=1 ;;
        esac
    done <<EOF
$_status
EOF
    if [ "$_dirty" -eq 1 ]; then
        vlog "  keep worktree $_name: has uncommitted, untracked, or non-allow-listed ignored changes"
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
        if "$GIT" -C "$REPO_ROOT" worktree remove "$_wt" >/dev/null 2>&1; then
            if [ -n "$_branch" ]; then
                _branch_short="${_branch#refs/heads/}"
                if "$GIT" -C "$REPO_ROOT" branch -d "$_branch_short" >/dev/null 2>&1; then
                    note_removed "worktree:$_name" 0
                else
                    vlog "  note: git branch -d $_branch_short failed (left in place)"
                    note_removed "worktree:$_name(branch-kept)" 0
                fi
            else
                note_removed "worktree:$_name" 0
            fi
        else
            vlog "  note: git worktree remove failed for $_wt (left in place)"
            note_removed "worktree:$_name(remove-failed)" 0
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
    "$GIT" -C "$REPO_ROOT" worktree prune -n -v 2>/dev/null | while IFS= read -r _l; do
        vlog "  $_l"
    done
else
    "$GIT" -C "$REPO_ROOT" worktree prune >/dev/null 2>&1
fi

# ---------------------------------------------------------------------------
# 1c. Scratch dirs.
# ---------------------------------------------------------------------------

vlog "== scratch dirs =="

# Re-snapshot again right before this pass, same reasoning as the worktree pass above.
_LSOF_CWD_LINES_PREV="$LSOF_CWD_LINES"
_LSOF_CWDS_PREV="$LSOF_CWDS"
capture_lsof_snapshot
if [ -z "$LSOF_CWDS" ]; then
    vlog "  note: lsof refresh before scratch pass came back empty, keeping prior snapshot"
    LSOF_CWD_LINES="$_LSOF_CWD_LINES_PREV"
    LSOF_CWDS="$_LSOF_CWDS_PREV"
fi

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

# looks_like_tm_scratch DIR -- true if DIR contains a .tm dir, a projects/ dir, a `tm` binary, is
# itself directly shaped like a `.tm` state dir (project.db/index.db at its own root, not nested
# under .tm/ -- a real, confirmed shape: a stray dotm-rooted scratch dir from a past accidental-run
# incident), is itself a bare cargo `target/`-shaped dir (CACHEDIR.TAG plus .rustc_info.json at its
# own root -- a real, confirmed shape: a probe's own `cargo build` pointed straight at a
# tm-<name>-* scratch dir under /tmp rather than at a checkout's target/, so this guard's
# checkout-target cleanup above never sees it; it's still pure, regenerable build output, and the
# usual 6h-idle and cwd-busy gates below still apply before anything here is removed), or a git
# repo whose only commits are from the last day.
looks_like_tm_scratch() {
    _d="$1"
    [ -d "$_d/.tm" ] && return 0
    [ -d "$_d/projects" ] && return 0
    [ -f "$_d/tm" ] && [ -x "$_d/tm" ] && return 0
    [ -f "$_d/project.db" ] && return 0
    [ -f "$_d/index.db" ] && return 0
    [ -f "$_d/CACHEDIR.TAG" ] && [ -f "$_d/.rustc_info.json" ] && return 0
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

        # Signature check first (cheap, no tree walk): only a dir that already looks like tm
        # scratch is worth the recursive mtime walk below.
        if ! looks_like_tm_scratch "$_d"; then
            vlog "  keep scratch: does not look like tm scratch -- $_d"
            continue
        fi

        # Recency: a write anywhere under $_d counts, not just at its own top level -- a live
        # trial writing files deeper in the tree, with its own cwd elsewhere, must not look idle
        # just because $_d's own directory entry hasn't changed. -print -quit stops at the first
        # recent entry found, so this only walks the whole tree for a dir that's already a real
        # candidate for removal (one that passed the cheap signature check above).
        _mtime_recent="$("$FIND" "$_d" -mmin -"$SCRATCH_IDLE_MIN" -print -quit 2>/dev/null)"
        if [ -n "$_mtime_recent" ]; then
            vlog "  keep scratch: too recent -- $_d"
            continue
        fi

        if any_process_cwd_under "$_d"; then
            vlog "  keep scratch: a process has its cwd inside it -- $_d"
            continue
        fi

        vlog "  eligible scratch: $_d"
        safe_rm "$_d" scratch
    done
}

# tmp.* is scoped to $TMPDIR (and DARWIN_USER_TEMP_DIR, when it resolves to something else) --
# that's where `mktemp -d` actually creates them; tm-* is scoped to /tmp, per the spec's own paths
# (/tmp/tm-trials, /tmp/tm-wide, /tmp/tm-accidental-*) and every other ad hoc `tm-*`-prefixed
# scratch dir this workspace's own probes actually create there (tm-target-*, tm-live-*,
# tm-home-*, tm-audit-*, tm-provider-*, etc. -- a fixed three-name allowlist here left every one
# of those permanently invisible to this guard, confirmed on this machine: a 6.9GB
# /tmp/tm-target-* cargo target dir, over 30 hours idle, that a fixed-name scan never even visited).
# A generic glob is safe here because looks_like_tm_scratch's own signature check (not the name)
# is what actually gates removal -- broadening the name match doesn't broaden what gets deleted,
# just what gets looked at. This also keeps the two glob families from being run redundantly
# against roots they were never seen under.
_tmp_roots_seen=""
for _root in "$TMPDIR_RESOLVED" "$DARWIN_TMP_RESOLVED"; do
    [ -z "$_root" ] && continue
    case " $_tmp_roots_seen " in
        *" $_root "*) continue ;;
    esac
    _tmp_roots_seen="$_tmp_roots_seen $_root"
    scan_scratch_root_glob "$_root" "tmp.*"
done
if [ -n "$SLASH_TMP_RESOLVED" ]; then
    scan_scratch_root_glob "$SLASH_TMP_RESOLVED" "tm-*"
fi

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
FREED_KB_TOTAL="$(/usr/bin/awk '{sum += $2} END {print sum + 0}' "$SUMMARY_FILE")"
FREED_GB_H="$(awk -v kb="$FREED_KB_TOTAL" 'BEGIN{printf "%.1f", kb/1024/1024}')"

echo "$TS [$MODE_TAG] free_before=${FREE_GB_START_H}G free_after=${FREE_GB_END_H}G freed=${FREED_GB_H}G removed: $SUMMARY_TRIMMED" >>"$LOG_FILE"

exit 0
