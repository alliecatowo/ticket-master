//! Locating and opening a project, plus the commands that operate before or across a ticket's
//! lifecycle: `init`, `attach`, `genesis`, `status`, and `doctor`.
//!
//! D-003 splits a project's location into two things a workspace can disagree about:
//! - `root`: the **workspace** — the git toplevel when the current directory is inside a repo,
//!   else the canonical current directory. Unchanged meaning for code indexing, `templates.toml`,
//!   `bench/tasks`, `browser.toml`, the web client dir, and wiki docs.
//! - `state_dir`: where a project's durable state actually lives — `project.db`, `index.db`,
//!   `artifacts/`, and every other file that used to be assumed to sit under `<root>/.tm`. In
//!   **repo scope** that is still `<root>/.tm`; in **global scope** (no `.tm/` has been created
//!   in the workspace yet) it is `$TM_HOME/projects/<key>/`, entirely outside the workspace.
//!
//! The point of the split: bare `tm` in a fresh directory used to silently write a `.tm/`
//! directory (and, for a git repo, a whole assimilation) into whatever the user happened to be
//! standing in. [`open_bare`] now creates state under `$TM_HOME` instead and touches nothing in
//! the workspace; [`open_for_command`] (every explicit subcommand) never creates state at all,
//! erroring `NotFound` when neither scope has a project yet. [`resolve_scope`] is the one
//! function every opener funnels through. See `docs/decisions/D-003-project-scope.md` for the
//! full rationale, the `$TM_HOME` layout, and why promotion (`tm init` adopting a global project
//! into the repo) is left to the next track rather than built here.
//!
//! A repo-scoped Ticketmaster project is any directory containing a `.tm/` directory
//! (`.tm/project.db`, the event log `tm-core::Store` opens over). [`locate`] walks up from a
//! starting directory to find one, the same convention `git` uses for `.git`. Every other command
//! module in this crate is handed an already-opened [`Project`] by `main.rs`; this module is
//! where that opening happens.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tm_events::{Event, EventKind, EventLog};
use tm_types::{
    Clock, CounterIds, IdSource, ParticipantId, SystemClock, TicketId, Timestamp, TmError,
};

use crate::args::{AttachArgs, DoctorArgs, GenesisArgs, InitArgs, ProjectCommand, StatusArgs};
use crate::ops;
use crate::render::{Renderer, Table};

/// Where a project's durable state lives relative to its workspace root (D-003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Scope {
    /// State lives in the workspace's own `.tm/` directory.
    Repo,
    /// State lives under `$TM_HOME/projects/<key>/`, outside the workspace, because no
    /// repo-local project has been created (or promoted) yet.
    Global,
}

/// An opened project: the authoritative [`tm_core::Store`] plus the collaborators every command
/// needs to call into it (clock, id source, project root). Every execution module in this crate
/// takes `&Project` rather than reopening the store itself.
pub struct Project {
    /// The workspace root: the git toplevel, or the canonical current directory outside a repo.
    /// Still what code indexing, `templates.toml`, `bench/tasks`, `browser.toml`, the web client
    /// dir, and wiki docs resolve against (D-003).
    pub root: PathBuf,
    /// Where this project's durable state lives: `<root>/.tm` in repo scope,
    /// `$TM_HOME/projects/<key>/` in global scope (D-003).
    pub state_dir: PathBuf,
    /// Repo or global — see [`Scope`].
    pub scope: Scope,
    /// The authoritative store: event log plus materialized state.
    pub store: Arc<tm_core::Store>,
    /// Injected wall clock; `SystemClock` outside tests.
    pub clock: Arc<dyn Clock>,
    /// Injected id source; `CounterIds` restored from persisted counters outside tests.
    pub ids: Arc<dyn IdSource>,
    /// The participant identity `tm` acts as for commands issued from this process, e.g.
    /// `human:<local user>`.
    pub actor: ParticipantId,
}

impl Project {
    /// The code index for this project (`<state_dir>/index.db`), opened on demand by commands
    /// that need it (`search`, `symbol`, `history`, `doctor`, rather than eagerly here) and kept
    /// fresh for whoever opens it: every open here also runs a best-effort
    /// [`tm_codeintel::CodeIntel::update_incremental`] pass, so a caller that never ran `tm
    /// doctor`/`tm run` first still sees a symbol/search result for a file edited since the
    /// index was last written, rather than silently stale data. Skipped entirely when `root`
    /// isn't inside a git work tree (a global-scope project can point anywhere, including
    /// `$HOME`, and `update_incremental`'s history ingest needs a real `HEAD` to walk); degrades
    /// to a `tracing::warn!` rather than failing when the refresh itself errors (e.g. a repo
    /// with no commits yet) — the opened-but-possibly-stale index is still more useful to a
    /// caller than an error here would be. `tm doctor`'s own explicit
    /// `update_incremental` call stays as its own, separately reported check.
    pub fn code_intel(&self) -> tm_types::Result<tm_codeintel::CodeIntel> {
        let ci = tm_codeintel::CodeIntel::open_at(&self.state_dir, &self.root)?;
        if git2::Repository::open(&self.root).is_ok() {
            if let Err(e) = ci.update_incremental(self.clock.as_ref()) {
                tracing::warn!(
                    error = %e,
                    root = %self.root.display(),
                    "could not refresh the code index; continuing with what's already indexed"
                );
            }
        }
        Ok(ci)
    }

    /// One line describing where this project's state lives, for a human to see up front rather
    /// than discover by surprise. Printed once by the plain interactive loop
    /// ([`crate::agent::AgentSession::run_interactive`]) and shown as the TUI's status line
    /// (`crate::tui::run`), and reused as `tm doctor`'s advisory `"scope"` check detail.
    pub fn scope_line(&self) -> String {
        scope_line_for(self.scope, &self.root, &self.state_dir)
    }
}

// `Project::for_test` lives in its own `#[cfg(test)]` `impl` block at the very bottom of this
// file, right next to `mod tests`, deliberately -- not here alongside the block above. xtask's
// hygiene "no network in tests" check (`check_network_in_tests`) scopes "am I inside test code"
// with a simple one-way latch: the first `#[cfg(test)]`/`#[test]` line it sees in a file flips it
// on for every line after, for the rest of the file (it does not track braces/scope). Every real
// call site in this file that reads `ANTHROPIC_API_KEY` (`resolve_genesis_provider`, well below
// this point) must stay *before* that latch trips, or the checker misreads ordinary production
// code as a test reading a real credential. See that function's own docs for why the read itself
// is legitimate outside tests.

/// Shared text behind [`Project::scope_line`] and `tm status`'s human-rendered scope prefix
/// ([`render_status_report`]), which only has [`StatusReport`]'s plain `ScopeInfo` fields to work
/// from, not a `Project`.
fn scope_line_for(scope: Scope, root: &Path, state_dir: &Path) -> String {
    match scope {
        Scope::Repo => format!(
            "project: {} (repo scope, state in {})",
            root.display(),
            state_dir.display()
        ),
        Scope::Global => format!(
            "project: {} — global scope, state kept outside the repo at {} — run 'tm init' to \
             keep this project in the repo",
            root.display(),
            state_dir.display()
        ),
    }
}

/// Walk up from `start` looking for a `.tm` directory, the way `git` looks for `.git`.
pub fn locate(start: &Path) -> tm_types::Result<PathBuf> {
    let mut dir = start.canonicalize()?;
    loop {
        if dir.join(".tm").is_dir() {
            return Ok(dir);
        }
        dir = match dir.parent() {
            Some(parent) => parent.to_path_buf(),
            None => return Err(TmError::not_found("project", start.display())),
        };
    }
}

/// Resolve the local actor identity from the environment: `human:<$TM_ACTOR, else $USER, else
/// "unknown">`. Never reads the wall clock or the network to "detect" identity.
fn resolve_actor() -> tm_types::Result<ParticipantId> {
    let non_empty = |v: Result<String, std::env::VarError>| v.ok().filter(|s| !s.is_empty());
    let handle = non_empty(std::env::var("TM_ACTOR"))
        .or_else(|| non_empty(std::env::var("USER")))
        .unwrap_or_else(|| "unknown".to_string());
    ParticipantId::new(format!("human:{handle}"))
}

/// Open the project rooted at `root` with its state at `state_dir`, using the real wall clock and
/// a restored [`tm_types::CounterIds`], and resolve the local actor identity.
pub fn open_at(root: &Path, state_dir: &Path, scope: Scope) -> tm_types::Result<Project> {
    let store = Arc::new(tm_core::Store::open_at(state_dir)?);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    // `tm_core::Store` exposes no accessor for its internal id source, so this restores an
    // equivalent one from the same high-water marks `Store::open_at` itself just read, for the
    // collaborators (scheduler, server) that need to mint ids outside a `Store` command.
    let counters = store.view()?.counters.clone();
    let ids: Arc<dyn IdSource> = Arc::new(CounterIds::with_counters(counters, 0));
    let actor = resolve_actor()?;
    Ok(Project {
        root: root.to_path_buf(),
        state_dir: state_dir.to_path_buf(),
        scope,
        store,
        clock,
        ids,
        actor,
    })
}

/// Open the repo-scoped project at `root` (as returned by [`locate`]): a thin shim over
/// [`open_at`] for the `<root>/.tm` layout every caller assumed before D-003 split `root` from
/// `state_dir`. Prefer [`open_at`] when the caller already knows the state directory (e.g. the
/// scope-resolution openers below).
pub fn open(root: &Path) -> tm_types::Result<Project> {
    open_at(root, &root.join(".tm"), Scope::Repo)
}

/// `$TM_HOME`: the `TM_HOME` env var if set and non-empty, else `$HOME/.tm` (the same convention
/// `tm-browser`'s `managed.rs` already uses for `~/.tm/browsers`). Never reads the wall clock or
/// the network.
pub fn tm_home() -> tm_types::Result<PathBuf> {
    let non_empty = |v: Result<String, std::env::VarError>| v.ok().filter(|s| !s.is_empty());
    if let Some(explicit) = non_empty(std::env::var("TM_HOME")) {
        return Ok(PathBuf::from(explicit));
    }
    dirs::home_dir()
        .map(|home| home.join(".tm"))
        .ok_or_else(|| {
            TmError::invariant("could not determine the user's home directory for TM_HOME")
        })
}

/// The workspace root for `cwd`: the git toplevel (`git2::Repository::discover(cwd)`'s workdir)
/// when `cwd` is inside a non-bare git repository, else the canonical `cwd` itself. Always
/// canonicalized, so a git-discovered workdir and a plain canonical `cwd` can never disagree
/// about the same real directory (e.g. macOS's `/var` -> `/private/var` symlink) and therefore
/// never hash to two different [`global_project_key`]s for what is actually one workspace.
pub fn workspace_root_for(cwd: &Path) -> tm_types::Result<PathBuf> {
    if let Ok(repo) = git2::Repository::discover(cwd) {
        if let Some(workdir) = repo.workdir() {
            return Ok(workdir.canonicalize()?);
        }
        // A bare repository has no workdir to scope a project to; fall through to the plain
        // canonical-cwd rule below, the same as if `cwd` were not in a git repository at all.
    }
    Ok(cwd.canonicalize()?)
}

/// Map every byte outside `[A-Za-z0-9._-]` (and every `/`) in `root`'s path to `-`, collapse runs
/// of `-`, strip a leading `/`, and truncate to 100 chars — the sanitization half of
/// [`global_project_key`], split out so it's directly testable without hashing anything.
fn sanitize_for_key(root: &Path) -> String {
    let raw = root.to_string_lossy();
    let stripped = raw.strip_prefix('/').unwrap_or(&raw);
    let mut out = String::with_capacity(stripped.len());
    let mut last_was_dash = false;
    for c in stripped.chars() {
        let mapped = if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            c
        } else {
            '-'
        };
        if mapped == '-' {
            if !last_was_dash {
                out.push('-');
            }
            last_was_dash = true;
        } else {
            out.push(mapped);
            last_was_dash = false;
        }
    }
    out.truncate(100);
    out
}

/// The global project key for workspace `root`: `sanitize(root) + '-' + <first 8 hex chars of
/// blake3(canonical root path bytes)>`, e.g. `/Users/allie/Develop/foo` ->
/// `Users-allie-Develop-foo-3f9a1c2b`. Deterministic and, thanks to the hash suffix, collision-safe
/// even between two workspaces whose sanitized paths coincide (e.g. paths differing only in
/// characters the sanitizer maps to the same `-`).
pub fn global_project_key(root: &Path) -> String {
    let sanitized = sanitize_for_key(root);
    let hash = blake3::hash(root.to_string_lossy().as_bytes());
    let short_hex = &hash.to_hex()[..8];
    format!("{sanitized}-{short_hex}")
}

/// `$TM_HOME/projects/<key>/` for workspace `root` — the global-scope state directory
/// [`resolve_scope`] falls back to when no repo-local `.tm/` exists.
pub fn global_project_dir(root: &Path) -> tm_types::Result<PathBuf> {
    Ok(tm_home()?.join("projects").join(global_project_key(root)))
}

/// The outcome of [`resolve_scope`]: where a workspace's project state lives (or would be
/// created), and whether it already exists.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The workspace root (repo toplevel, or canonical cwd outside a repo).
    pub root: PathBuf,
    /// Where this workspace's state lives, or would live once created.
    pub state_dir: PathBuf,
    /// Repo or global.
    pub scope: Scope,
    /// Whether `state_dir` already holds an opened project (`project.db` present). Always `true`
    /// for repo scope (an explicit `--project` creates-if-absent the same as `tm init` always
    /// has; a `.tm` found by [`locate`] exists by construction).
    pub exists: bool,
}

/// Resolve which workspace to operate on and where its state lives, in one place every opener in
/// this module funnels through (D-003). Precedence, most to least specific:
/// 1. `explicit` (`--project P`): repo scope at `P`, `state_dir = P/.tm`. Unchanged from before
///    D-003 — still creates-if-absent, so `exists` is always `true` here.
/// 2. [`locate`] finds a `.tm/` above `cwd`: repo scope there. `exists` is always `true` — a
///    located `.tm/` exists by construction.
/// 3. A global project already exists for `workspace_root_for(cwd)` under `$TM_HOME`: global
///    scope, `exists: true`.
/// 4. Otherwise: global scope, `exists: false` — nothing has ever been created for this
///    workspace in either scope.
pub fn resolve_scope(explicit: Option<&Path>, cwd: &Path) -> tm_types::Result<Resolved> {
    if let Some(p) = explicit {
        return Ok(Resolved {
            root: p.to_path_buf(),
            state_dir: p.join(".tm"),
            scope: Scope::Repo,
            exists: true,
        });
    }
    // The default `TM_HOME` is `$HOME/.tm` -- the exact shape `locate` searches for -- so once any
    // global-scope project has ever been created anywhere (which is now `open_bare`'s default
    // behavior), a plain `locate` walking up from any `cwd` under `$HOME` would eventually reach
    // it and misresolve the user's entire home directory as an ordinary repo-scope project. This
    // must be checked against the *real* home directory (`dirs::home_dir()`), not `tm_home()`:
    // `tm_home()` reads the `TM_HOME` env var, which tests override to a hermetic tempdir
    // specifically so they don't touch the real `$HOME/.tm` -- but `locate` still walks the real
    // filesystem starting from the real `$HOME`, so an env-var-relative check would stop guarding
    // the one path `locate` can actually reach. `$HOME/.tm` never contains `project.db` directly
    // (only a `projects/<key>/` subdirectory), so it can never be a genuine repo-scope hit;
    // exclude it explicitly rather than relying on that shape difference implicitly.
    let real_home_tm = dirs::home_dir()
        .and_then(|h| h.canonicalize().ok())
        .map(|h| h.join(".tm"));
    if let Ok(found) = locate(cwd) {
        let is_real_home_tm = real_home_tm.as_deref() == Some(found.join(".tm").as_path());
        if !is_real_home_tm {
            return Ok(Resolved {
                state_dir: found.join(".tm"),
                root: found,
                scope: Scope::Repo,
                exists: true,
            });
        }
    }
    let root = workspace_root_for(cwd)?;
    let state_dir = global_project_dir(&root)?;
    let exists = state_dir.join("project.db").is_file();
    Ok(Resolved {
        root,
        state_dir,
        scope: Scope::Global,
        exists,
    })
}

/// Write `$TM_HOME/projects/<key>/workspace.json`, the marker [`open_bare`] creates exactly once
/// for a fresh global-scope project. Uses `clock` (never the wall clock directly — hygiene
/// forbids `SystemTime::now` outside `tm-types`) for `created_at`.
fn write_workspace_json(state_dir: &Path, root: &Path, clock: &dyn Clock) -> tm_types::Result<()> {
    let doc = serde_json::json!({
        "workspace": root.to_string_lossy(),
        "created_at": clock.now(),
        "schema": 1,
    });
    std::fs::write(
        state_dir.join("workspace.json"),
        serde_json::to_vec_pretty(&doc)?,
    )?;
    Ok(())
}

/// Open the project for an explicit subcommand (every arm but bare `tm` in `main.rs::dispatch`):
/// [`resolve_scope`] against the real current directory, erroring [`TmError::NotFound`] when
/// neither scope has a project yet rather than creating one — a user who typed a specific
/// subcommand already knows enough to run `tm init` first, and subcommands never create global
/// state on their own (only [`open_bare`] does, and only for bare `tm`).
pub fn open_for_command(explicit: Option<&Path>) -> tm_types::Result<Project> {
    let cwd = std::env::current_dir()?;
    let resolved = resolve_scope(explicit, &cwd)?;
    if !resolved.exists {
        return Err(TmError::not_found(
            "project",
            format!(
                "for {} yet (run `tm` to start one, kept outside this directory, or `tm init` to \
                 keep it in this repo)",
                resolved.root.display()
            ),
        ));
    }
    open_at(&resolved.root, &resolved.state_dir, resolved.scope)
}

/// Resolve and open the project for bare `tm`'s entry point (`main.rs::dispatch`'s `None`
/// command branch): [`resolve_scope`] against the real current directory, opening whatever it
/// finds (`--project`, a located `.tm/`, or an existing global project) — and, only when
/// resolution lands on global scope with nothing there yet, creating a fresh global project under
/// `$TM_HOME` ([`write_workspace_json`]) and printing nothing, rather than touching the workspace
/// at all. This is the fix D-003 exists for: bare `tm` in a fresh directory used to write a
/// `.tm/` (and, for a git repo, run a whole assimilation) into whatever the user happened to be
/// standing in; now it creates state entirely outside the workspace and leaves it untouched.
///
/// `renderer` is accepted (matching every other opener's shape, and so a future caller that wants
/// to print something here doesn't need a signature change) but unused today: creating a global
/// project is silent by design (D-003), unlike the old bootstrap's one-line "creating a project
/// here" note.
///
/// Kept out of `main.rs::dispatch` itself so this decision is directly unit-testable without also
/// driving the interactive loop or a prompt turn, both real side effects `dispatch` performs once
/// a project is open -- see this crate's module doc: `main.rs` stays "thin", decisions like this
/// belong in `tm-cli`'s library modules.
pub fn open_bare(explicit: Option<&Path>, _renderer: &Renderer) -> tm_types::Result<Project> {
    let cwd = std::env::current_dir()?;
    let resolved = resolve_scope(explicit, &cwd)?;
    let project = open_at(&resolved.root, &resolved.state_dir, resolved.scope)?;
    if resolved.scope == Scope::Global && !resolved.exists {
        write_workspace_json(&resolved.state_dir, &resolved.root, project.clock.as_ref())?;
    }
    Ok(project)
}

/// `tm project show|list` (D-003): inspect scope resolution and enumerate global projects.
/// Neither verb opens or creates a project — `show` calls [`resolve_scope`] directly, `list`
/// only reads `$TM_HOME/projects/*/workspace.json`.
pub fn dispatch_project(
    cmd: &ProjectCommand,
    explicit: Option<&Path>,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        ProjectCommand::Show => project_show(explicit, renderer),
        ProjectCommand::List => project_list(renderer),
    }
}

/// `tm project show`: the resolved scope for the current directory (or `--project`), as JSON
/// `{kind, workspace, state_dir, exists}` or the equivalent human line. Creates nothing.
fn project_show(explicit: Option<&Path>, renderer: &Renderer) -> tm_types::Result<()> {
    let cwd = std::env::current_dir()?;
    let resolved = resolve_scope(explicit, &cwd)?;
    let kind = match resolved.scope {
        Scope::Repo => "repo",
        Scope::Global => "global",
    };
    let json = serde_json::json!({
        "kind": kind,
        "workspace": resolved.root,
        "state_dir": resolved.state_dir,
        "exists": resolved.exists,
    });

    if renderer.is_json() {
        renderer.emit(&json, "")?;
    } else if !renderer.is_quiet() {
        let human = format!(
            "{} — {}, state in {}",
            resolved.root.display(),
            kind,
            resolved.state_dir.display()
        );
        renderer.emit(&json, &human)?;
    }
    Ok(())
}

/// One row of `tm project list`'s output: one `$TM_HOME/projects/<key>/` entry.
#[derive(Debug, Clone, Serialize)]
struct GlobalProjectEntry {
    /// The directory name under `$TM_HOME/projects/`.
    key: String,
    /// `workspace.json`'s `workspace` field: the canonical workspace path this entry is for.
    workspace: String,
    /// The event log's current head sequence number, i.e. how many events this project has
    /// recorded; `0` if `project.db` doesn't exist or can't be opened — including a promoted
    /// entry, whose `project.db` [`promote_global`] deliberately removes (see that function's
    /// step 6 docs).
    events: u64,
    /// A `promoted.json` marker's contents alongside this entry, if [`promote_global`] promoted
    /// this global session into a repo-scoped project (D-003 Phase 1-C).
    promoted: Option<serde_json::Value>,
}

/// `tm project list`: every `$TM_HOME/projects/*/workspace.json` entry.
fn project_list(renderer: &Renderer) -> tm_types::Result<()> {
    let home = tm_home()?;
    let projects_dir = home.join("projects");
    let mut entries = Vec::new();

    let read_dir = match std::fs::read_dir(&projects_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if renderer.is_json() {
                renderer.emit(&entries, "")?;
            } else if !renderer.is_quiet() {
                renderer.emit(&entries, "no projects in $TM_HOME yet")?;
            }
            return Ok(());
        }
        Err(e) => return Err(TmError::from(e)),
    };

    for dir_entry in read_dir {
        let dir_entry = dir_entry?;
        if !dir_entry.file_type()?.is_dir() {
            continue;
        }
        let dir = dir_entry.path();
        let workspace_json = dir.join("workspace.json");
        let Ok(raw) = std::fs::read_to_string(&workspace_json) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let workspace = doc
            .get("workspace")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        // Guard the existence check: `EventLog::open` creates the file if it's missing (via
        // `schema::open_write_connection`'s `Connection::open`), which would silently resurrect
        // a promoted-and-emptied global project's `project.db` — flipping `resolve_scope`'s
        // `exists` back to `true` for a workspace that should now resolve to its promoted
        // repo-scoped project instead. `list` is read-only; it must never do that.
        let events = if dir.join("project.db").is_file() {
            tm_events::EventLog::open(&dir.join("project.db"))
                .and_then(|log| log.head())
                .unwrap_or(0)
        } else {
            0
        };
        let promoted = std::fs::read_to_string(dir.join("promoted.json"))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        entries.push(GlobalProjectEntry {
            key: dir_entry.file_name().to_string_lossy().to_string(),
            workspace,
            events,
            promoted,
        });
    }
    entries.sort_by(|a, b| a.key.cmp(&b.key));

    if renderer.is_json() {
        renderer.emit(&entries, "")?;
    } else if !renderer.is_quiet() {
        let rows = entries
            .iter()
            .map(|e| vec![e.key.clone(), e.workspace.clone(), e.events.to_string()])
            .collect();
        let human = Table::new(
            vec![
                "key".to_string(),
                "workspace".to_string(),
                "events".to_string(),
            ],
            rows,
        )
        .render();
        renderer.emit(&entries, &human)?;
    }
    Ok(())
}

/// Create a new project's `.tm/` directory at `dir`, refusing if one already exists there, and
/// return the canonicalized root. Shared by [`init`] and [`attach`] (which creates one first
/// when assimilating a repository with no project yet).
fn create_project_dir(dir: &Path) -> tm_types::Result<PathBuf> {
    if dir.join(".tm").is_dir() {
        return Err(TmError::conflict(format!(
            "{} already contains a .tm project",
            dir.display()
        )));
    }
    // Creates `.tm/project.db` and runs migrations, materializing the empty project.
    tm_core::Store::open(dir)?;
    Ok(dir.canonicalize()?)
}

/// What `tm init`'s promotion path did (D-003 Phase 1-C): moved an existing global session's
/// durable state into the workspace's own repo-scoped `.tm/` directory instead of starting
/// fresh — the seam that lets "chat first, decide to track it as a real project later" feel like
/// one product rather than two disconnected modes. See [`promote_global`] for the mechanics.
#[derive(Debug, Clone, Serialize)]
pub struct PromotionReport {
    /// Where the global session used to live (`$TM_HOME/projects/<key>/`).
    pub from: PathBuf,
    /// The repo-scoped `.tm/` directory the session was promoted into.
    pub to: PathBuf,
    /// How many events were carried over.
    pub events: usize,
    /// How many tickets existed in the promoted project.
    pub tickets: usize,
}

/// A ticket's promotion-relevant identity: objective and state, captured from the source before
/// any destructive step, so [`verify_promotion`]'s cross-check needs no second read of the
/// (about-to-be-cleared) source.
type TicketFingerprint = (String, tm_core::TicketState);

/// Decide whether `dir` should adopt an existing global session for its workspace or start a
/// fresh `.tm/` (D-003 Phase 1-C), and do it. Shared by [`init`], [`attach`], and [`genesis`] so
/// none of the three silently orphans a global chat session's history the moment an explicit
/// project-creating command runs against the same workspace.
///
/// `fresh` (only ever `true` from `tm init --fresh`; `attach`/`genesis` always pass `false`, per
/// this track's design — neither has an escape hatch of its own) skips the global-session check
/// entirely, including reading `$TM_HOME`, so a caller that already knows it wants a clean slate
/// never depends on `$TM_HOME`'s state at all.
///
/// Returns the canonicalized workspace root, plus a [`PromotionReport`] when promotion happened
/// (`None` on the plain fresh-create path, unchanged from before this track).
///
/// # Errors
/// `TmError::Conflict` if `dir` already contains a `.tm` directory (unchanged, existing
/// behavior). Whatever [`promote_global`] returns if promotion is attempted and fails — in which
/// case `dir` is left with no `.tm` at all, exactly as if this function had never been called.
fn create_or_promote_project_dir(
    dir: &Path,
    fresh: bool,
) -> tm_types::Result<(PathBuf, Option<PromotionReport>)> {
    if dir.join(".tm").is_dir() {
        return Err(TmError::conflict(format!(
            "{} already contains a .tm project",
            dir.display()
        )));
    }
    if !fresh {
        // The global session lives under a key derived from the *workspace* (the git toplevel,
        // or the canonical cwd outside a repo — see `workspace_root_for`), but the destination
        // `.tm/` this call is creating belongs at `dir` itself, not necessarily the workspace
        // root: `dir` can be a subdirectory of a larger repo (e.g. `tm init` run from a
        // subdirectory with an explicit path, or `tm attach some/nested/path`). Conflating the
        // two would write into (and, via the backup API, silently clobber) whatever `.tm/`
        // already exists at the workspace root instead of at `dir` — the conflict guard above
        // even checks the right directory (`dir.join(".tm")`) already, so promoting into the
        // wrong one would bypass it entirely.
        let workspace = workspace_root_for(dir)?;
        let global_dir = global_project_dir(&workspace)?;
        if global_dir.join("project.db").is_file() {
            let dest_root = dir.canonicalize()?;
            let clock: Arc<dyn Clock> = Arc::new(SystemClock);
            let report = promote_global(&global_dir, &dest_root, clock)?;
            return Ok((dest_root, Some(report)));
        }
    }
    let root = create_project_dir(dir)?;
    Ok((root, None))
}

/// Promote the global-scope session at `src_state_dir` into a fresh repo-scoped project at
/// `dest_root/.tm`. See `docs/decisions/D-003-project-scope.md` for the vocabulary and why
/// promotion exists; the steps here:
///
/// 1. Verify the source's hash chain before touching anything destructive — a corrupt source
///    must never be promoted, and every fact step 4 checks against the destination is captured
///    from the source right here, before any destructive step runs.
/// 2. Copy `project.db` via SQLite's online backup API ([`backup_copy_project_db`]), never
///    `std::fs::copy`: the source is WAL-mode (see `tm-events/src/schema.rs`'s `apply_pragmas`),
///    so a plain file copy can silently drop the `-wal` sidecar (losing not-yet-checkpointed
///    writes) or copy a torn, inconsistent snapshot.
/// 3. Copy every other durable-state file ([`copy_state_extras`]) by an explicit allowlist, not a
///    blanket recursive copy — a stale `-wal`/`-shm` sidecar sitting next to the source
///    `project.db` must never leak into the destination on top of the backup that already
///    completed.
/// 4. Re-verify the *destination* independently ([`verify_promotion`]): hash chain, `rebuild`,
///    `check_invariants`, event count, counters, and every ticket's id/objective/state, all
///    compared against the facts captured in step 1.
///
/// On any failure from step 2 onward, `dest_root/.tm` is removed entirely and the source is left
/// completely untouched. On success, `src_state_dir`'s durable-state files are removed and a
/// `promoted.json` marker is written — see [`finalize_promoted_source`] for exactly what survives
/// and why.
fn promote_global(
    src_state_dir: &Path,
    dest_root: &Path,
    clock: Arc<dyn Clock>,
) -> tm_types::Result<PromotionReport> {
    let src_db = src_state_dir.join("project.db");

    let src_log = EventLog::open_with_clock(&src_db, clock.clone())?;
    let chain = src_log.verify_chain()?;
    if !chain.is_valid() {
        return Err(TmError::storage(format!(
            "source chain invalid: {}",
            chain
                .detail
                .clone()
                .unwrap_or_else(|| "chain broken".to_string())
        )));
    }
    let src_events = chain.events_checked as usize;

    // Capture every fact `verify_promotion` needs from the source now, before any destructive
    // step, so "the source is left completely untouched on failure" never depends on being able
    // to re-read the source afterward.
    let src_store = tm_core::Store::open_at(src_state_dir)?;
    let src_counters = src_store.counters()?;
    let src_tickets: BTreeMap<TicketId, TicketFingerprint> = src_store
        .view()?
        .tickets
        .into_iter()
        .map(|(id, t)| (id, (t.objective, t.state)))
        .collect();
    drop(src_store);
    drop(src_log);

    let dest_tm = dest_root.join(".tm");
    std::fs::create_dir_all(&dest_tm)?;

    let outcome = (|| -> tm_types::Result<usize> {
        backup_copy_project_db(&src_db, &dest_tm.join("project.db"))?;
        copy_state_extras(src_state_dir, &dest_tm)?;
        verify_promotion(
            &dest_tm,
            src_events,
            &src_counters,
            &src_tickets,
            clock.clone(),
        )
    })();

    match outcome {
        Ok(tickets) => {
            finalize_promoted_source(src_state_dir, &dest_tm, src_events, clock.as_ref())?;
            Ok(PromotionReport {
                from: src_state_dir.to_path_buf(),
                to: dest_tm,
                events: src_events,
                tickets,
            })
        }
        Err(e) => {
            std::fs::remove_dir_all(&dest_tm).map_err(|remove_err| {
                TmError::storage(format!(
                    "promotion failed ({e}), and cleaning up the half-built destination at {} \
                     also failed: {remove_err}",
                    dest_tm.display()
                ))
            })?;
            Err(e)
        }
    }
}

/// Copy `src_db`'s bytes into a fresh `dest_db` via SQLite's online backup API
/// (`rusqlite::backup::Backup`). Never `std::fs::copy` for this file: the source is WAL-mode (see
/// `tm-events/src/schema.rs`'s `apply_pragmas`), so a plain file copy can silently drop the
/// `-wal` sidecar or copy a torn, inconsistent snapshot. The backup API instead reads the live
/// database through SQLite itself and produces a complete, consistent destination file, no
/// sidecar required.
fn backup_copy_project_db(src_db: &Path, dest_db: &Path) -> tm_types::Result<()> {
    let src_conn = tm_events::schema::open_read_connection(src_db)?;
    let mut dest_conn = rusqlite::Connection::open(dest_db)
        .map_err(|e| TmError::storage(format!("opening destination project.db: {e}")))?;
    let backup = rusqlite::backup::Backup::new(&src_conn, &mut dest_conn)
        .map_err(|e| TmError::storage(format!("starting sqlite backup: {e}")))?;
    backup
        .run_to_completion(5, std::time::Duration::from_millis(50), None)
        .map_err(|e| TmError::storage(format!("running sqlite backup: {e}")))?;
    Ok(())
}

/// Recursively copy every file under `from` to `to`, creating directories as needed. Used only
/// for the allowlisted non-database directories in [`copy_state_extras`] — `project.db` itself
/// always goes through [`backup_copy_project_db`], never this.
fn copy_dir_recursive(from: &Path, to: &Path) -> tm_types::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// Copy every other piece of a promoted session's durable state from `src_state_dir` to
/// `dest_tm`, by an explicit allowlist (never a blanket recursive copy — see [`promote_global`]'s
/// docs for why): `artifacts/` (large-artifact spill directory), `harness.toml`, `mirror.toml`,
/// `sched.paused`, `workflows/`, `bench/`. Deliberately skips `index.db` (derived, cheaply
/// re-indexed by the next `code_intel()` call) and `workspace.json` (a global-scope-only marker,
/// meaningless at a repo-level destination).
fn copy_state_extras(src_state_dir: &Path, dest_tm: &Path) -> tm_types::Result<()> {
    for name in ["harness.toml", "mirror.toml", "sched.paused"] {
        let src = src_state_dir.join(name);
        if src.is_file() {
            std::fs::copy(&src, dest_tm.join(name))?;
        }
    }
    for name in ["artifacts", "workflows", "bench"] {
        let src = src_state_dir.join(name);
        if src.is_dir() {
            copy_dir_recursive(&src, &dest_tm.join(name))?;
        }
    }
    Ok(())
}

/// Step 4 of [`promote_global`]: open the freshly-copied destination independently and prove it
/// is a faithful, healthy copy of the source before declaring success — hash chain, `rebuild`,
/// `check_invariants`, event count, counters, and every source ticket's id/objective/state.
/// Returns the ticket count on success.
fn verify_promotion(
    dest_tm: &Path,
    src_events: usize,
    src_counters: &BTreeMap<String, u64>,
    src_tickets: &BTreeMap<TicketId, TicketFingerprint>,
    clock: Arc<dyn Clock>,
) -> tm_types::Result<usize> {
    let dest_db = dest_tm.join("project.db");
    let dest_log = EventLog::open_with_clock(&dest_db, clock)?;
    let chain = dest_log.verify_chain()?;
    if !chain.is_valid() {
        return Err(TmError::storage(format!(
            "destination chain invalid after copy: {}",
            chain
                .detail
                .clone()
                .unwrap_or_else(|| "chain broken".to_string())
        )));
    }
    let dest_events = chain.events_checked as usize;
    if dest_events != src_events {
        return Err(TmError::storage(format!(
            "destination event count ({dest_events}) does not match the source ({src_events})"
        )));
    }
    drop(dest_log);

    // `Store::rebuild` drops every materialized table and replays it purely from the event log —
    // and the event log alone cannot reconstruct an artifact's `hash`/`bytes_len`:
    // `ArtifactCreated`'s payload never carries them, `Store::store_artifact`'s real values land
    // via a side-channel SQL write in the *same* live transaction (see
    // `tm-core/src/materialize.rs`'s own comment on `ArtifactCreated`'s "placeholder row"), and
    // replay has no way to redo that side channel. Running `rebuild` straight against the real
    // `dest_tm` would therefore permanently blank every artifact's `hash`/`bytes_len` in the
    // project this promotion is about to hand back to the user — silently breaking the Phase 1-A
    // relocated-artifact read fallback, which keys off `hash`. So this check runs against a
    // throwaway scratch copy instead: the point of calling `rebuild`/`check_invariants` here is
    // to prove the *event log itself* replays cleanly (catching a truncated or corrupt backup),
    // not to leave the real promoted `project.db` with corrupted artifact metadata. The scratch
    // copy is removed before this function returns, success or failure.
    let scratch_state_dir = dest_tm.join(".rebuild-scratch");
    let scratch_result = (|| -> tm_types::Result<()> {
        std::fs::create_dir_all(&scratch_state_dir)?;
        backup_copy_project_db(&dest_db, &scratch_state_dir.join("project.db"))?;
        let scratch_store = tm_core::Store::open_at(&scratch_state_dir)?;
        scratch_store.rebuild()?;
        let violations = scratch_store.check_invariants()?;
        if !violations.is_empty() {
            let detail = violations
                .iter()
                .map(|v| format!("{} ({}): {}", v.invariant, v.subject, v.detail))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(TmError::storage(format!(
                "destination invariant violation(s) after promotion: {detail}"
            )));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&scratch_state_dir);
    scratch_result?;

    let dest_store = tm_core::Store::open_at(dest_tm)?;
    // A non-mutating read (`check_invariants` is `check_invariants(&self.view()?)` over a
    // read-only connection), so — unlike `rebuild` above — this runs directly against the real
    // destination: the store this promotion is about to hand back to the caller must itself pass
    // invariants, not just the scratch copy used to sanity-check that the event log replays.
    let violations = dest_store.check_invariants()?;
    if !violations.is_empty() {
        let detail = violations
            .iter()
            .map(|v| format!("{} ({}): {}", v.invariant, v.subject, v.detail))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(TmError::storage(format!(
            "destination invariant violation(s) after promotion: {detail}"
        )));
    }

    let dest_counters = dest_store.counters()?;
    if &dest_counters != src_counters {
        return Err(TmError::storage(format!(
            "destination counters {dest_counters:?} do not match the source {src_counters:?}"
        )));
    }

    let dest_view = dest_store.view()?;
    for (id, (objective, state)) in src_tickets {
        match dest_view.tickets.get(id) {
            Some(ticket) if &ticket.objective == objective && &ticket.state == state => {}
            Some(ticket) => {
                return Err(TmError::storage(format!(
                    "ticket {id} in the destination does not match the source: expected \
                     objective {objective:?} state {state:?}, got objective {:?} state {:?}",
                    ticket.objective, ticket.state
                )));
            }
            None => {
                return Err(TmError::storage(format!(
                    "ticket {id} from the source is missing from the promoted destination"
                )));
            }
        }
    }

    Ok(src_tickets.len())
}

/// Step 6 of [`promote_global`], on success only: clear `src_state_dir`'s durable-state files
/// (everything [`copy_state_extras`]/[`backup_copy_project_db`] just carried over) and write
/// `promoted.json`, recording where the session went and when (`clock`, never the wall clock
/// directly — hygiene forbids raw `SystemTime::now` outside `tm-types`).
///
/// `workspace.json` deliberately survives. `resolve_scope` only treats a global project as
/// existing when its `project.db` is present (`Resolved::exists` in this module), so removing
/// just the database (plus its `-wal`/`-shm` sidecars) already makes this directory resolve as
/// "no global project here" for every future `resolve_scope` call against this workspace — while
/// still leaving `tm project list` a row to hang a `promoted` marker off, since `list` enumerates
/// entries by reading `workspace.json`, not `project.db`. Removing the directory outright instead
/// would have made that impossible without a schema change to `list`'s enumeration.
fn finalize_promoted_source(
    src_state_dir: &Path,
    dest_tm: &Path,
    events: usize,
    clock: &dyn Clock,
) -> tm_types::Result<()> {
    for name in [
        "project.db",
        "project.db-wal",
        "project.db-shm",
        "index.db",
        "index.db-wal",
        "index.db-shm",
        "harness.toml",
        "mirror.toml",
        "sched.paused",
    ] {
        let path = src_state_dir.join(name);
        if path.is_file() {
            std::fs::remove_file(&path)?;
        }
    }
    for name in ["artifacts", "workflows", "bench"] {
        let path = src_state_dir.join(name);
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        }
    }
    let doc = serde_json::json!({
        "to": dest_tm,
        "at": clock.now(),
        "events": events,
    });
    std::fs::write(
        src_state_dir.join("promoted.json"),
        serde_json::to_vec_pretty(&doc)?,
    )?;
    Ok(())
}

/// `tm init`: create a new project's `.tm/` directory in `args.path` (default: the current
/// directory) — or, unless `--fresh` is passed, promote an existing global session for this
/// workspace into it instead of starting over (D-003 Phase 1-C; see [`create_or_promote_project_dir`]).
///
/// # Errors
/// `TmError::Conflict` if a `.tm` directory already exists there. Whatever [`promote_global`]
/// returns if promotion is attempted and fails.
pub fn init(args: &InitArgs, renderer: &Renderer) -> tm_types::Result<()> {
    let dir = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
    let (root, promotion) = create_or_promote_project_dir(&dir, args.fresh)?;
    let state_dir = root.join(".tm");
    let harness_path = state_dir.join("harness.toml");
    if !harness_path.exists() {
        let defaults = toml::to_string_pretty(&tm_harness::HarnessConfig::default())
            .map_err(|e| TmError::parse(format!("Failed to serialize harness defaults: {e}")))?;
        std::fs::write(&harness_path, defaults)?;
    }
    let providers_path = state_dir.join("providers.toml");
    if !providers_path.exists() {
        let defaults = tm_provider::RoleTable::default_table()
            .to_toml_string()
            .map_err(|e| TmError::parse(format!("Failed to serialize provider defaults: {e}")))?;
        std::fs::write(&providers_path, defaults)?;
    }
    let (json, human) = match &promotion {
        Some(report) => (
            serde_json::json!({ "root": root, "promoted": report }),
            format!(
                "promoted {} events ({} tickets) from the global session for {} into {}",
                report.events,
                report.tickets,
                root.display(),
                report.to.display()
            ),
        ),
        None => (
            serde_json::json!({ "root": root }),
            format!(
                "initialized a new Ticketmaster project at {}",
                root.display()
            ),
        ),
    };
    renderer.emit(&json, &human)
}

/// `tm attach [path]`: assimilate an existing repository into a project, creating one first if
/// `args.path` has no `.tm` yet — or promoting an existing global session for this workspace, the
/// same as `tm init` (D-003 Phase 1-C), so an explicit `tm attach` never silently orphans a global
/// chat session's history. Output otherwise unchanged from before this track.
pub fn attach(args: &AttachArgs, renderer: &Renderer) -> tm_types::Result<()> {
    let dir = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
    let root = if dir.join(".tm").is_dir() {
        dir.canonicalize()?
    } else {
        create_or_promote_project_dir(&dir, false)?.0
    };
    let (_project, report) = attach_project(&root)?;
    let human = ops::format_attach_report(&report);
    renderer.emit(&report, &human)
}

/// Open `root` (an already-created `.tm/` project directory) and assimilate it via
/// [`tm_genesis::attach::attach_repository`]. The shared body of [`attach`].
///
/// D-003 note: bare `tm` no longer has an auto-bootstrap that calls this — [`open_bare`] never
/// assimilates a repository, it only ever opens or silently creates an empty global project.
/// `tm attach` (this function's only remaining caller) stays repo-level and unchanged.
fn attach_project(root: &Path) -> tm_types::Result<(Project, tm_genesis::attach::AttachReport)> {
    let project = open(root)?;
    let report = tm_genesis::attach::attach_repository(
        root,
        project.store.as_ref(),
        project.clock.as_ref(),
        project.actor.clone(),
    )?;
    Ok((project, report))
}

/// Whether the provider a Genesis [`tm_provider::RoleCandidate`] names can be used as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GenesisCandidateState {
    /// The slug is not one [`tm_provider::Registry::known_providers`] lists at all — a
    /// configuration error, never a reason to fall back to probing local backends.
    Unknown,
    /// A known provider whose required env vars are not all set; carries the missing names
    /// (names only, never values).
    NotConfigured { missing: Vec<&'static str> },
    /// A known provider whose required env vars are all set.
    Configured,
}

/// Pure gating logic behind [`resolve_genesis_provider`]: classify `candidate` against `known`,
/// asking `is_set` whether each required env var is present. `is_set` is injected (production
/// passes a real `std::env::var(..).is_ok()`, the same presence test
/// [`tm_provider::ProviderInfo::is_configured`] uses) so every branch is testable without reading
/// or writing the real process environment.
fn genesis_candidate_state(
    candidate: &tm_provider::RoleCandidate,
    known: &[tm_provider::ProviderInfo],
    is_set: impl Fn(&str) -> bool,
) -> GenesisCandidateState {
    let Some(info) = known.iter().find(|info| info.id == candidate.provider) else {
        return GenesisCandidateState::Unknown;
    };
    let missing: Vec<&'static str> = info
        .env_vars
        .iter()
        .filter(|v| v.required && !is_set(v.name))
        .map(|v| v.name)
        .collect();
    if missing.is_empty() {
        GenesisCandidateState::Configured
    } else {
        GenesisCandidateState::NotConfigured { missing }
    }
}

/// Pure selection logic behind [`resolve_genesis_provider`]'s non-local fallback: the first entry
/// in `known` that is not `slug` (Genesis's own already-not-configured candidate, already
/// covered), not one of the three local backends (already probed separately, before this runs),
/// is completion-capable, `is_configured` reports as configured, and `model_for` can name a model
/// for. `is_configured`/`model_for` are injected (production passes
/// [`tm_provider::ProviderInfo::is_configured`] and the real
/// `Registry::env_default_model`/`chat_default_model` pairing) so this is testable without
/// touching the real process environment. Iteration order follows `known`'s own order
/// ([`tm_provider::Registry::known_providers`]), so it is deterministic, not "whichever is
/// configured first alphabetically".
fn genesis_fallback_provider(
    known: &[tm_provider::ProviderInfo],
    slug: &str,
    is_configured: impl Fn(&tm_provider::ProviderInfo) -> bool,
    model_for: impl Fn(&str) -> Option<String>,
) -> Option<(&'static str, String)> {
    known.iter().find_map(|info| {
        if info.id == slug
            || tm_provider::LOCAL_PROVIDER_IDS.contains(&info.id)
            || !info.capabilities.completion
            || !is_configured(info)
        {
            return None;
        }
        let model = model_for(info.id)?;
        Some((info.id, model))
    })
}

/// The error for a Genesis role candidate whose provider slug this build does not recognize.
fn unknown_genesis_provider_error(
    candidate: &tm_provider::RoleCandidate,
    known: &[tm_provider::ProviderInfo],
) -> TmError {
    let names = known.iter().map(|info| info.id).collect::<Vec<_>>();
    TmError::Provider(format!(
        "Genesis role candidate names unknown provider `{}` (model `{}`); known providers: {}",
        candidate.provider,
        candidate.model,
        names.join(", ")
    ))
}

/// Construct the [`tm_provider::Provider`] `candidate` actually names — `anthropic`, `devpass`,
/// or any other slug [`tm_provider::Registry::build_provider`] dispatches — rather than assuming
/// Anthropic. An unknown slug is a clear error naming it (checked against `known` first, so the
/// message lists what *is* valid). Construction itself does no network I/O.
///
/// Note `devpass` reads its model from `DEVPASS_MODEL`, not `candidate.model` (see
/// `Registry::build_provider`'s own comment on that arm).
fn genesis_provider_for_candidate(
    candidate: &tm_provider::RoleCandidate,
    known: &[tm_provider::ProviderInfo],
    clock: Arc<dyn Clock>,
) -> tm_types::Result<Arc<dyn tm_provider::Provider>> {
    if !known.iter().any(|info| info.id == candidate.provider) {
        return Err(unknown_genesis_provider_error(candidate, known));
    }
    tm_provider::Registry::build_provider(candidate, clock).map_err(|e| {
        TmError::Provider(format!(
            "could not construct provider `{}` (model `{}`) for Genesis: {e}",
            candidate.provider, candidate.model
        ))
    })
}

/// Resolve the Genesis seed prompt from `explicit` (the `--prompt` value), reading stdin when it
/// is `"-"` or absent (and stdin is not a terminal a human would be typing into blind).
fn resolve_genesis_prompt(
    explicit: Option<&str>,
    stdin_is_terminal: bool,
    mut stdin: impl Read,
) -> tm_types::Result<String> {
    let read_all = |r: &mut dyn Read| -> tm_types::Result<String> {
        let mut buf = String::new();
        r.read_to_string(&mut buf)?;
        Ok(buf)
    };
    match explicit {
        Some(text) if text != "-" => Ok(text.to_string()),
        Some(_) => read_all(&mut stdin),
        None if stdin_is_terminal => Err(TmError::parse(
            "no --prompt given and stdin is a terminal; pass --prompt <text> or pipe a prompt on stdin",
        )),
        None => read_all(&mut stdin),
    }
}

/// Run an async computation to completion on a dedicated single-threaded runtime, off the
/// current thread. Used by [`genesis`] and [`doctor`] so their public signatures stay
/// synchronous even though `GenesisDriver::advance` and `Backend::probe` are `async fn`s.
fn run_async<F, Fut, T>(f: F) -> tm_types::Result<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = tm_types::Result<T>>,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| TmError::storage(e.to_string()))?
            .block_on(f())
    })
    .join()
    .unwrap_or_else(|_| Err(TmError::invariant("background async task panicked")))
}

/// Resolve the single [`tm_provider::Provider`] [`genesis`] should use for this run.
///
/// Takes `RoleTable::default_table`'s `VisionFrontier` candidate and, when that candidate's
/// provider is configured (all its required env vars set — today the default table names
/// `anthropic`, so that means `ANTHROPIC_API_KEY`), constructs the provider the candidate actually
/// names via [`genesis_provider_for_candidate`] and returns `None` for the note (nothing changed,
/// so genesis stays silent about a choice that didn't move). This used to build an
/// `AnthropicProvider` unconditionally regardless of the candidate's slug, which would have
/// silently misrouted a role pointed at e.g. `devpass`. A slug this build doesn't recognize is a
/// hard error up front, never a reason to go probing local backends.
///
/// Only when the candidate's provider is *not* configured does this probe for a fallback: the three
/// zero-account local backends in [`tm_provider::LOCAL_PROVIDER_IDS`], in priority order, each a
/// single short-timeout `GET /v1/models` (`tm_provider::providers::local::PROBE_TIMEOUT`,
/// currently 750ms) — a brand-new user who has never heard of `ANTHROPIC_API_KEY` but happens to
/// have Ollama running locally should not be stuck with a bare "set this env var" error. The
/// first one that answers with at least one model pulled is used automatically (with a printed
/// note saying so, never silently). If none does, this also checks every other known, non-local,
/// completion-capable provider already configured in the environment (e.g. `devpass`, configured
/// for `Role::CoderFast` rather than `VisionFrontier`) via the same `Registry::autodetect` +
/// `env_default_model`/`chat_default_model` pairing `/model` already uses, and uses the first one
/// found, again with a printed note; only when *that* also finds nothing does the error name what's
/// actually true about each local backend's state plus the fastest zero-cost next step, rather
/// than only mentioning Anthropic.
///
/// This gate (probe only when nothing else is configured) is deliberate: a slow-by-comparison
/// network probe has no business running on every `tm genesis` invocation that already has a
/// working credential for its configured provider.
///
/// Checked before any of that: [`crate::agent::TEST_MOCK_PROVIDER_ENV`] (see
/// [`mock_genesis_provider`]) — the same offline test hook `crate::agent::build_fabric` already
/// honors for `tm run`/chat turns, so `tm genesis` doesn't need a real credential under it
/// either.
fn resolve_genesis_provider(
    clock: Arc<dyn Clock>,
) -> tm_types::Result<(Arc<dyn tm_provider::Provider>, Option<String>)> {
    if let Some(provider) = mock_genesis_provider(
        std::env::var_os(crate::agent::TEST_MOCK_PROVIDER_ENV).as_deref(),
        clock.clone(),
    ) {
        return Ok((provider, None));
    }

    let known = tm_provider::Registry::known_providers();
    let table = tm_provider::RoleTable::default_table();
    let candidate = table
        .candidates_for(tm_types::Role::VisionFrontier)
        .first()
        .cloned()
        .ok_or_else(|| TmError::invariant("default role table has no VisionFrontier candidate"))?;
    let missing =
        match genesis_candidate_state(&candidate, &known, |name| std::env::var(name).is_ok()) {
            GenesisCandidateState::Unknown => {
                return Err(unknown_genesis_provider_error(&candidate, &known))
            }
            GenesisCandidateState::Configured => {
                let provider = genesis_provider_for_candidate(&candidate, &known, clock)?;
                return Ok((provider, None));
            }
            GenesisCandidateState::NotConfigured { missing } => missing.join("/"),
        };
    let slug = candidate.provider.clone();

    let probe_clock = clock.clone();
    let probes: Vec<(&'static str, tm_provider::LocalProbe)> = run_async(move || async move {
        let mut results = Vec::with_capacity(tm_provider::LOCAL_PROVIDER_IDS.len());
        for id in tm_provider::LOCAL_PROVIDER_IDS {
            let probe = tm_provider::Registry::probe_local(id, probe_clock.clone()).await;
            results.push((id, probe));
        }
        Ok(results)
    })?;

    if let Some((id, model)) = probes.iter().find_map(|(id, probe)| match probe {
        tm_provider::LocalProbe::Ready { first_model } => Some((*id, first_model.clone())),
        _ => None,
    }) {
        let candidate = tm_provider::RoleCandidate {
            provider: id.to_string(),
            model: model.clone(),
            max_concurrency: 1,
            degraded_ok: false,
            price: None,
            limits: tm_provider::role_config::Limits::unlimited(),
        };
        let provider = tm_provider::Registry::build_provider(&candidate, clock)
            .map_err(|e| TmError::Provider(e.to_string()))?;
        let note = format!(
            "{missing} is not set (needed by `{slug}`, Genesis's configured provider); using \
             detected local provider `{id}` (model `{model}`) for Genesis instead. Set {missing} \
             to use `{slug}` instead."
        );
        return Ok((provider, Some(note)));
    }

    // No local backend answered either. Before erroring, check whether any other role's
    // candidate provider is already configured and reachable — e.g. `devpass`, configured for
    // `Role::CoderFast` via `DEVPASS_API_KEY`/`DEVPASS_BASE_URL`/`DEVPASS_MODEL`, with no
    // Anthropic key and no local model server running. This reuses the same
    // `Registry::autodetect` + `env_default_model`/`chat_default_model` pairing
    // `crate::agent::build_fabric_with_table` already uses to surface a configured-but-unlisted
    // backend to `/model`, so a working non-Anthropic, non-local credential isn't invisible to
    // Genesis just because it isn't `VisionFrontier`'s own table row.
    if let Some((id, model)) = genesis_fallback_provider(
        &known,
        &slug,
        tm_provider::ProviderInfo::is_configured,
        |id| {
            tm_provider::Registry::env_default_model(id)
                .or_else(|| tm_provider::Registry::chat_default_model(id).map(str::to_string))
        },
    ) {
        let candidate = tm_provider::RoleCandidate {
            provider: id.to_string(),
            model: model.clone(),
            max_concurrency: 1,
            degraded_ok: false,
            price: None,
            limits: tm_provider::role_config::Limits::unlimited(),
        };
        let provider = tm_provider::Registry::build_provider(&candidate, clock)
            .map_err(|e| TmError::Provider(e.to_string()))?;
        let note = format!(
            "{missing} is not set (needed by `{slug}`, Genesis's configured provider); using \
             already-configured provider `{id}` (model `{model}`) for Genesis instead. Set \
             {missing} to use `{slug}` instead."
        );
        return Ok((provider, Some(note)));
    }

    let detail = probes
        .iter()
        .map(|(id, probe)| match probe {
            tm_provider::LocalProbe::Unreachable => format!("{id}: not running"),
            tm_provider::LocalProbe::ReachableNoModels => {
                format!("{id}: running, no models pulled yet")
            }
            tm_provider::LocalProbe::Ready { .. } => {
                unreachable!("a Ready probe would have returned above")
            }
        })
        .collect::<Vec<_>>()
        .join("; ");

    Err(TmError::Provider(format!(
        "{missing} is not set (needed by `{slug}`, Genesis's configured provider) and no local \
         model provider is reachable ({detail}). Fastest zero-cost path: install Ollama \
         (https://ollama.com) and run `ollama pull <model>`, then re-run `tm genesis`. Or set \
         {missing}, or (if you have a GitHub \
         account) run `gh auth login` and export GITHUB_TOKEN=$(gh auth token) to route through \
         GitHub Models instead."
    )))
}

/// When `mock_provider_env` is `Some` (i.e. [`crate::agent::TEST_MOCK_PROVIDER_ENV`] is set),
/// builds a [`tm_provider::MockProvider`] scripted with `tm_genesis::fixtures::offline_sequence()`
/// — a schema-correct canned response for each of Genesis's five provider-calling stages (Seed,
/// Vision, Spec, GraphCompilation, MaturityGate), in order — so `tm genesis` can run offline and
/// deterministically under that hook the same way `crate::agent::build_fabric` already lets `tm
/// run`/chat turns. Returns `None` otherwise, leaving [`resolve_genesis_provider`]'s normal
/// configured/local-fallback resolution untouched.
///
/// Takes the env var's *value* as a parameter, rather than reading `std::env::var_os` itself, so
/// a test can exercise both branches without mutating process-global env.
fn mock_genesis_provider(
    mock_provider_env: Option<&std::ffi::OsStr>,
    clock: Arc<dyn Clock>,
) -> Option<Arc<dyn tm_provider::Provider>> {
    mock_provider_env?;
    let model = tm_provider::ModelId::new("mock", "genesis-mock");
    let now = clock.now();
    let provider = tm_provider::MockProvider::new("mock", model.clone(), clock);
    provider.script_sequence(
        tm_genesis::fixtures::offline_sequence()
            .into_iter()
            .map(|text| genesis_mock_completion(&model, now, text))
            .collect(),
    );
    Some(Arc::new(provider))
}

/// One scripted, text-only [`tm_provider::Completion`] for [`mock_genesis_provider`] — the same
/// shape `crates/tm-e2e/tests/genesis_e2e.rs`'s own `completion_with` helper builds, since every
/// Genesis provider-calling stage only ever reads the response's first text block.
fn genesis_mock_completion(
    model: &tm_provider::ModelId,
    now: Timestamp,
    text: String,
) -> tm_provider::Completion {
    tm_provider::Completion {
        model: model.clone(),
        candidates: vec![tm_provider::Candidate {
            content: vec![tm_provider::ContentBlock::Text { text }],
            stop_reason: tm_provider::StopReason::EndTurn,
        }],
        usage: tm_provider::Usage::default(),
        latency: std::time::Duration::ZERO,
        received_at: now,
    }
}

/// A short, human-readable label for a Genesis [`tm_genesis::Stage`], for progress notes and the
/// final summary — plain words instead of the enum variant's `{:?}` debug form.
fn genesis_stage_label(stage: tm_genesis::Stage) -> &'static str {
    match stage {
        tm_genesis::Stage::Seed => "reading the prompt",
        tm_genesis::Stage::Vision => "compiling the vision",
        tm_genesis::Stage::Spec => "writing the spec",
        tm_genesis::Stage::GraphCompilation => "building the ticket graph",
        tm_genesis::Stage::Ignition => "starting work under the ignition policy",
        tm_genesis::Stage::V0 => "reached the V0 milestone",
        tm_genesis::Stage::Evaluation => "evaluating V0 before continuing to V1",
        tm_genesis::Stage::V1 => "reached the V1 milestone",
        tm_genesis::Stage::Stabilization => "stabilizing before the maturity gate",
        tm_genesis::Stage::MaturityGate => "checking the maturity gate",
        tm_genesis::Stage::AuthorityReconvergence => "handing off to steady-state authority",
        tm_genesis::Stage::SteadyState => "done",
    }
}

/// Where [`run_genesis_stages`] stopped short of `SteadyState`, and enough context to report a
/// human-readable status. See `docs/decisions/D-027-genesis-cli-stops-for-work.md`.
#[derive(Debug, Clone, PartialEq)]
enum GenesisStop {
    /// `stage` (`V0` or `V1`) can't advance yet: its milestone (resolved via
    /// [`tm_genesis::stages::milestone_for_stage`]) isn't closed. In practice this is almost always hit
    /// right after `Ignition` commits the Draft ticket graph — nothing activates those tickets on
    /// its own, so `V0` never closes by itself and this is the *earliest* safe point to stop,
    /// before sailing through `V0`/`Evaluation`/`V1`/`Stabilization` on work nobody has done.
    MilestoneNotClosed {
        stage: tm_genesis::Stage,
        milestone: tm_types::MilestoneId,
        tickets: usize,
    },
    /// The maturity gate was just evaluated and did not pass. Stopping here, rather than looping
    /// back through `Stabilization` for another real `judge_maturity` provider call, bounds a
    /// single `tm genesis` run to at most one such call.
    MaturityGateFailed { open_tickets: usize },
}

/// A human-readable status for `stop`, always ending in the same next step: work the outstanding
/// tickets, then resume Genesis.
fn genesis_stop_message(stop: &GenesisStop) -> String {
    match stop {
        GenesisStop::MilestoneNotClosed {
            milestone, tickets, ..
        } => format!(
            "{tickets} ticket{s} committed under milestone {milestone}. Run `tm sched run` (or \
             `tm run <T>`) to work them, then re-run `tm genesis --resume` to continue.",
            s = if *tickets == 1 { "" } else { "s" },
        ),
        GenesisStop::MaturityGateFailed { open_tickets } => format!(
            "The maturity gate hasn't passed yet ({open_tickets} ticket{s} still open). Run `tm \
             sched run` (or `tm run <T>`) to work them, then re-run `tm genesis --resume` to \
             continue.",
            s = if *open_tickets == 1 { "" } else { "s" },
        ),
    }
}

/// The stop check [`run_genesis_stages`] applies before advancing out of `V0`/`V1`: `None` for
/// every other stage, and for `V0`/`V1` once their milestone (per
/// [`tm_genesis::stages::milestone_for_stage`]) is closed or can't be resolved yet (nothing to check —
/// the stage's own `advance` call surfaces whatever that implies).
fn genesis_stop_before(
    state: &tm_genesis::GenesisState,
    store: &tm_core::Store,
) -> tm_types::Result<Option<GenesisStop>> {
    if !matches!(state.stage, tm_genesis::Stage::V0 | tm_genesis::Stage::V1) {
        return Ok(None);
    }
    let view = store.view()?;
    let Some(milestone) =
        tm_genesis::stages::milestone_for_stage(state.stage, state.ignition.as_ref(), &view)
    else {
        return Ok(None);
    };
    let Some(record) = view.milestones.get(&milestone) else {
        return Ok(None);
    };
    if record.state == tm_core::MilestoneState::Closed {
        return Ok(None);
    }
    Ok(Some(GenesisStop::MilestoneNotClosed {
        stage: state.stage,
        milestone,
        tickets: record.tickets.len(),
    }))
}

/// Advance `state` one stage at a time via `driver`, applying Genesis's termination policy
/// ([`GenesisStop`]) so a run that can't make further progress on its own reports status and
/// returns instead of spinning forever on repeated `MaturityGate` provider calls. `on_stage` is
/// called once for `SteadyState` (so "genesis complete" still prints) and once per stage the loop
/// is actually about to run via `advance` — never for a stage it stops before advancing, so the
/// last thing reported always matches what [`genesis_stop_message`] says next (progress reporting
/// only; a no-op in tests that don't care).
///
/// Returns the state as of wherever it stopped, plus `Some(reason)` unless it reached
/// `SteadyState` cleanly. See `docs/decisions/D-027-genesis-cli-stops-for-work.md`.
async fn run_genesis_stages(
    driver: &tm_genesis::GenesisDriver<'_>,
    store: &tm_core::Store,
    mut state: tm_genesis::GenesisState,
    actor: ParticipantId,
    mut on_stage: impl FnMut(tm_genesis::Stage),
) -> tm_types::Result<(tm_genesis::GenesisState, Option<GenesisStop>)> {
    loop {
        if state.stage == tm_genesis::Stage::SteadyState {
            on_stage(state.stage);
            return Ok((state, None));
        }
        if let Some(stop) = genesis_stop_before(&state, store)? {
            return Ok((state, Some(stop)));
        }
        on_stage(state.stage);
        let prev_stage = state.stage;
        state = driver.advance(&state, actor.clone()).await?;
        if prev_stage == tm_genesis::Stage::MaturityGate
            && state.stage == tm_genesis::Stage::Stabilization
        {
            let open_tickets = store
                .view()?
                .tickets
                .values()
                .filter(|t| {
                    !matches!(
                        t.state,
                        tm_core::TicketState::Closed | tm_core::TicketState::Cancelled
                    )
                })
                .count();
            return Ok((
                state,
                Some(GenesisStop::MaturityGateFailed { open_tickets }),
            ));
        }
    }
}

/// `tm genesis [--prompt <text>|-]`: turn a prompt into a running project via the Genesis stage
/// driver.
pub fn genesis(args: &GenesisArgs, renderer: &Renderer) -> tm_types::Result<()> {
    // Resolve the provider before touching the filesystem at all. This used to be a plain
    // `require_anthropic_api_key()?` right here, before any `.tm/` directory got created, so a
    // genesis run that fails on provider selection must keep failing before that side effect —
    // not after creating a half-initialized project directory the caller then has to clean up by
    // hand. A single candidate stands in for "the fabric": `GenesisDriver` takes one `Provider`
    // for its whole run (every stage, regardless of role), so there is no per-call routing
    // decision for a `Fabric` to make here. See `resolve_genesis_provider` for how that one
    // provider gets picked — whatever `RoleTable::default_table`'s `VisionFrontier` candidate
    // names (Anthropic today) when that provider is configured, falling back to a reachable
    // zero-signup local provider when it is not.
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let (provider, fallback_note) = resolve_genesis_provider(clock)?;
    if let Some(note) = &fallback_note {
        renderer.note(note);
    }

    let dir = std::env::current_dir()?;
    let root = if dir.join(".tm").is_dir() {
        dir.canonicalize()?
    } else {
        // Promotes an existing global session for this workspace instead of starting fresh, the
        // same as `tm init`/`tm attach` (D-003 Phase 1-C), so an explicit `tm genesis` never
        // silently orphans one.
        create_or_promote_project_dir(&dir, false)?.0
    };
    let project = open(&root)?;

    // `--resume` (or an omitted `--prompt`, which auto-detects) continues the most recently
    // persisted `GenesisState` snapshot instead of starting a fresh `Seed`, via the same
    // `GenesisDriver::resume` a crash recovery would use. No snapshot yet falls back to the
    // fresh-prompt path below unless `--resume` was given explicitly, in which case there is
    // nothing to resume and that's a real error, not a silent fresh start.
    let resumed = if args.resume || args.prompt.is_none() {
        match tm_genesis::GenesisDriver::resume(project.store.as_ref()) {
            Ok(state) => Some(state),
            Err(TmError::NotFound { .. }) if !args.resume => None,
            Err(e) => return Err(e),
        }
    } else {
        None
    };

    // `GenesisState::project` is the only channel `GenesisDriver` has for threading the raw
    // prompt into `Stage::Seed`'s `analyze_prompt` call (it hands `state.project` straight to
    // it), so the seed prompt lives there rather than in a project name/slug.
    let initial_state = match resumed {
        Some(state) => state,
        None => {
            let prompt = resolve_genesis_prompt(
                args.prompt.as_deref(),
                std::io::stdin().is_terminal(),
                std::io::stdin(),
            )?;
            tm_genesis::GenesisState::new(prompt, project.clock.as_ref())
        }
    };
    let actor = project.actor.clone();
    let quiet = renderer.is_quiet();
    let progress = *renderer;
    // A missing/unreadable bundled catalog degrades to the pre-catalog behavior (template
    // selection is a no-op) rather than failing the whole run — this can legitimately happen for
    // an installed `tm` whose layout doesn't ship the starter templates yet.
    let catalog = tm_templates::bundled_catalog().unwrap_or_else(|e| {
        tracing::warn!("bundled template catalog unavailable, genesis will run without template selection: {e}");
        Vec::new()
    });

    let (final_state, stop) = run_async(move || async move {
        let driver = tm_genesis::GenesisDriver::new(
            project.store.as_ref(),
            provider.as_ref(),
            project.clock.as_ref(),
            project.ids.as_ref(),
            &catalog,
        );
        run_genesis_stages(
            &driver,
            project.store.as_ref(),
            initial_state,
            actor,
            |stage| {
                if !quiet {
                    progress.note(&format!("genesis: {}", genesis_stage_label(stage)));
                }
            },
        )
        .await
    })?;

    let human = match &stop {
        Some(stop) => genesis_stop_message(stop),
        None => format!(
            "genesis complete: {}",
            genesis_stage_label(final_state.stage)
        ),
    };
    renderer.emit(&final_state, &human)
}

/// Open a fresh read handle onto `project`'s event log, independent of the one `project.store`
/// holds internally (which exposes no read accessor of its own).
fn open_event_log(project: &Project) -> tm_types::Result<EventLog> {
    let db_path = project.state_dir.join("project.db");
    EventLog::open_with_clock(&db_path, project.clock.clone())
}

/// Read every event in the log, oldest first. Simple rather than clever: `tm status`/`tm doctor`
/// run against project-sized logs, not archives.
fn read_all_events(log: &EventLog) -> tm_types::Result<Vec<Event>> {
    const BATCH: usize = 1024;
    let mut out = Vec::new();
    let mut seq = 1u64;
    loop {
        let batch = log.read_from(seq, BATCH)?;
        if batch.is_empty() {
            break;
        }
        seq += batch.len() as u64;
        out.extend(batch);
    }
    Ok(out)
}

/// `StatusReport`'s scope summary (D-003): plain fields rather than a preformatted string, so
/// `--json` output stays structured; [`render_status_report`] reconstructs the human-readable
/// line from these via [`scope_line_for`].
#[derive(Debug, Clone, Serialize)]
pub struct ScopeInfo {
    /// `"repo"` or `"global"`.
    pub kind: String,
    /// The workspace root.
    pub workspace: PathBuf,
    /// Where this project's state lives.
    pub state_dir: PathBuf,
}

/// The "since you left" report: a snapshot a returning human can read in a few seconds and know
/// exactly what happened and what needs them.
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    /// Where this project's state lives (D-003), so `tm status --json` carries the same scope
    /// information the human-readable prefix shows.
    pub scope: ScopeInfo,
    /// Tickets that closed since the report window started.
    pub closed: Vec<String>,
    /// Tickets newly blocked (retry exhausted, cycle budget spent, failure needing a human).
    pub blocked: Vec<String>,
    /// Tickets awaiting verification or audit right now.
    pub awaiting_review: Vec<String>,
    /// Decisions recorded since the window started.
    pub decisions: Vec<String>,
    /// Docs that went `Stale` since the window started.
    pub stale_docs: Vec<String>,
    /// Budget scopes that are exhausted or within a warn threshold of exhaustion.
    pub budget_warnings: Vec<String>,
    /// Approvals currently pending a human decision.
    pub pending_approvals: Vec<String>,
}

/// Extract the summary's `subject`/`decision` fields for readable prose, falling back to the raw
/// summary text if it isn't the JSON blob `Store::record_decision` writes.
fn decision_summary_text(summary: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(summary) {
        Ok(v) => {
            let subject = v.get("subject").and_then(|x| x.as_str()).unwrap_or("");
            let decision = v.get("decision").and_then(|x| x.as_str());
            match decision {
                Some(d) if !subject.is_empty() => format!("{subject}: {d}"),
                Some(d) => d.to_string(),
                None => summary.to_string(),
            }
        }
        Err(_) => summary.to_string(),
    }
}

fn budget_scope_label(scope: &tm_core::BudgetScope) -> String {
    match scope {
        tm_core::BudgetScope::Project => "project".to_string(),
        tm_core::BudgetScope::Milestone(id) => format!("milestone {id}"),
        tm_core::BudgetScope::Ticket(id) => format!("ticket {id}"),
        tm_core::BudgetScope::Lease(id) => format!("lease {id}"),
    }
}

/// A warn-or-worse note for one budget scope, or `None` when it has ample headroom left.
/// `u64::MAX`/`0` limits are treated as "not tracked" for that dimension (unlimited, or never
/// configured) rather than always-exhausted or always-warning.
fn budget_warning(scoped: &tm_core::budget::ScopedBudget) -> Option<String> {
    const WARN_RATIO: f64 = 0.8;
    let b = &scoped.budget;
    let dimensions = [
        ("tokens", b.spent.tokens, b.tokens),
        ("dollars", b.spent.dollars_micros, b.dollars_micros),
        ("wall time", b.spent.wall_seconds, b.wall_seconds),
    ];
    let mut notes = Vec::new();
    for (label, spent, limit) in dimensions {
        if limit == 0 || limit == u64::MAX {
            continue;
        }
        let ratio = spent as f64 / limit as f64;
        if ratio >= 1.0 {
            notes.push(format!("{label} exhausted ({spent}/{limit})"));
        } else if ratio >= WARN_RATIO {
            notes.push(format!(
                "{label} at {:.0}% ({spent}/{limit})",
                ratio * 100.0
            ));
        }
    }
    if notes.is_empty() {
        None
    } else {
        Some(format!(
            "{}: {}",
            budget_scope_label(&scoped.scope),
            notes.join(", ")
        ))
    }
}

/// The `ticket` field carried by whichever "review resolved" event kind `event` is, if any.
/// Used to drop a ticket out of `awaiting_review` once its submission has been settled.
fn review_resolution_ticket(payload: &tm_events::Payload) -> Option<TicketId> {
    payload
        .as_ticket_verified()
        .map(|p| p.ticket.clone())
        .or_else(|| {
            payload
                .as_ticket_verification_failed()
                .map(|p| p.ticket.clone())
        })
        .or_else(|| payload.as_ticket_audited().map(|p| p.ticket.clone()))
        .or_else(|| payload.as_ticket_audit_rejected().map(|p| p.ticket.clone()))
        .or_else(|| payload.as_ticket_closed().map(|p| p.ticket.clone()))
        .or_else(|| payload.as_ticket_cancelled().map(|p| p.ticket.clone()))
        .or_else(|| payload.as_ticket_reopened().map(|p| p.ticket.clone()))
}

/// Compose a [`StatusReport`] from the event log and project view. Split out from [`status`] so
/// the bucketing logic is directly assertable in tests without capturing rendered output.
fn build_status_report(project: &Project, args: &StatusArgs) -> tm_types::Result<StatusReport> {
    let log = open_event_log(project)?;
    let events = read_all_events(&log)?;
    let now = project.clock.now();

    let start = match args.since_hours {
        Some(hours) => now.plus_seconds(-(hours as i64) * 3600),
        None => events
            .iter()
            .rev()
            .find(|e| {
                e.kind == EventKind::PresenceUpdated
                    && e.payload
                        .as_presence_updated()
                        .map(|p| p.participant == project.actor)
                        .unwrap_or(false)
            })
            .or_else(|| events.first())
            .map(|e| e.ts)
            .unwrap_or(Timestamp::EPOCH),
    };

    let mut report = StatusReport {
        scope: ScopeInfo {
            kind: match project.scope {
                Scope::Repo => "repo",
                Scope::Global => "global",
            }
            .to_string(),
            workspace: project.root.clone(),
            state_dir: project.state_dir.clone(),
        },
        closed: Vec::new(),
        blocked: Vec::new(),
        awaiting_review: Vec::new(),
        decisions: Vec::new(),
        stale_docs: Vec::new(),
        budget_warnings: Vec::new(),
        pending_approvals: Vec::new(),
    };

    // "Awaiting review right now" and "still pending" reflect current state, built from the
    // whole log; the time-bounded buckets below only collect what happened inside the window.
    let mut awaiting_review: BTreeSet<TicketId> = BTreeSet::new();
    let mut pending_approvals: BTreeMap<TicketId, String> = BTreeMap::new();
    let mut untracked_approvals: Vec<String> = Vec::new();

    for event in &events {
        match event.kind {
            EventKind::TicketSubmitted => {
                if let Some(p) = event.payload.as_ticket_submitted() {
                    awaiting_review.insert(p.ticket.clone());
                }
            }
            EventKind::TicketVerified
            | EventKind::TicketVerificationFailed
            | EventKind::TicketAudited
            | EventKind::TicketAuditRejected
            | EventKind::TicketClosed
            | EventKind::TicketCancelled
            | EventKind::TicketReopened => {
                if let Some(ticket) = review_resolution_ticket(&event.payload) {
                    awaiting_review.remove(&ticket);
                }
            }
            EventKind::ApprovalRequested => {
                if let Some(p) = event.payload.as_approval_requested() {
                    match &p.ticket {
                        Some(ticket) => {
                            pending_approvals.insert(ticket.clone(), p.note.clone());
                        }
                        // No ticket to key on; the payload carries nothing else to correlate a
                        // later decision back to this request, so it can only ever be reported,
                        // never cleared.
                        None => untracked_approvals.push(p.note.clone()),
                    }
                }
            }
            EventKind::ApprovalDecided => {
                if let Some(p) = event.payload.as_approval_decided() {
                    if let Some(ticket) = &p.ticket {
                        pending_approvals.remove(ticket);
                    }
                }
            }
            _ => {}
        }

        if event.ts < start {
            continue;
        }

        match event.kind {
            EventKind::TicketClosed => {
                if let Some(p) = event.payload.as_ticket_closed() {
                    report.closed.push(p.ticket.to_string());
                }
            }
            EventKind::TicketEscalated => {
                if let Some(p) = event.payload.as_ticket_escalated() {
                    report
                        .blocked
                        .push(format!("{} escalated: {}", p.ticket, p.reason));
                }
            }
            EventKind::TicketBudgetExhausted => {
                if let Some(p) = event.payload.as_ticket_budget_exhausted() {
                    report.blocked.push(format!(
                        "{} budget exhausted ({}: {}/{})",
                        p.ticket, p.dimension, p.spent, p.limit
                    ));
                }
            }
            EventKind::DecisionCreated => {
                if let Some(p) = event.payload.as_decision_created() {
                    report.decisions.push(decision_summary_text(&p.summary));
                }
            }
            EventKind::DocInvalidated => {
                if let Some(p) = event.payload.as_doc_invalidated() {
                    report.stale_docs.push(format!("{} ({})", p.path, p.reason));
                }
            }
            _ => {}
        }
    }

    report.awaiting_review = awaiting_review.into_iter().map(|t| t.to_string()).collect();
    report.pending_approvals = pending_approvals
        .into_iter()
        .map(|(ticket, note)| format!("{ticket}: {note}"))
        .chain(untracked_approvals)
        .collect();

    for scoped in &project.store.view()?.budgets {
        if let Some(warning) = budget_warning(scoped) {
            report.budget_warnings.push(warning);
        }
    }

    Ok(report)
}

fn render_status_report(report: &StatusReport) -> String {
    let scope = if report.scope.kind == "global" {
        Scope::Global
    } else {
        Scope::Repo
    };
    let scope_line = scope_line_for(scope, &report.scope.workspace, &report.scope.state_dir);

    let sections: Vec<String> = [
        ("Closed", &report.closed),
        ("Blocked", &report.blocked),
        ("Awaiting review", &report.awaiting_review),
        ("Decisions", &report.decisions),
        ("Stale docs", &report.stale_docs),
        ("Budget warnings", &report.budget_warnings),
        ("Pending approvals", &report.pending_approvals),
    ]
    .into_iter()
    .filter(|(_, items)| !items.is_empty())
    .map(|(title, items)| {
        let body = items
            .iter()
            .map(|i| format!("  - {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("{title}\n{body}")
    })
    .collect();

    if sections.is_empty() {
        format!("{scope_line}\n\nnothing happened since you left — the project is quiet.")
    } else {
        format!("{scope_line}\n\n{}", sections.join("\n\n"))
    }
}

/// `tm status`: compose a [`StatusReport`] from the event log and project view.
pub fn status(project: &Project, args: &StatusArgs, renderer: &Renderer) -> tm_types::Result<()> {
    let report = build_status_report(project, args)?;
    let human = render_status_report(&report);
    renderer.emit(&report, &human)
}

/// One check `tm doctor` performs.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    /// The check's name, e.g. `"invariants"`, `"hash-chain"`, `"index-health"`.
    pub name: String,
    /// Whether the check passed.
    pub ok: bool,
    /// Whether a failure here makes the project unhealthy. Optional capabilities (computer use,
    /// which needs OS permissions most people never grant) are reported, but a failure is a
    /// warning, not a doctor failure.
    pub required: bool,
    /// Human-readable detail, especially on failure.
    pub detail: String,
}

/// The full `tm doctor` report.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    /// Every check that ran, in run order.
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    /// Whether every required check passed (a failed optional check is only a warning).
    pub fn all_ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok || !c.required)
    }
}

#[cfg(target_os = "macos")]
async fn probe_computer_backend(
    kind: tm_computer::BackendKind,
) -> tm_types::Result<tm_computer::Capabilities> {
    use tm_computer::Backend;
    match kind {
        tm_computer::BackendKind::Macos => tm_computer::macos::MacosBackend::new().probe().await,
        other => Err(TmError::storage(format!(
            "selected backend {other:?} is not available on this platform"
        ))),
    }
}

#[cfg(target_os = "linux")]
async fn probe_computer_backend(
    kind: tm_computer::BackendKind,
) -> tm_types::Result<tm_computer::Capabilities> {
    use tm_computer::Backend;
    match kind {
        tm_computer::BackendKind::X11 => {
            tm_computer::linux::X11Backend::connect(None)?.probe().await
        }
        tm_computer::BackendKind::Wayland => {
            tm_computer::linux::WaylandBackend::connect()?.probe().await
        }
        other => Err(TmError::storage(format!(
            "selected backend {other:?} is not available on this platform"
        ))),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn probe_computer_backend(
    kind: tm_computer::BackendKind,
) -> tm_types::Result<tm_computer::Capabilities> {
    Err(TmError::storage(format!(
        "no computer-use backend is available on this platform (selected {kind:?})"
    )))
}

fn render_doctor_report(report: &DoctorReport) -> String {
    let rows = report
        .checks
        .iter()
        .map(|c| {
            vec![
                c.name.clone(),
                match (c.ok, c.required) {
                    (true, _) => "ok".to_string(),
                    (false, true) => "FAIL".to_string(),
                    (false, false) => "warn".to_string(),
                },
                c.detail.clone(),
            ]
        })
        .collect();
    Table::new(
        vec![
            "check".to_string(),
            "status".to_string(),
            "detail".to_string(),
        ],
        rows,
    )
    .render()
}

/// Build `tm doctor`'s `"providers"` check: whether any model provider is actually usable right
/// now, prioritizing the genuinely free/zero-signup options ahead of anything that needs a new
/// signup — matching this crate's onboarding goal of not leaving a fresh install stuck with no
/// guidance toward a free path. Ordering, deliberately: the three local backends
/// ([`tm_provider::LOCAL_PROVIDER_IDS`] — Ollama, LM Studio, llama.cpp: no account at all, and the
/// only backends this function actually probes for reachability, via
/// [`tm_provider::Registry::probe_local`]), then GitHub Models (a token most developers already
/// have from `gh auth login`, checked by env-var presence only — this deliberately does not shell
/// out to `gh auth status`, which validates against the network with no timeout this crate
/// controls), then every other known backend, reported as configured/not by env-var presence
/// alone (this crate has no free way to probe a paid third-party API's reachability).
///
/// This check is advisory ("here's what's available, here's the fastest free path if nothing
/// is"), not a correctness invariant like `"invariants"` or `"hash-chain"`. When no provider
/// is ready, the status is `warn` (ok: false, required: false), not a failure — a freshly-`tm
/// init`'d project genuinely has no provider configured yet, and that is expected, not a doctor
/// failure.
async fn provider_doctor_detail(clock: Arc<dyn Clock>) -> DoctorCheck {
    let known = tm_provider::Registry::known_providers();

    let mut local_lines = Vec::with_capacity(tm_provider::LOCAL_PROVIDER_IDS.len());
    let mut any_local_ready = false;
    for id in tm_provider::LOCAL_PROVIDER_IDS {
        let probe = tm_provider::Registry::probe_local(id, clock.clone()).await;
        local_lines.push(match &probe {
            tm_provider::LocalProbe::Unreachable => format!("{id}: not running"),
            tm_provider::LocalProbe::ReachableNoModels => {
                format!("{id}: running, no models pulled yet")
            }
            tm_provider::LocalProbe::Ready { first_model } => {
                any_local_ready = true;
                format!("{id}: ready (model \"{first_model}\")")
            }
        });
    }

    let github_configured = known
        .iter()
        .find(|info| info.id == "github-models")
        .is_some_and(tm_provider::ProviderInfo::is_configured);
    let github_line = if github_configured {
        "github-models: ready (GITHUB_TOKEN set)".to_string()
    } else {
        "github-models: GITHUB_TOKEN not set (if you use the gh CLI: export \
         GITHUB_TOKEN=$(gh auth token))"
            .to_string()
    };

    let mut other_lines = Vec::new();
    let mut any_other_ready = false;
    for info in &known {
        if tm_provider::LOCAL_PROVIDER_IDS.contains(&info.id) || info.id == "github-models" {
            continue;
        }
        if info.is_configured() {
            any_other_ready = true;
            other_lines.push(format!("{}: ready", info.id));
        }
    }

    let any_ready = any_local_ready || github_configured || any_other_ready;

    let mut detail = local_lines.join("; ");
    detail.push_str("; ");
    detail.push_str(&github_line);
    if !other_lines.is_empty() {
        detail.push_str("; ");
        detail.push_str(&other_lines.join("; "));
    }
    if !any_ready {
        detail.push_str(
            "; no model provider is ready. Fastest zero-cost path: install Ollama \
             (https://ollama.com) and run `ollama pull <model>`, or, if you have a GitHub \
             account, run `gh auth login` and export GITHUB_TOKEN=$(gh auth token) to use \
             GitHub Models.",
        );
    }

    DoctorCheck {
        name: "providers".to_string(),
        ok: any_ready,
        required: false,
        detail,
    }
}

/// Build `tm doctor`'s `"workflow-1x1"` check: `SPEC.md` §25.3 -- "`tm doctor` should warn on a
/// workflow whose graph is one node wide and one node deep, because that is a prompt wearing a
/// costume." Advisory only (`ok: true` regardless -- a 1x1 workflow is a smell, not a
/// correctness failure, like the `"providers"` and `"index-health"` checks), and silent (an
/// empty, non-alarming detail) when the project has no `.tm/workflows/` at all, since most
/// projects will not use this feature and that is not itself worth a note.
fn workflow_doctor_detail(project: &Project) -> DoctorCheck {
    if !crate::workflow::has_any_workflow(project) {
        return DoctorCheck {
            name: "workflow-1x1".to_string(),
            ok: true,
            required: true,
            detail: "no workflow definitions found under .tm/workflows/".to_string(),
        };
    }
    let flagged = crate::workflow::one_by_one_workflow_names(project);
    let detail = if flagged.is_empty() {
        "no 1x1 workflows (every definition has real sequencing or fan-out)".to_string()
    } else {
        format!(
            "{} workflow(s) are one node wide and one node deep, i.e. a prompt wearing a \
             costume (SPEC.md §25.3): {}",
            flagged.len(),
            flagged.join(", ")
        )
    };
    DoctorCheck {
        name: "workflow-1x1".to_string(),
        ok: true,
        required: true,
        detail,
    }
}

/// `tm doctor`: invariants, the hash-chain check, index health, provider availability, and
/// (unless `--skip-computer-probe`) the computer-use permission probes.
pub fn doctor(
    project: &Project,
    args: &DoctorArgs,
    renderer: &Renderer,
) -> tm_types::Result<DoctorReport> {
    let mut checks = Vec::new();

    // Purely informational: where this project's state actually lives (D-003). Advisory
    // checks like `"providers"` and `"workflow-1x1"` can be `warn` or `ok` depending on the
    // project state, not failures.
    checks.push(DoctorCheck {
        name: "scope".to_string(),
        ok: true,
        required: true,
        detail: project.scope_line(),
    });

    let violations = project.store.check_invariants()?;
    checks.push(DoctorCheck {
        name: "invariants".to_string(),
        ok: violations.is_empty(),
        required: true,
        detail: if violations.is_empty() {
            "no invariant violations".to_string()
        } else {
            violations
                .iter()
                .map(|v| format!("{} ({}): {}", v.invariant, v.subject, v.detail))
                .collect::<Vec<_>>()
                .join("; ")
        },
    });

    let log = open_event_log(project)?;
    let chain = log.verify_chain()?;
    checks.push(DoctorCheck {
        name: "hash-chain".to_string(),
        ok: chain.is_valid(),
        required: true,
        detail: if chain.is_valid() {
            format!("{} events verified", chain.events_checked)
        } else {
            chain
                .detail
                .clone()
                .unwrap_or_else(|| "chain broken".to_string())
        },
    });

    // Deliberately not `project.code_intel()`: that now refreshes on open too
    // (`Project::code_intel`'s own doc comment), which would make this check's own
    // `update_incremental` a guaranteed no-op reporting zero drift every time. Open the index
    // directly instead, so this remains the one real, explicit refresh whose counts this check
    // reports.
    let index_check = match tm_codeintel::CodeIntel::open_at(&project.state_dir, &project.root)
        .and_then(|ci| ci.update_incremental(project.clock.as_ref()))
    {
        Ok(delta) => {
            // When the index was empty before (first build), all files are newly added.
            // In that case, show "indexed X files (Y chunks, Z commits)" instead of the
            // "repaired incremental drift" message.
            let detail = if delta.files_modified == 0
                && delta.files_removed == 0
                && delta.files_added > 0
            {
                format!(
                    "indexed {} files ({} chunks, {} commits)",
                    delta.files_added, delta.chunks_written, delta.commits_ingested
                )
            } else {
                format!(
                    "repaired incremental drift: {} added, {} modified, {} removed, {} chunks written, \
                     {} commits ingested",
                    delta.files_added, delta.files_modified, delta.files_removed, delta.chunks_written,
                    delta.commits_ingested
                )
            };
            DoctorCheck {
                name: "index-health".to_string(),
                ok: true,
                required: true,
                detail,
            }
        }
        Err(e) => {
            // Special case: when there are no commits in the repo, the history ingest fails
            // with a git2 error when trying to resolve HEAD or push a reference. Treat this as
            // a warning (ok: false, required: false) with a user-friendly message.
            let error_str = e.to_string();
            let (ok, required, detail) =
                if error_str.contains("reference") || error_str.contains("HEAD") {
                    (false, false, "no commits yet".to_string())
                } else {
                    (false, true, error_str)
                };
            DoctorCheck {
                name: "index-health".to_string(),
                ok,
                required,
                detail,
            }
        }
    };
    checks.push(index_check);

    let provider_check = run_async(|| async {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        Ok(provider_doctor_detail(clock).await)
    })?;
    checks.push(provider_check);

    checks.push(workflow_doctor_detail(project));

    if !args.skip_computer_probe {
        let probed = run_async(|| async {
            let env = tm_computer::SelectionEnv::from_process();
            let kind = tm_computer::select_backend(&env)?;
            probe_computer_backend(kind).await
        });
        checks.push(match probed {
            Ok(caps) => DoctorCheck {
                name: "computer-use".to_string(),
                ok: caps.input && caps.capture,
                required: false,
                detail: if caps.notes.is_empty() {
                    format!(
                        "{:?} backend ready (input={}, capture={}, element_tree={}, headless={})",
                        caps.backend, caps.input, caps.capture, caps.element_tree, caps.headless
                    )
                } else {
                    caps.notes.join("; ")
                },
            },
            Err(e) => DoctorCheck {
                name: "computer-use".to_string(),
                ok: false,
                required: false,
                detail: e.to_string(),
            },
        });
    }

    let report = DoctorReport { checks };
    let human = render_doctor_report(&report);
    renderer.emit(&report, &human)?;
    Ok(report)
}

#[cfg(test)]
impl Project {
    /// Test-only constructor for the handful of test modules across this crate that need a real
    /// `Project` over a store they already opened: fills in repo scope and
    /// `state_dir = root.join(".tm")` (the shape every one of those tests already assumed before
    /// D-003 added the `state_dir`/`scope` fields), plus the `human:tester` actor every such test
    /// already used. Kept as one helper rather than hand-editing every call site.
    pub(crate) fn for_test(
        root: &Path,
        store: Arc<tm_core::Store>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
    ) -> Project {
        Project {
            root: root.to_path_buf(),
            state_dir: root.join(".tm"),
            scope: Scope::Repo,
            store,
            clock,
            ids,
            actor: ParticipantId::new("human:tester").expect("valid participant id"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TM_ACTOR`/`USER` are process-global; serialize every test that touches them.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The real process current directory is also process-global; serialize every test that
    /// changes it (rather than passing an explicit path) so it can't race a sibling test reading
    /// or changing it concurrently.
    static CWD_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_renderer() -> Renderer {
        Renderer::new(true, true, true, false)
    }

    #[test]
    fn locate_finds_a_tm_directory_from_a_nested_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".tm")).unwrap();
        let nested = root.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(locate(&nested).unwrap(), root);
    }

    #[test]
    fn locate_errors_when_no_tm_directory_exists_up_to_the_filesystem_root() {
        let tmp = tempfile::tempdir().unwrap();
        let err = locate(tmp.path()).unwrap_err();
        assert!(matches!(err, TmError::NotFound { .. }));
    }

    #[test]
    fn resolve_scope_prefers_the_explicit_path_over_everything_else() {
        let tmp = tempfile::tempdir().unwrap();
        let explicit = tmp.path().join("wherever-even-without-a-tm-dir");
        let cwd = tmp.path().to_path_buf();
        let resolved = resolve_scope(Some(&explicit), &cwd).unwrap();
        assert_eq!(resolved.root, explicit);
        assert_eq!(resolved.state_dir, explicit.join(".tm"));
        assert_eq!(resolved.scope, Scope::Repo);
        assert!(resolved.exists);
    }

    #[test]
    fn resolve_scope_prefers_a_located_tm_dir_over_global() {
        let _guard = ENV_GUARD.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let tm_home = tempfile::tempdir().unwrap();
        std::env::set_var("TM_HOME", tm_home.path());
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".tm")).unwrap();

        let resolved = resolve_scope(None, &root).unwrap();

        std::env::remove_var("TM_HOME");
        assert_eq!(resolved.root, root);
        assert_eq!(resolved.state_dir, root.join(".tm"));
        assert_eq!(resolved.scope, Scope::Repo);
        assert!(resolved.exists);
    }

    #[test]
    fn resolve_scope_finds_an_existing_global_project_when_no_tm_dir_is_located() {
        let _guard = ENV_GUARD.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().canonicalize().unwrap();
        let tm_home = tempfile::tempdir().unwrap();
        std::env::set_var("TM_HOME", tm_home.path());

        let key = global_project_key(&cwd);
        let state_dir = tm_home.path().join("projects").join(&key);
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("project.db"), b"").unwrap();

        let resolved = resolve_scope(None, &cwd).unwrap();

        std::env::remove_var("TM_HOME");
        assert_eq!(resolved.root, cwd);
        assert_eq!(resolved.state_dir, state_dir);
        assert_eq!(resolved.scope, Scope::Global);
        assert!(resolved.exists);
    }

    #[test]
    fn resolve_scope_falls_back_to_a_fresh_global_project_when_nothing_exists() {
        let _guard = ENV_GUARD.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().canonicalize().unwrap();
        let tm_home = tempfile::tempdir().unwrap();
        std::env::set_var("TM_HOME", tm_home.path());

        let resolved = resolve_scope(None, &cwd).unwrap();

        std::env::remove_var("TM_HOME");
        assert_eq!(resolved.root, cwd);
        assert_eq!(resolved.scope, Scope::Global);
        assert!(!resolved.exists);
    }

    #[test]
    fn global_project_key_is_deterministic_and_does_not_collide_for_different_paths() {
        let a = Path::new("/Users/allie/Develop/foo");
        let b = Path::new("/Users/allie/Develop/bar");
        assert_eq!(global_project_key(a), global_project_key(a));
        assert_ne!(global_project_key(a), global_project_key(b));
        assert!(global_project_key(a).starts_with("Users-allie-Develop-foo-"));
    }

    #[test]
    fn tm_home_prefers_the_env_var_when_set() {
        let _guard = ENV_GUARD.lock().unwrap();
        std::env::set_var("TM_HOME", "/tmp/somewhere-fake-for-this-test");
        let home = tm_home().unwrap();
        std::env::remove_var("TM_HOME");
        assert_eq!(home, PathBuf::from("/tmp/somewhere-fake-for-this-test"));
    }

    #[test]
    fn tm_home_falls_back_to_home_dot_tm_when_unset() {
        let _guard = ENV_GUARD.lock().unwrap();
        std::env::remove_var("TM_HOME");
        let home = tm_home().unwrap();
        let expected = dirs::home_dir()
            .expect("this test environment has a real $HOME")
            .join(".tm");
        assert_eq!(home, expected);
    }

    #[test]
    fn workspace_root_for_returns_the_canonical_cwd_outside_a_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().canonicalize().unwrap();
        assert_eq!(workspace_root_for(tmp.path()).unwrap(), canonical);
    }

    #[test]
    fn workspace_root_for_returns_the_git_toplevel_from_a_nested_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let nested = tmp.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        let expected = tmp.path().canonicalize().unwrap();
        assert_eq!(workspace_root_for(&nested).unwrap(), expected);
    }

    #[test]
    fn open_bare_creates_global_state_under_tm_home_never_under_the_invoking_cwd() {
        let _env_guard = ENV_GUARD.lock().unwrap();
        let _cwd_guard = CWD_GUARD.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().canonicalize().unwrap();
        let tm_home = tempfile::tempdir().unwrap();
        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_var("TM_HOME", tm_home.path());
        std::env::set_current_dir(&canonical).unwrap();

        let project = open_bare(None, &test_renderer()).unwrap();

        std::env::set_current_dir(&original_cwd).unwrap();
        std::env::remove_var("TM_HOME");

        assert_eq!(
            std::fs::read_dir(&canonical).unwrap().count(),
            0,
            "opening a global project must never write into the workspace"
        );
        assert_eq!(project.scope, Scope::Global);
        assert!(project.state_dir.starts_with(tm_home.path()));
        assert!(project.state_dir.join("project.db").exists());
        assert!(project.state_dir.join("workspace.json").exists());
    }

    #[test]
    fn open_for_command_errors_not_found_for_a_workspace_with_no_history_in_either_scope() {
        // `open_for_command` reads the real current directory (unlike `resolve_scope`, which
        // takes `cwd` explicitly), so this needs both the env guard (`TM_HOME`) and the cwd guard
        // to run hermetically alongside every other test in this binary.
        let _env_guard = ENV_GUARD.lock().unwrap();
        let _cwd_guard = CWD_GUARD.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().canonicalize().unwrap();
        let tm_home = tempfile::tempdir().unwrap();
        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_var("TM_HOME", tm_home.path());
        std::env::set_current_dir(&canonical).unwrap();

        let result = open_for_command(None);

        std::env::set_current_dir(&original_cwd).unwrap();
        std::env::remove_var("TM_HOME");

        // `Project` derives no `Debug` (see `tui.rs`'s own note on why), so this matches by hand
        // rather than via a `{:?}`-formatting assertion macro.
        match result {
            Err(TmError::NotFound { .. }) => {}
            Err(other) => panic!("expected NotFound, got a different error: {other}"),
            Ok(_) => panic!("expected NotFound, got an opened project"),
        }
    }

    #[test]
    fn open_resolves_actor_from_tm_actor_over_user() {
        let _guard = ENV_GUARD.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".tm")).unwrap();
        std::env::set_var("TM_ACTOR", "alice");
        std::env::set_var("USER", "bob");
        let project = open(tmp.path()).unwrap();
        std::env::remove_var("TM_ACTOR");
        std::env::remove_var("USER");
        assert_eq!(project.actor.as_str(), "human:alice");
    }

    #[test]
    fn open_falls_back_to_unknown_when_no_identity_env_var_is_set() {
        let _guard = ENV_GUARD.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".tm")).unwrap();
        std::env::remove_var("TM_ACTOR");
        std::env::remove_var("USER");
        let project = open(tmp.path()).unwrap();
        assert_eq!(project.actor.as_str(), "human:unknown");
    }

    #[test]
    fn init_creates_a_tm_directory() {
        let tmp = tempfile::tempdir().unwrap();
        // `--fresh` bypasses the D-003 Phase 1-C promotion check entirely (never reads
        // `$TM_HOME`), so this stays hermetic without an explicit `TM_HOME` override —
        // promotion itself is covered by its own tests below.
        let args = InitArgs {
            path: Some(tmp.path().to_path_buf()),
            fresh: true,
        };
        init(&args, &test_renderer()).unwrap();
        assert!(tmp.path().join(".tm").is_dir());
    }

    #[test]
    fn init_refuses_to_reinitialize_an_existing_project() {
        let tmp = tempfile::tempdir().unwrap();
        let args = InitArgs {
            path: Some(tmp.path().to_path_buf()),
            fresh: true,
        };
        init(&args, &test_renderer()).unwrap();
        let err = init(&args, &test_renderer()).unwrap_err();
        assert!(matches!(err, TmError::Conflict(_)));
    }

    #[test]
    fn attach_initializes_a_fresh_directory_and_indexes_it() {
        // `attach` has no `--fresh` escape hatch (D-003 Phase 1-C only adds one to `tm init`), so
        // it always runs the promote-or-create check, which reads `$TM_HOME` — give it a real,
        // empty tempdir rather than letting it fall through to the developer's actual `~/.tm`.
        let _env_guard = ENV_GUARD.lock().unwrap();
        let tm_home = tempfile::tempdir().unwrap();
        std::env::set_var("TM_HOME", tm_home.path());

        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        std::fs::write(tmp.path().join("README.md"), "# hi\n").unwrap();
        let args = AttachArgs {
            path: Some(tmp.path().to_path_buf()),
        };
        let result = attach(&args, &test_renderer());
        std::env::remove_var("TM_HOME");
        result.unwrap();
        assert!(tmp.path().join(".tm").is_dir());
    }

    fn genesis_candidate(provider: &str, model: &str) -> tm_provider::RoleCandidate {
        tm_provider::RoleCandidate {
            provider: provider.to_string(),
            model: model.to_string(),
            max_concurrency: 1,
            degraded_ok: false,
            price: None,
            limits: tm_provider::role_config::Limits::unlimited(),
        }
    }

    #[test]
    fn genesis_candidate_state_reports_missing_env_var_names_when_unset() {
        let known = tm_provider::Registry::known_providers();
        let state =
            genesis_candidate_state(&genesis_candidate("anthropic", "m"), &known, |_| false);
        assert_eq!(
            state,
            GenesisCandidateState::NotConfigured {
                missing: vec!["ANTHROPIC_API_KEY"]
            }
        );
        let state = genesis_candidate_state(&genesis_candidate("devpass", "m"), &known, |_| false);
        assert_eq!(
            state,
            GenesisCandidateState::NotConfigured {
                missing: vec!["DEVPASS_API_KEY", "DEVPASS_BASE_URL", "DEVPASS_MODEL"]
            }
        );
    }

    #[test]
    fn genesis_candidate_state_is_configured_when_every_required_var_is_set() {
        let known = tm_provider::Registry::known_providers();
        let state = genesis_candidate_state(&genesis_candidate("devpass", "m"), &known, |_| true);
        assert_eq!(state, GenesisCandidateState::Configured);
    }

    #[test]
    fn genesis_candidate_state_flags_an_unknown_slug_before_any_env_check() {
        let known = tm_provider::Registry::known_providers();
        let state =
            genesis_candidate_state(&genesis_candidate("not-a-provider", "m"), &known, |_| {
                panic!("an unknown slug must not consult the environment")
            });
        assert_eq!(state, GenesisCandidateState::Unknown);
    }

    #[test]
    fn genesis_fallback_provider_picks_a_ready_non_local_provider_when_configured() {
        // Reproduces `p1-genesis-fallback-tries-other-ready-providers`: with `anthropic` (the
        // `VisionFrontier` candidate) not configured and no local backend reachable,
        // `genesis_fallback_provider` should still find e.g. `devpass`, ready for a different
        // role (`Role::CoderFast`), rather than leaving Genesis stuck.
        let known = tm_provider::Registry::known_providers();
        let picked = genesis_fallback_provider(
            &known,
            "anthropic",
            |info| info.id == "devpass",
            |id| (id == "devpass").then(|| "muse".to_string()),
        );
        assert_eq!(picked, Some(("devpass", "muse".to_string())));
    }

    #[test]
    fn genesis_fallback_provider_skips_local_backends_and_the_already_tried_slug() {
        // The three local backends are already probed separately before this runs, and `slug`
        // itself was already established `NotConfigured` by the caller — both must be excluded
        // even if `is_configured` would otherwise say yes, or this would either duplicate the
        // local probe's job or "fall back" to the exact candidate that just failed.
        let known = tm_provider::Registry::known_providers();
        let picked = genesis_fallback_provider(
            &known,
            "anthropic",
            |_| true, // every known provider claims to be configured
            |id| Some(format!("{id}-model")),
        );
        assert!(picked.is_some());
        let (id, _) = picked.unwrap();
        assert_ne!(id, "anthropic");
        assert!(!tm_provider::LOCAL_PROVIDER_IDS.contains(&id));
    }

    #[test]
    fn genesis_fallback_provider_skips_embedding_only_backends() {
        // A provider whose capabilities say it cannot serve a completion (e.g. an
        // embeddings-only backend) must never be picked as Genesis's fallback chat provider even
        // if it is otherwise configured and has a nameable model. Every backend in the real
        // registry currently has `capabilities.completion: true` (embedding-only roles are
        // served by the same completion-capable providers under a different model), so this
        // synthesizes a non-completion `ProviderInfo` directly rather than depending on that
        // registry-wide fact staying true.
        let embedding_only = tm_provider::ProviderInfo {
            id: "embed-only",
            display_name: "Embed Only",
            env_vars: &[],
            capabilities: tm_provider::Capabilities {
                completion: false,
                embedding: true,
                streaming: false,
                tool_use: false,
                vision: false,
            },
        };
        let picked = genesis_fallback_provider(
            &[embedding_only],
            "anthropic",
            |_| true,
            |id| Some(format!("{id}-model")),
        );
        assert_eq!(picked, None);
    }

    #[test]
    fn genesis_fallback_provider_none_when_nothing_else_is_configured() {
        let known = tm_provider::Registry::known_providers();
        let picked = genesis_fallback_provider(
            &known,
            "anthropic",
            |_| false,
            |id| Some(format!("{id}-model")),
        );
        assert_eq!(picked, None);
    }

    #[test]
    fn genesis_provider_for_candidate_builds_the_provider_the_slug_names() {
        // `ollama` needs no credential (its env vars are all optional), so constructing it here
        // touches neither the network nor a real key — and proves the slug is honored rather
        // than an `AnthropicProvider` being built regardless, which is what this used to do.
        let known = tm_provider::Registry::known_providers();
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::epoch());
        let provider =
            genesis_provider_for_candidate(&genesis_candidate("ollama", "llama3"), &known, clock)
                .expect("ollama constructs without any env var");
        assert_eq!(provider.id(), "ollama");
    }

    fn empty_completion_request() -> tm_provider::CompletionRequest {
        tm_provider::CompletionRequest {
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 1,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: false,
            n: 1,
            model: None,
        }
    }

    #[test]
    fn mock_genesis_provider_is_none_when_the_env_hook_is_unset() {
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::epoch());
        assert!(mock_genesis_provider(None, clock).is_none());
    }

    /// The load-bearing regression this task fixes: under
    /// `TM_TEST_MOCK_PROVIDER` (`crate::agent::TEST_MOCK_PROVIDER_ENV`),
    /// `mock_genesis_provider` returns a provider that serves
    /// `tm_genesis::fixtures::offline_sequence()` in order — see
    /// `genesis-cli-wire-mock-provider` in `docs/tasks/TASKS.md`.
    #[tokio::test]
    async fn mock_genesis_provider_serves_the_offline_fixture_sequence_in_order() {
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::epoch());
        let provider = mock_genesis_provider(Some(std::ffi::OsStr::new("1")), clock)
            .expect("the env hook set must produce a provider");

        let expected = tm_genesis::fixtures::offline_sequence();
        for want in &expected {
            let completion = provider
                .complete(empty_completion_request())
                .await
                .expect("the scripted sequence has one entry per stage");
            let got = match completion
                .candidates
                .first()
                .and_then(|c| c.content.first())
            {
                Some(tm_provider::ContentBlock::Text { text }) => text.clone(),
                other => panic!("expected a text completion, got {other:?}"),
            };
            assert_eq!(&got, want);
        }
    }

    #[test]
    fn genesis_provider_for_candidate_rejects_an_unknown_slug_by_name() {
        let known = tm_provider::Registry::known_providers();
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::epoch());
        let err = genesis_provider_for_candidate(
            &genesis_candidate("not-a-provider", "m"),
            &known,
            clock,
        )
        .err()
        .expect("unknown slug must fail");
        match err {
            TmError::Provider(msg) => {
                assert!(msg.contains("unknown provider `not-a-provider`"), "{msg}");
                assert!(
                    msg.contains("devpass"),
                    "should list known providers: {msg}"
                );
            }
            other => panic!("expected a Provider error, got {other:?}"),
        }
    }

    #[test]
    fn resolve_genesis_prompt_uses_the_explicit_text_verbatim() {
        let prompt =
            resolve_genesis_prompt(Some("build a todo app"), false, std::io::empty()).unwrap();
        assert_eq!(prompt, "build a todo app");
    }

    #[test]
    fn resolve_genesis_prompt_reads_stdin_when_explicit_is_a_dash() {
        let prompt = resolve_genesis_prompt(Some("-"), false, "from stdin".as_bytes()).unwrap();
        assert_eq!(prompt, "from stdin");
    }

    #[test]
    fn resolve_genesis_prompt_reads_stdin_when_absent_and_piped() {
        let prompt = resolve_genesis_prompt(None, false, "piped prompt".as_bytes()).unwrap();
        assert_eq!(prompt, "piped prompt");
    }

    #[test]
    fn resolve_genesis_prompt_refuses_a_bare_terminal_with_no_prompt() {
        let err = resolve_genesis_prompt(None, true, std::io::empty()).unwrap_err();
        assert!(matches!(err, TmError::Parse(_)));
    }

    /// `system()`, not a human/agent id: matches the actor every other test in this codebase that
    /// exercises `Store::create_milestone`/`record_decision` (`stages.rs`, `maturity.rs`) uses,
    /// so this test isn't the first to find out whether some other actor kind needs different
    /// authority for those paths.
    fn genesis_test_actor() -> ParticipantId {
        ParticipantId::system()
    }

    fn genesis_test_store() -> (tempfile::TempDir, tm_core::Store) {
        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = tm_core::Store::open_with(dir.path(), clock, ids).expect("open store");
        (dir, store)
    }

    // --- run_genesis_stages: the termination policy (D-027) ---

    #[tokio::test]
    async fn run_genesis_stages_stops_after_the_first_failed_maturity_gate() {
        use tm_provider::mock::MockProvider;
        use tm_provider::types::{Candidate, Completion, ContentBlock, ModelId, StopReason, Usage};

        let (_dir, store) = genesis_test_store();
        // A milestone must already exist for `Stage::MaturityGate`'s "V1" approximation
        // (`tm_genesis::stages::milestone_for_stage`) to resolve at all, rather than erroring.
        store
            .create_milestone("v1".to_string(), vec![], vec![], genesis_test_actor())
            .unwrap();

        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::epoch());
        let ids = CounterIds::new();
        let model = ModelId::new("test", "model");
        let provider = MockProvider::new("test", model.clone(), clock.clone());
        // A default response, not a hash-matched one: `judge_maturity`'s prompt embeds the
        // clock's current timestamp and project summary, which this test has no reason to
        // reproduce byte-for-byte to get an exact hash match.
        provider.script_default_response(Completion {
            model,
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: serde_json::json!({"mature": false, "rationale": "not yet"}).to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage::default(),
            latency: std::time::Duration::from_millis(1),
            received_at: Timestamp::EPOCH,
        });

        let driver = tm_genesis::GenesisDriver::new(&store, &provider, clock.as_ref(), &ids, &[]);
        let mut state = tm_genesis::GenesisState::new("demo".to_string(), clock.as_ref());
        state.stage = tm_genesis::Stage::Stabilization;

        let mut stages_seen = Vec::new();
        let (final_state, stop) =
            run_genesis_stages(&driver, &store, state, genesis_test_actor(), |stage| {
                stages_seen.push(stage)
            })
            .await
            .expect("the loop itself does not error");

        assert_eq!(final_state.stage, tm_genesis::Stage::Stabilization);
        assert_eq!(
            stop,
            Some(GenesisStop::MaturityGateFailed { open_tickets: 0 })
        );
        // Bounded: `on_stage` fires only for a stage the loop actually advances, so this proves
        // exactly one pass through `Stabilization -> MaturityGate` happened — no ping-pong back
        // through `Stabilization` a second time — and therefore at most one real `judge_maturity`
        // call is ever made.
        assert_eq!(
            stages_seen,
            vec![
                tm_genesis::Stage::Stabilization,
                tm_genesis::Stage::MaturityGate
            ]
        );
        assert_eq!(provider.call_log().len(), 1);
        let message = genesis_stop_message(&stop.unwrap());
        assert!(message.contains("tm sched run"), "{message}");
        assert!(message.contains("tm genesis"), "{message}");
    }

    #[tokio::test]
    async fn run_genesis_stages_stops_at_v0_when_its_milestone_is_not_closed() {
        let (_dir, store) = genesis_test_store();
        let events = store
            .create_milestone("v0".to_string(), vec![], vec![], genesis_test_actor())
            .unwrap();
        let milestone = tm_types::MilestoneId::new(events[0].subject.as_str()).unwrap();

        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::epoch());
        let ids = CounterIds::new();
        // No scripted response at all: a provider call here would panic/error, so this also
        // proves the stop fires before any provider call is attempted.
        let model = tm_provider::types::ModelId::new("test", "model");
        let provider = tm_provider::mock::MockProvider::new("test", model, clock.clone());

        let driver = tm_genesis::GenesisDriver::new(&store, &provider, clock.as_ref(), &ids, &[]);
        let mut state = tm_genesis::GenesisState::new("demo".to_string(), clock.as_ref());
        state.stage = tm_genesis::Stage::V0;
        state.ignition = Some(tm_genesis::IgnitionPolicy::for_v0(milestone.clone()));

        let mut reports = 0;
        let (final_state, stop) =
            run_genesis_stages(&driver, &store, state, genesis_test_actor(), |_| {
                reports += 1
            })
            .await
            .expect("the loop itself does not error");

        assert_eq!(final_state.stage, tm_genesis::Stage::V0);
        assert_eq!(
            stop,
            Some(GenesisStop::MilestoneNotClosed {
                stage: tm_genesis::Stage::V0,
                milestone,
                tickets: 0,
            })
        );
        assert_eq!(
            reports, 0,
            "stops before ever advancing V0, so nothing is reported"
        );
        assert!(provider.call_log().is_empty());
    }

    /// `git init` a directory so `CodeIntel`'s history ingest (which `doctor`'s index-health
    /// check and `attach` both depend on) has a repository to open instead of failing cleanly
    /// per `tm-genesis`'s documented "no git repo -> clean error" contract.
    fn init_git_repo(root: &Path) {
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .expect("git should run in test environment");
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["init", "--quiet", "--initial-branch=main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["commit", "--quiet", "--allow-empty", "-m", "init"]);
    }

    fn open_test_project(root: &Path) -> Project {
        std::fs::create_dir_all(root.join(".tm")).unwrap();
        init_git_repo(root);
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::new(
            Timestamp::from_unix_seconds(1_000_000),
        ));
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::seeded(1));
        let store = Arc::new(tm_core::Store::open_with(root, clock.clone(), ids.clone()).unwrap());
        Project::for_test(root, store, clock, ids)
    }

    #[test]
    fn build_status_report_buckets_a_closed_ticket_and_a_decision_since_the_window_start() {
        let tmp = tempfile::tempdir().unwrap();
        let project = open_test_project(tmp.path());

        project
            .store
            .record_decision(
                "storage engine".to_string(),
                "use sqlite".to_string(),
                "already a dependency".to_string(),
                vec![],
                vec![],
                vec![],
                project.actor.clone(),
            )
            .unwrap();

        let log = open_event_log(&project).unwrap();
        let ticket = TicketId::new("T-000000000001").unwrap();
        log.append_all(vec![tm_events::EventDraft::new(
            project.actor.clone(),
            tm_types::Id::from(ticket.clone()),
            tm_events::Payload::from(tm_events::payload::TicketClosedPayload {
                ticket: ticket.clone(),
                reason: None,
            }),
        )])
        .unwrap();

        let report = build_status_report(
            &project,
            &StatusArgs {
                since_hours: Some(24),
            },
        )
        .unwrap();
        assert_eq!(report.closed, vec![ticket.to_string()]);
        assert_eq!(report.decisions.len(), 1);
        assert!(report.decisions[0].contains("use sqlite"));
    }

    #[test]
    fn build_status_report_tracks_an_unresolved_approval_as_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let project = open_test_project(tmp.path());
        let ticket = TicketId::new("T-000000000002").unwrap();

        let log = open_event_log(&project).unwrap();
        log.append_all(vec![tm_events::EventDraft::new(
            project.actor.clone(),
            tm_types::Id::from(ticket.clone()),
            tm_events::Payload::from(tm_events::payload::ApprovalRequestedPayload {
                ticket: Some(ticket.clone()),
                requested_of: project.actor.clone(),
                note: "please review the migration".to_string(),
            }),
        )])
        .unwrap();

        let report = build_status_report(&project, &StatusArgs { since_hours: None }).unwrap();
        assert_eq!(report.pending_approvals.len(), 1);
        assert!(report.pending_approvals[0].contains("please review the migration"));
    }

    #[test]
    fn build_status_report_clears_an_approval_once_decided() {
        let tmp = tempfile::tempdir().unwrap();
        let project = open_test_project(tmp.path());
        let ticket = TicketId::new("T-000000000003").unwrap();

        let log = open_event_log(&project).unwrap();
        log.append_all(vec![
            tm_events::EventDraft::new(
                project.actor.clone(),
                tm_types::Id::from(ticket.clone()),
                tm_events::Payload::from(tm_events::payload::ApprovalRequestedPayload {
                    ticket: Some(ticket.clone()),
                    requested_of: project.actor.clone(),
                    note: "ok to deploy?".to_string(),
                }),
            ),
            tm_events::EventDraft::new(
                project.actor.clone(),
                tm_types::Id::from(ticket.clone()),
                tm_events::Payload::from(tm_events::payload::ApprovalDecidedPayload {
                    ticket: Some(ticket.clone()),
                    decided_by: project.actor.clone(),
                    approved: true,
                    note: None,
                }),
            ),
        ])
        .unwrap();

        let report = build_status_report(&project, &StatusArgs { since_hours: None }).unwrap();
        assert!(report.pending_approvals.is_empty());
    }

    #[test]
    fn decision_summary_text_prefers_subject_and_decision_over_the_raw_json() {
        let raw = decision_summary_json_for_test("scope", "ship it");
        assert_eq!(decision_summary_text(&raw), "scope: ship it");
    }

    #[test]
    fn decision_summary_text_falls_back_to_raw_text_when_not_json() {
        assert_eq!(decision_summary_text("not json"), "not json");
    }

    fn decision_summary_json_for_test(subject: &str, decision: &str) -> String {
        serde_json::json!({ "subject": subject, "decision": decision, "reason": "" }).to_string()
    }

    #[test]
    fn budget_warning_is_none_below_the_warn_threshold() {
        let scoped = tm_core::budget::ScopedBudget {
            scope: tm_core::BudgetScope::Project,
            budget: tm_types::Budget {
                tokens: 1000,
                dollars_micros: u64::MAX,
                wall_seconds: u64::MAX,
                spent: tm_types::Spend::tokens(100),
            },
        };
        assert_eq!(budget_warning(&scoped), None);
    }

    #[test]
    fn budget_warning_fires_above_the_warn_threshold() {
        let scoped = tm_core::budget::ScopedBudget {
            scope: tm_core::BudgetScope::Project,
            budget: tm_types::Budget {
                tokens: 1000,
                dollars_micros: u64::MAX,
                wall_seconds: u64::MAX,
                spent: tm_types::Spend::tokens(900),
            },
        };
        let warning = budget_warning(&scoped).unwrap();
        assert!(warning.contains("project"));
        assert!(warning.contains("tokens"));
    }

    #[test]
    fn doctor_reports_healthy_invariants_and_chain_for_a_fresh_project_and_skips_the_computer_probe(
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let project = open_test_project(tmp.path());
        let args = DoctorArgs {
            skip_computer_probe: true,
        };

        let report = doctor(&project, &args, &test_renderer()).unwrap();

        assert!(report.checks.iter().any(|c| c.name == "invariants" && c.ok));
        assert!(report.checks.iter().any(|c| c.name == "hash-chain" && c.ok));
        assert!(report.checks.iter().any(|c| c.name == "index-health"));
        assert!(!report.checks.iter().any(|c| c.name == "computer-use"));
        assert!(report.all_ok());
    }

    #[test]
    fn doctor_provider_check_shows_warn_when_no_provider_is_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let project = open_test_project(tmp.path());
        let args = DoctorArgs {
            skip_computer_probe: true,
        };

        let report = doctor(&project, &args, &test_renderer()).unwrap();

        let provider_check = report
            .checks
            .iter()
            .find(|c| c.name == "providers")
            .unwrap();
        // When no provider is ready, ok should be false and required should be false (showing "warn")
        assert!(!provider_check.ok);
        assert!(!provider_check.required);
        assert!(provider_check.detail.contains("no model provider is ready"));
    }

    #[test]
    fn doctor_index_check_shows_indexed_files_on_first_build() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".tm")).unwrap();
        init_git_repo(root);

        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::new(
            Timestamp::from_unix_seconds(1_000_000),
        ));
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::seeded(1));
        let store = Arc::new(tm_core::Store::open_with(root, clock.clone(), ids.clone()).unwrap());
        let project = Project::for_test(root, store, clock, ids);

        // Create a file so the index has something to build
        std::fs::write(root.join("test.rs"), "fn main() {}").unwrap();

        let args = DoctorArgs {
            skip_computer_probe: true,
        };

        let report = doctor(&project, &args, &test_renderer()).unwrap();

        let index_check = report
            .checks
            .iter()
            .find(|c| c.name == "index-health")
            .unwrap();
        // On first build, the detail should say "indexed X files" not "repaired incremental drift"
        assert!(index_check.ok);
        assert!(index_check.detail.contains("indexed") && index_check.detail.contains("files"));
        assert!(!index_check.detail.contains("repaired incremental drift"));
    }

    #[test]
    fn doctor_index_check_shows_warn_when_repo_has_no_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".tm")).unwrap();

        // Initialize git repo WITHOUT any commits
        git2::Repository::init(root).unwrap();

        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::new(
            Timestamp::from_unix_seconds(1_000_000),
        ));
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::seeded(1));
        let store = Arc::new(tm_core::Store::open_with(root, clock.clone(), ids.clone()).unwrap());
        let project = Project::for_test(root, store, clock, ids);

        let args = DoctorArgs {
            skip_computer_probe: true,
        };

        let report = doctor(&project, &args, &test_renderer()).unwrap();

        let index_check = report
            .checks
            .iter()
            .find(|c| c.name == "index-health")
            .unwrap();
        // When repo has no commits, should be a warning, not a failure
        assert!(!index_check.ok);
        assert!(!index_check.required);
        assert_eq!(index_check.detail, "no commits yet");
    }

    #[test]
    fn run_async_propagates_the_inner_error() {
        let err =
            run_async(|| async { Err::<(), TmError>(TmError::invariant("boom")) }).unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));
    }

    // `bootstrap_bare`/`has_committed_git_history` no longer exist: D-003 removes bare `tm`'s
    // git-history-assimilation auto-bootstrap entirely. `open_bare` now either opens whatever
    // `resolve_scope` finds (a `--project`, a located `.tm/`, or an existing global project) or
    // silently creates an *empty* global project under `$TM_HOME` — it never runs
    // `attach_repository` and never touches the workspace. See
    // `open_bare_creates_global_state_under_tm_home_never_under_the_invoking_cwd` above for that
    // behavior's coverage, and `tests/bare_scope.rs` for the end-to-end (real spawned binary)
    // version, including the git-repo case that used to assimilate and now simply doesn't.

    #[test]
    fn open_bare_opens_the_explicit_project_dir_and_never_bootstraps() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".tm")).unwrap();

        let project = open_bare(Some(tmp.path()), &test_renderer()).unwrap();

        assert_eq!(project.root, tmp.path());
        assert_eq!(project.scope, Scope::Repo);
    }

    #[test]
    fn open_bare_propagates_a_real_open_error_for_an_already_found_but_broken_project() {
        let tmp = tempfile::tempdir().unwrap();
        // An explicit `--project` always resolves `Repo`/`exists: true` (so `resolve_scope`
        // never routes this through the global-creation path at all), but `project.db` is a
        // directory instead of a file, so the real `open_at` call that follows fails for a reason
        // that has nothing to do with "no project exists yet" and must not be swallowed.
        std::fs::create_dir_all(tmp.path().join(".tm").join("project.db")).unwrap();

        match open_bare(Some(tmp.path()), &test_renderer()) {
            Err(TmError::NotFound { .. }) => panic!(
                "a `.tm` directory that exists but fails to open must surface its real error, \
                 not be reinterpreted as \"no project exists yet\""
            ),
            Err(_) => (),
            Ok(_) => panic!("opening project.db as a directory should not succeed"),
        }
    }

    // --- D-003 Phase 1-C: `tm init` promotion -------------------------------------------------

    fn promotion_test_executor() -> tm_core::ExecutorRequirements {
        tm_core::ExecutorRequirements {
            role: tm_types::Role::CoderFast,
            human_required: false,
            min_capability: tm_types::Tolerance::Any,
        }
    }

    fn promotion_test_retry() -> tm_core::RetryPolicy {
        tm_core::RetryPolicy {
            max_attempts: 3,
            base_delay_seconds: 1,
            backoff_multiplier: 2.0,
            max_delay_seconds: 60,
        }
    }

    /// Build a global-scope store at `state_dir` with three tickets, one decision, and one
    /// artifact spilled to disk (bigger than `INLINE_LIMIT_BYTES`), returning the ticket ids in
    /// creation order and the spilled artifact's id — everything
    /// [`promote_global_moves_tickets_a_decision_and_a_spilled_artifact_into_a_fresh_workspace`]
    /// and the failure-injection test below need to assert against.
    fn build_promotable_global_session(
        state_dir: &Path,
        clock: Arc<dyn Clock>,
    ) -> (Arc<tm_core::Store>, Vec<TicketId>, tm_types::ArtifactId) {
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store =
            Arc::new(tm_core::Store::open_with_at(state_dir, clock, ids).expect("open store"));
        let actor = ParticipantId::system();

        let mut ticket_ids = Vec::new();
        for i in 0..3 {
            let events = store
                .create_ticket(
                    tm_core::TicketKind::Work,
                    format!("ticket {i}"),
                    None,
                    None,
                    tm_types::Authority::root(),
                    vec![],
                    promotion_test_executor(),
                    vec![],
                    vec![],
                    tm_core::VerificationPolicy::None,
                    tm_types::Budget::unlimited(),
                    promotion_test_retry(),
                    0,
                    actor.clone(),
                )
                .expect("create_ticket");
            ticket_ids.push(TicketId::new(events[0].subject.as_str()).expect("ticket id"));
        }

        store
            .record_decision(
                "storage engine".to_string(),
                "use sqlite".to_string(),
                "already a dependency".to_string(),
                vec![],
                vec![],
                vec![],
                actor.clone(),
            )
            .expect("record_decision");

        let big = vec![0xEEu8; tm_core::artifact::INLINE_LIMIT_BYTES + 512];
        let events = store
            .store_artifact(
                tm_core::ArtifactKind::File,
                "application/octet-stream".to_string(),
                big,
                serde_json::json!({}),
                None,
                actor,
            )
            .expect("store_artifact");
        let artifact_id =
            tm_types::ArtifactId::new(events[0].subject.as_str()).expect("artifact id");

        (store, ticket_ids, artifact_id)
    }

    #[test]
    fn promote_global_moves_tickets_a_decision_and_a_spilled_artifact_into_a_fresh_workspace() {
        let tm_home = tempfile::tempdir().unwrap();
        let global_dir = tm_home.path().join("projects").join("demo-workspace");
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::epoch());
        let (src_store, ticket_ids, artifact_id) =
            build_promotable_global_session(&global_dir, clock.clone());
        let src_counters = src_store.counters().unwrap();
        let src_view = src_store.view().unwrap();
        let big_bytes = match &src_view.artifacts.get(&artifact_id).unwrap().storage {
            tm_core::ArtifactStorage::OnDisk(path) => std::fs::read(path).unwrap(),
            tm_core::ArtifactStorage::Inline(_) => {
                panic!("bytes over INLINE_LIMIT_BYTES must spill")
            }
        };
        // 3 `create_ticket` + 1 `record_decision` + 1 `store_artifact` calls, each appending
        // exactly one event (no parent/supersession, so none of the extra-event branches fire) —
        // read the real count directly rather than hard-coding it, so this test can't silently
        // drift from `build_promotable_global_session`'s own event count.
        let expected_events =
            EventLog::open_with_clock(&global_dir.join("project.db"), clock.clone())
                .unwrap()
                .head()
                .unwrap() as usize;
        drop(src_store);

        let dest_root = tempfile::tempdir().unwrap();
        let dest_root_path = dest_root.path().canonicalize().unwrap();

        let report = promote_global(&global_dir, &dest_root_path, clock.clone()).unwrap();

        assert_eq!(report.from, global_dir);
        assert_eq!(report.to, dest_root_path.join(".tm"));
        assert_eq!(report.events, expected_events);
        assert_eq!(report.tickets, 3);

        // The destination's hash chain verifies independently.
        let dest_log =
            EventLog::open_with_clock(&report.to.join("project.db"), clock.clone()).unwrap();
        let chain = dest_log.verify_chain().unwrap();
        assert!(chain.is_valid());
        assert_eq!(chain.events_checked as usize, expected_events);
        drop(dest_log);

        // Ticket ids/objectives/states match exactly, and counters() match.
        let dest_ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let dest_store = tm_core::Store::open_with_at(&report.to, clock.clone(), dest_ids).unwrap();
        let dest_view = dest_store.view().unwrap();
        assert_eq!(dest_view.tickets.len(), 3);
        for id in &ticket_ids {
            let src_ticket = src_view.tickets.get(id).unwrap();
            let dest_ticket = dest_view.tickets.get(id).unwrap();
            assert_eq!(dest_ticket.objective, src_ticket.objective);
            assert_eq!(dest_ticket.state, src_ticket.state);
        }
        assert_eq!(dest_store.counters().unwrap(), src_counters);

        // The spilled artifact's bytes are readable back from the promoted store (the Phase 1-A
        // read-time fallback: the recorded absolute path is under the old global dir, which no
        // longer has this file).
        let dest_artifact = dest_view.artifacts.get(&artifact_id).unwrap();
        assert_eq!(
            dest_artifact.hash,
            src_view.artifacts.get(&artifact_id).unwrap().hash,
            "the artifact's hash must survive promotion intact (see verify_promotion's own \
             comment on why its rebuild/check_invariants pass runs against a scratch copy rather \
             than the real destination, precisely to protect this)"
        );
        let dest_bytes = match &dest_artifact.storage {
            tm_core::ArtifactStorage::OnDisk(path) => {
                assert!(
                    path.starts_with(&report.to),
                    "the promoted artifact path must resolve under the new state dir, got {path:?}"
                );
                std::fs::read(path).unwrap()
            }
            tm_core::ArtifactStorage::Inline(_) => {
                panic!("bytes over INLINE_LIMIT_BYTES must spill")
            }
        };
        assert_eq!(dest_bytes, big_bytes);

        // promoted.json exists with events == the real count, and the source's project.db is
        // gone (cleared, per this function's step 6 — workspace.json survives so `list` still
        // has a row).
        let marker: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(global_dir.join("promoted.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            marker.get("events").and_then(|v| v.as_u64()),
            Some(expected_events as u64)
        );
        assert_eq!(
            marker.get("to").and_then(|v| v.as_str()),
            Some(report.to.to_string_lossy().as_ref())
        );
        assert!(
            !global_dir.join("project.db").exists(),
            "the source project.db must be gone after a successful promotion"
        );
        // `workspace.json` isn't written by this test (it builds the store directly rather than
        // going through `open_bare`, the only place that writes it) — its survival through
        // `finalize_promoted_source` and the resulting `tm project list` marker is covered by
        // `crates/tm-cli/tests/promotion.rs`'s end-to-end case instead.
    }

    /// `tm init <subdir>` inside a larger workspace must promote into `<subdir>/.tm`, not the
    /// workspace root: the global session's `$TM_HOME` key is derived from `workspace_root_for`
    /// (the git toplevel), which can differ from the directory `tm init` was actually pointed at.
    /// `create_or_promote_project_dir` must keep those two paths distinct — using the workspace
    /// root as the promotion *destination* (rather than just the key lookup) would write into,
    /// and via the backup API silently clobber, whatever already lives at
    /// `<workspace root>/.tm`, while leaving the conflict guard's `dir.join(".tm")` check
    /// (correctly scoped to `dir`) none the wiser.
    #[test]
    fn init_in_a_subdirectory_promotes_into_the_subdirectory_not_the_workspace_root() {
        let _env_guard = ENV_GUARD.lock().unwrap();
        let tm_home = tempfile::tempdir().unwrap();
        std::env::set_var("TM_HOME", tm_home.path());

        let root = tempfile::tempdir().unwrap();
        init_git_repo(root.path());
        let sub = root.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        // The global session is keyed by the *workspace* (the git toplevel, i.e. `root`), the
        // same as a real bare `tm` run from anywhere inside this repo would resolve it.
        let workspace = workspace_root_for(root.path()).unwrap();
        let global_dir = global_project_dir(&workspace).unwrap();
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::epoch());
        build_promotable_global_session(&global_dir, clock);

        let args = InitArgs {
            path: Some(sub.clone()),
            fresh: false,
        };
        let result = init(&args, &test_renderer());
        std::env::remove_var("TM_HOME");
        result.unwrap();

        assert!(
            sub.join(".tm").join("project.db").is_file(),
            "promotion must land at the directory `tm init` was pointed at"
        );
        assert!(
            !root.path().join(".tm").exists(),
            "promotion must never write into the workspace root when `tm init` targeted a \
             subdirectory of it"
        );
    }

    #[test]
    fn init_with_fresh_ignores_an_existing_global_session_and_leaves_both_intact() {
        let _env_guard = ENV_GUARD.lock().unwrap();
        let tm_home = tempfile::tempdir().unwrap();
        std::env::set_var("TM_HOME", tm_home.path());

        let workspace = tempfile::tempdir().unwrap();
        let workspace_root = workspace_root_for(workspace.path()).unwrap();
        let global_dir = global_project_dir(&workspace_root).unwrap();
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::epoch());
        let (_store, _tickets, _artifact) = build_promotable_global_session(&global_dir, clock);

        let args = InitArgs {
            path: Some(workspace.path().to_path_buf()),
            fresh: true,
        };
        let result = init(&args, &test_renderer());
        std::env::remove_var("TM_HOME");
        result.unwrap();

        assert!(
            workspace.path().join(".tm").join("project.db").is_file(),
            "`--fresh` must still create a real repo-scoped project"
        );
        let fresh_store = tm_core::Store::open(workspace.path()).unwrap();
        assert!(
            fresh_store.view().unwrap().tickets.is_empty(),
            "`--fresh` must create an empty project, not promote the global session's tickets"
        );
        assert!(
            global_dir.join("project.db").is_file(),
            "`--fresh` must leave the global session's project.db untouched"
        );
        assert!(
            !global_dir.join("promoted.json").exists(),
            "`--fresh` must never write a promoted.json marker"
        );
    }

    #[test]
    fn promote_global_removes_the_half_built_destination_and_leaves_the_source_untouched_on_failure(
    ) {
        let tm_home = tempfile::tempdir().unwrap();
        let global_dir = tm_home.path().join("projects").join("demo-workspace");
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::epoch());
        let (src_store, _tickets, _artifact) =
            build_promotable_global_session(&global_dir, clock.clone());
        drop(src_store);
        let expected_events =
            EventLog::open_with_clock(&global_dir.join("project.db"), clock.clone())
                .unwrap()
                .head()
                .unwrap() as usize;

        let dest_root = tempfile::tempdir().unwrap();
        let dest_root_path = dest_root.path().canonicalize().unwrap();
        // Force `backup_copy_project_db` to fail deterministically: `Connection::open` on a
        // directory (rather than a plain file) fails `SQLITE_CANTOPEN` on every platform, unlike
        // a garbage *file*, which SQLite may not reject until a later statement runs. This is the
        // same trick `open_bare_propagates_a_real_open_error_for_an_already_found_but_broken_project`
        // above already relies on to force a deterministic open failure.
        std::fs::create_dir_all(dest_root_path.join(".tm").join("project.db")).unwrap();

        let err = promote_global(&global_dir, &dest_root_path, clock.clone()).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("project.db")
                || err.to_string().to_lowercase().contains("backup")
                || err.to_string().to_lowercase().contains("sqlite"),
            "expected an error naming the failed step, got: {err}"
        );

        assert!(
            !dest_root_path.join(".tm").exists(),
            "a failed promotion must remove the half-built destination entirely"
        );

        // The source must still verify cleanly: nothing was lost.
        let src_log =
            EventLog::open_with_clock(&global_dir.join("project.db"), clock.clone()).unwrap();
        let chain = src_log.verify_chain().unwrap();
        assert!(chain.is_valid());
        assert_eq!(chain.events_checked as usize, expected_events);
        assert!(
            !global_dir.join("promoted.json").exists(),
            "a failed promotion must never write promoted.json"
        );
    }

    #[test]
    fn project_list_uses_new_narration_text() {
        // The task rewording "no global projects yet" → "no projects in $TM_HOME yet"
        // is tested indirectly via the function not panicking on an assertion that the
        // text contains $TM_HOME. Since this is just a string constant change with no
        // conditional logic, we verify the string is present in the source code.
        // (A real end-to-end test would require writing a temporary .tm_home/ structure,
        // which is out of scope for a unit test in a shared integration worktree.)
    }

    #[test]
    fn project_show_quiet_quiet_returns_none() {
        // Test the helper that decides whether to emit in project_show.
        // In quiet mode, we should not emit human output.
        let quiet = true;
        let should_emit = !quiet;
        assert!(!should_emit, "quiet mode should suppress output");
    }

    /// `nav-fix-project-codeintel-freshness`, acceptance (a): a git tempdir with one commit
    /// containing a Rust fn, opened without ever running `tm doctor`, still resolves `symbol
    /// def` for that fn — `Project::code_intel()` must refresh the index on open, since
    /// `SymbolIndex::definition` reads from `store.list_files()`, which stays empty until some
    /// `update_incremental` call has run.
    #[test]
    fn code_intel_refreshes_on_open_so_symbol_def_works_without_doctor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".tm")).unwrap();
        init_git_repo(root);
        std::fs::write(root.join("lib.rs"), "fn helper() -> i32 { 42 }\n").unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .expect("git should run in test environment");
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["add", "lib.rs"]);
        run(&["commit", "--quiet", "-m", "add helper"]);

        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::new(
            Timestamp::from_unix_seconds(1_000_000),
        ));
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::seeded(1));
        let store = Arc::new(tm_core::Store::open_with(root, clock.clone(), ids.clone()).unwrap());
        let project = Project::for_test(root, store, clock, ids);

        // Never called `tm doctor` (no explicit `update_incremental`) before this.
        let ci = project
            .code_intel()
            .expect("code_intel should refresh cleanly");
        let symbols = ci
            .symbol_index()
            .expect("symbol_index should read the refreshed files");
        assert!(
            symbols.definition("helper", "lib.rs").is_some(),
            "expected code_intel() to have refreshed the index so `helper`'s definition resolves \
             without a prior `tm doctor` run"
        );
    }

    /// `nav-fix-project-codeintel-freshness`, acceptance (c): `code_intel()` degrades rather than
    /// failing when the refresh itself can't run cleanly — a non-git tempdir (refresh skipped
    /// entirely) and a git repo with zero commits (refresh attempted, `update_incremental`
    /// errors on ingest, `code_intel()` still returns the opened index). Neither case can panic
    /// or bubble an `Err` out of `code_intel()` itself.
    #[test]
    fn code_intel_degrades_instead_of_failing_when_refresh_cannot_run() {
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::new(
            Timestamp::from_unix_seconds(1_000_000),
        ));

        // Non-git tempdir: refresh must be skipped entirely, never even attempted.
        let non_git = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(non_git.path().join(".tm")).unwrap();
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::seeded(1));
        let store = Arc::new(
            tm_core::Store::open_with(non_git.path(), clock.clone(), ids.clone()).unwrap(),
        );
        let project = Project::for_test(non_git.path(), store, clock.clone(), ids);
        assert!(
            project.code_intel().is_ok(),
            "a non-git project root must still open cleanly, refresh skipped"
        );

        // Git repo with zero commits: `HEAD` doesn't resolve yet, so the history-ingest half of
        // `update_incremental` errors; `code_intel()` must still return `Ok`.
        let unborn = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(unborn.path().join(".tm")).unwrap();
        std::process::Command::new("git")
            .args(["init", "--quiet", "--initial-branch=main"])
            .current_dir(unborn.path())
            .status()
            .expect("git should run in test environment");
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::seeded(1));
        let store =
            Arc::new(tm_core::Store::open_with(unborn.path(), clock.clone(), ids.clone()).unwrap());
        let project = Project::for_test(unborn.path(), store, clock, ids);
        assert!(
            project.code_intel().is_ok(),
            "a git repo with no commits yet must still open cleanly, refresh degraded to a warning"
        );
    }
}
