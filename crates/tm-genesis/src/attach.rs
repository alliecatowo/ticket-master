//! Attach: assimilate an existing repository instead of bootstrapping a green-field one.
//!
//! `tm attach` is the alternate front door into Genesis (`SPEC.md` §12): instead of turning a
//! prompt into a project from nothing, it turns a working tree that already exists into
//! [`tm_core`] project state. [`attach_repository`] indexes the tree ([`tm_codeintel::CodeIntel`]
//! walks, chunks, embeds and ingests git history in one call), builds the symbol graph, discovers
//! documentation, detects the build/test system and any external issue trackers, infers a few
//! cheap conventions, and finally creates the one ticket every attached project starts with:
//! `T-001`, "Understand current architecture sufficiently to safely accept autonomous work".
//!
//! Bias to action applies here exactly as it does to [`crate::seed`]: attach only raises a
//! [`Question`] (`blocking == true`) when nothing about the repository lets it make a reasonable
//! default, and folds everything else into an [`Assumption`] instead. Unlike [`crate::seed`],
//! this module never calls a provider — every fact it produces is read mechanically off the
//! working tree (manifests, well-known filenames, the git remote), so it is fully deterministic
//! given the same tree and the same [`Clock`].

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use tm_codeintel::CodeIntel;
use tm_core::{
    ContextRef, ExecutorRequirements, ResourceClaim, ResourceMode, RetryPolicy, Store, TicketKind,
    VerificationPolicy,
};
use tm_types::{
    Authority, Budget, Clock, ParticipantId, PatternSet, Predicate, RepoAuthority,
    Result as TmResult, Role, Spend, TicketId, TmError, Tolerance,
};

use crate::seed::{Assumption, Question};

/// Well-known documentation filenames/directories attach looks for at the repository root, and
/// what each one is taken to mean.
const DOC_HINTS: &[(&str, DocKind)] = &[
    ("README.md", DocKind::Readme),
    ("README", DocKind::Readme),
    ("readme.md", DocKind::Readme),
    ("CONTRIBUTING.md", DocKind::Contributing),
    ("ARCHITECTURE.md", DocKind::Architecture),
    ("DESIGN.md", DocKind::Architecture),
    ("SPEC.md", DocKind::Architecture),
    ("CHANGELOG.md", DocKind::Changelog),
    ("docs", DocKind::Directory),
    ("doc", DocKind::Directory),
];

/// Manifests attach recognizes, and the build-system name/build-command/test-command they imply.
/// `{manifest}` lines are checked at the repository root only; a monorepo with manifests nested
/// deeper is reported as-is (this table does not walk subdirectories) and left to a human/agent
/// to refine, which is exactly the kind of thing a non-blocking [`Assumption`] exists for.
const BUILD_HINTS: &[(&str, &str, &[&str], &[&str])] = &[
    (
        "Cargo.toml",
        "cargo",
        &["cargo", "build"],
        &["cargo", "test"],
    ),
    (
        "package.json",
        "npm",
        &["npm", "run", "build"],
        &["npm", "test"],
    ),
    (
        "pyproject.toml",
        "python (pyproject)",
        &["python", "-m", "build"],
        &["pytest"],
    ),
    (
        "setup.py",
        "python (setuptools)",
        &["python", "setup.py", "build"],
        &["pytest"],
    ),
    (
        "go.mod",
        "go",
        &["go", "build", "./..."],
        &["go", "test", "./..."],
    ),
    ("Makefile", "make", &["make"], &["make", "test"]),
];

/// What kind of documentation a [`DiscoveredDoc`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocKind {
    /// A README at the repository root.
    Readme,
    /// Contribution guidelines.
    Contributing,
    /// An architecture/design document.
    Architecture,
    /// A changelog.
    Changelog,
    /// A directory of further documentation (`docs/`, `doc/`).
    Directory,
}

/// One piece of documentation attach found in the working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredDoc {
    /// Repository-relative path.
    pub path: String,
    /// What kind of documentation this is.
    pub kind: DocKind,
}

/// A build/test toolchain attach recognized from a manifest file at the repository root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSystem {
    /// Human-readable name, e.g. `"cargo"`.
    pub name: String,
    /// The manifest file that identified this build system.
    pub manifest_path: String,
    /// A reasonable default build command, argv form.
    pub build_command: Vec<String>,
    /// A reasonable default test command, argv form.
    pub test_command: Vec<String>,
}

/// An external issue/ticket tracker attach found evidence of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalTracker {
    /// Tracker name, e.g. `"GitHub Issues"`.
    pub name: String,
    /// What attach saw that implied this tracker, in prose.
    pub evidence: String,
}

/// One convention inferred from the repository, cheap enough not to need a [`Question`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Convention {
    /// The convention, in prose, e.g. "formatted with rustfmt".
    pub text: String,
    /// What on disk implied it.
    pub evidence: String,
}

/// Everything [`attach_repository`] learned about the repository, plus the id of the `T-001`
/// investigation ticket it created.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttachReport {
    /// The repository root that was attached.
    pub project_root: PathBuf,
    /// Files newly indexed or re-indexed by [`tm_codeintel::CodeIntel::update_incremental`].
    pub files_indexed: u64,
    /// Chunks (re)written by the same pass.
    pub chunks_indexed: u64,
    /// Git commits newly ingested into history by the same pass.
    pub commits_ingested: u64,
    /// True once [`tm_codeintel::CodeIntel::symbol_index`] parsed the tree without error (it
    /// tolerates individual unparsable files, so this is false only when the file list itself
    /// could not be read).
    pub symbol_graph_built: bool,
    /// Documentation found in the working tree.
    pub docs: Vec<DiscoveredDoc>,
    /// Build/test systems detected from root-level manifests.
    pub build_systems: Vec<BuildSystem>,
    /// External trackers evidenced by repository metadata.
    pub external_trackers: Vec<ExternalTracker>,
    /// Conventions inferred from configuration files present in the tree.
    pub conventions: Vec<Convention>,
    /// Reasonable defaults adopted instead of asking a human.
    pub assumptions: Vec<Assumption>,
    /// Questions raised during attach; only `blocking == true` ones should ever reach a human.
    pub questions: Vec<Question>,
    /// The `T-001` investigation ticket created to hold "understand this codebase" work.
    pub root_ticket: TicketId,
}

impl AttachReport {
    /// Every question with `blocking == true` that is still open, in encounter order. Mirrors
    /// [`crate::seed::Seed::blocking_open_questions`]: callers surface exactly these.
    pub fn blocking_open_questions(&self) -> Vec<&Question> {
        self.questions
            .iter()
            .filter(|q| q.blocking && q.is_open())
            .collect()
    }
}

/// Look for [`DOC_HINTS`] at `root`, plus every markdown file one level into a discovered
/// documentation directory (bounded to one level: attach is a survey, not a full walk).
fn discover_docs(root: &Path) -> Vec<DiscoveredDoc> {
    let mut docs = Vec::new();
    for (name, kind) in DOC_HINTS {
        let full = root.join(name);
        if !full.exists() {
            continue;
        }
        match kind {
            DocKind::Directory => {
                let Ok(entries) = fs::read_dir(&full) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let is_doc = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("md"))
                        .unwrap_or(false);
                    if is_doc {
                        if let Ok(rel) = path.strip_prefix(root) {
                            docs.push(DiscoveredDoc {
                                path: rel.to_string_lossy().replace('\\', "/"),
                                kind: DocKind::Directory,
                            });
                        }
                    }
                }
            }
            other => docs.push(DiscoveredDoc {
                path: (*name).to_string(),
                kind: *other,
            }),
        }
    }
    docs
}

/// Look for [`BUILD_HINTS`] manifests at `root`.
fn detect_build_systems(root: &Path) -> Vec<BuildSystem> {
    BUILD_HINTS
        .iter()
        .filter(|(manifest, ..)| root.join(manifest).exists())
        .map(|(manifest, name, build, test)| BuildSystem {
            name: (*name).to_string(),
            manifest_path: (*manifest).to_string(),
            build_command: build.iter().map(|s| s.to_string()).collect(),
            test_command: test.iter().map(|s| s.to_string()).collect(),
        })
        .collect()
}

/// Evidence-based external tracker detection: a `.github` directory implies GitHub Issues (CI
/// workflows and issue templates both live there), and the `origin` remote's host, if reachable,
/// names the actual tracker. Never fails: an unreadable/absent `.git` just yields no evidence.
fn detect_external_trackers(root: &Path) -> Vec<ExternalTracker> {
    let mut trackers = Vec::new();
    if root.join(".github").is_dir() {
        trackers.push(ExternalTracker {
            name: "GitHub Issues".to_string(),
            evidence: ".github/ directory present".to_string(),
        });
    }
    if let Ok(repo) = git2::Repository::open(root) {
        if let Ok(remote) = repo.find_remote("origin") {
            if let Some(url) = remote.url() {
                let tracker = if url.contains("github.com") {
                    Some("GitHub Issues")
                } else if url.contains("gitlab.com") {
                    Some("GitLab Issues")
                } else if url.contains("bitbucket.org") {
                    Some("Bitbucket Issues")
                } else {
                    None
                };
                if let Some(name) = tracker {
                    let already = trackers.iter().any(|t: &ExternalTracker| t.name == name);
                    if !already {
                        trackers.push(ExternalTracker {
                            name: name.to_string(),
                            evidence: format!("origin remote {url:?}"),
                        });
                    }
                }
            }
        }
    }
    trackers
}

/// Cheap, file-presence-only convention inference: formatting configs and a lockfile's implied
/// package manager. Anything requiring judgment (naming style, test philosophy, ...) is left to
/// later Genesis stages that actually read code, not to this survey pass.
fn infer_conventions(root: &Path) -> Vec<Convention> {
    const HINTS: &[(&str, &str)] = &[
        (".rustfmt.toml", "formatted with rustfmt"),
        ("rustfmt.toml", "formatted with rustfmt"),
        (".prettierrc", "formatted with Prettier"),
        (".prettierrc.json", "formatted with Prettier"),
        (".editorconfig", "editor settings pinned via .editorconfig"),
        (
            ".pre-commit-config.yaml",
            "pre-commit hooks enforce style before commit",
        ),
        (
            "package-lock.json",
            "npm is the package manager (package-lock.json committed)",
        ),
        (
            "pnpm-lock.yaml",
            "pnpm is the package manager (pnpm-lock.yaml committed)",
        ),
        (
            "yarn.lock",
            "yarn is the package manager (yarn.lock committed)",
        ),
        (
            "Cargo.lock",
            "dependency versions are pinned via Cargo.lock",
        ),
    ];
    HINTS
        .iter()
        .filter(|(file, _)| root.join(file).exists())
        .map(|(file, text)| Convention {
            text: (*text).to_string(),
            evidence: (*file).to_string(),
        })
        .collect()
}

/// Authority granted to `T-001`: broad read access to understand the repository, no write/git/
/// ticket/project powers. An investigation ticket earns write authority (if any) only once it
/// spawns follow-up work, not at creation.
fn investigation_authority() -> Authority {
    Authority {
        repository: RepoAuthority {
            read: PatternSet::all(),
            write: PatternSet::empty(),
        },
        ..Authority::default()
    }
}

/// A generous-but-bounded budget for `T-001`: reading and summarizing a repository can burn a
/// lot of tokens on a large tree, but must still terminate.
fn investigation_budget() -> Budget {
    Budget {
        tokens: 200_000,
        dollars_micros: 5_000_000,
        wall_seconds: 3_600,
        spent: Spend::default(),
    }
}

fn investigation_retry() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        base_delay_seconds: 1,
        backoff_multiplier: 2.0,
        max_delay_seconds: 60,
    }
}

/// Fold detected facts into [`Assumption`]s, raising a `blocking` [`Question`] only where attach
/// truly has no reasonable default to fall back on (`SPEC.md` §12's bias-to-action rule, applied
/// mechanically instead of via a model since every input here is a yes/no file check).
fn assess(
    build_systems: &[BuildSystem],
    external_trackers: &[ExternalTracker],
    docs: &[DiscoveredDoc],
) -> (Vec<Assumption>, Vec<Question>) {
    let mut assumptions = Vec::new();
    let mut questions = Vec::new();

    if build_systems.is_empty() {
        questions.push(Question::new(
            "What command builds and tests this repository?",
            true,
            "no recognized manifest (Cargo.toml, package.json, pyproject.toml, setup.py, go.mod, Makefile) was found at the repository root",
        ));
    } else {
        for system in build_systems {
            assumptions.push(Assumption::new(
                format!(
                    "build with `{}`, test with `{}`",
                    system.build_command.join(" "),
                    system.test_command.join(" ")
                ),
                format!("inferred from {}", system.manifest_path),
                0.8,
            ));
        }
    }

    if external_trackers.is_empty() {
        assumptions.push(Assumption::new(
            "no external issue tracker is integrated; Ticketmaster is the sole tracker",
            "no .github directory and no recognized remote host implying a tracker were found",
            0.6,
        ));
    } else {
        for tracker in external_trackers {
            assumptions.push(Assumption::new(
                format!("{} is the project's external tracker", tracker.name),
                tracker.evidence.clone(),
                0.7,
            ));
        }
    }

    if docs.is_empty() {
        assumptions.push(Assumption::new(
            "no existing documentation describes this repository; T-001 must read source directly",
            "no README, CONTRIBUTING, ARCHITECTURE/DESIGN/SPEC, CHANGELOG or docs/ directory was found",
            0.5,
        ));
    }

    (assumptions, questions)
}

/// Assimilate the repository at `project_root` into `store`'s project state: index it, build the
/// symbol graph, ingest git history, discover docs, detect build/test tooling and external
/// trackers, infer cheap conventions, and create `T-001`.
///
/// `T-001` is always created, whether or not blocking questions were raised: understanding the
/// codebase is exactly the work needed to answer them, so attach does not gate ticket creation on
/// having every answer up front. Callers surface [`AttachReport::blocking_open_questions`] to a
/// human; everything else is recorded as an [`Assumption`] on [`AttachReport`], not asked.
///
/// # Errors
/// Whatever [`tm_codeintel::CodeIntel::open`]/`update_incremental`/`symbol_index` or
/// [`tm_core::Store::create_ticket`] return, e.g. `TmError::Storage` if the repository or the
/// project database cannot be read/written.
pub fn attach_repository(
    project_root: &Path,
    store: &Store,
    clock: &dyn Clock,
    actor: ParticipantId,
) -> TmResult<AttachReport> {
    let code_intel = CodeIntel::open(project_root)?;
    let delta = code_intel.update_incremental(clock)?;
    // `symbol_index` tolerates any individual file's parse failure internally (logging and
    // skipping it); an `Err` here means the file list itself could not be read, which is a real
    // failure, not "nothing had a grammar".
    code_intel.symbol_index()?;

    let docs = discover_docs(project_root);
    let build_systems = detect_build_systems(project_root);
    let external_trackers = detect_external_trackers(project_root);
    let conventions = infer_conventions(project_root);
    let (assumptions, questions) = assess(&build_systems, &external_trackers, &docs);

    let context_refs: Vec<ContextRef> = docs
        .iter()
        .take(5)
        .map(|d| ContextRef {
            locator: d.path.clone(),
            reason: format!("discovered documentation ({:?})", d.kind),
        })
        .collect();

    let events = store.create_ticket(
        TicketKind::Investigation,
        "Understand current architecture sufficiently to safely accept autonomous work".to_string(),
        None,
        None,
        investigation_authority(),
        vec![ResourceClaim {
            paths: PatternSet::all(),
            mode: ResourceMode::Shared,
        }],
        ExecutorRequirements {
            role: Role::ExplorerCheap,
            human_required: false,
            min_capability: Tolerance::Preferred,
        },
        context_refs,
        vec![Predicate::Judgment {
            claim: "the executor can describe this repository's architecture, build/test \
                    workflow and conventions accurately enough to plan safe autonomous work"
                .to_string(),
        }],
        VerificationPolicy::None,
        investigation_budget(),
        investigation_retry(),
        0,
        actor,
    )?;
    let created = events
        .first()
        .ok_or_else(|| TmError::invariant("create_ticket returned no events"))?;
    let root_ticket = TicketId::new(created.subject.as_str())?;

    Ok(AttachReport {
        project_root: project_root.to_path_buf(),
        files_indexed: delta.files_added + delta.files_modified,
        chunks_indexed: delta.chunks_written,
        commits_ingested: delta.commits_ingested,
        symbol_graph_built: true,
        docs,
        build_systems,
        external_trackers,
        conventions,
        assumptions,
        questions,
        root_ticket,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tm_types::{CounterIds, FixedClock, IdSource};

    fn open_store(root: &Path) -> Store {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        Store::open_with(root, clock, ids).expect("open store")
    }

    fn init_git_repo(path: &Path) {
        let repo = git2::Repository::init(path).expect("git init");
        let sig = git2::Signature::new("Test", "test@example.com", &git2::Time::new(0, 0))
            .expect("signature");
        let tree_id = repo
            .index()
            .expect("index")
            .write_tree()
            .expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .expect("commit");
    }

    fn cargo_project(dir: &TempDir) {
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), "pub fn hi() {}\n").unwrap();
        fs::write(dir.path().join("README.md"), "# X\n").unwrap();
    }

    #[test]
    fn attach_happy_path_creates_t001_and_detects_cargo() {
        let dir = TempDir::new().unwrap();
        cargo_project(&dir);
        init_git_repo(dir.path());
        let store = open_store(dir.path());
        let clock = FixedClock::epoch();

        let report =
            attach_repository(dir.path(), &store, &clock, ParticipantId::system()).unwrap();

        assert_eq!(report.root_ticket.as_str(), "T-001");
        assert_eq!(report.build_systems.len(), 1);
        assert_eq!(report.build_systems[0].name, "cargo");
        assert!(report.symbol_graph_built);
        assert!(report.questions.is_empty());
        assert!(report.docs.iter().any(|d| d.path == "README.md"));

        let view = store.view().unwrap();
        assert!(view.tickets.contains_key(&report.root_ticket));
    }

    #[test]
    fn attach_raises_blocking_question_with_no_build_system() {
        let dir = TempDir::new().unwrap();
        init_git_repo(dir.path());
        let store = open_store(dir.path());
        let clock = FixedClock::epoch();

        let report =
            attach_repository(dir.path(), &store, &clock, ParticipantId::system()).unwrap();

        assert!(report.build_systems.is_empty());
        assert_eq!(report.blocking_open_questions().len(), 1);
        // T-001 is still created even though a blocking question was raised.
        assert_eq!(report.root_ticket.as_str(), "T-001");
    }

    #[test]
    fn attach_without_git_repo_fails_cleanly() {
        let dir = TempDir::new().unwrap();
        cargo_project(&dir);
        // No `git init`: `CodeIntel::update_incremental`'s history ingest requires a repository,
        // so attach surfaces that failure rather than papering over it.
        let store = open_store(dir.path());
        let clock = FixedClock::epoch();

        let report = attach_repository(dir.path(), &store, &clock, ParticipantId::system());
        assert!(report.is_err());
    }

    #[test]
    fn discover_docs_finds_root_readme_and_docs_dir_markdown() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("README.md"), "# hi").unwrap();
        fs::create_dir_all(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("docs/guide.md"), "guide").unwrap();
        fs::write(dir.path().join("docs/notes.txt"), "not markdown").unwrap();

        let docs = discover_docs(dir.path());
        assert!(docs
            .iter()
            .any(|d| d.path == "README.md" && d.kind == DocKind::Readme));
        assert!(docs.iter().any(|d| d.path == "docs/guide.md"));
        assert!(!docs.iter().any(|d| d.path.contains("notes.txt")));
    }

    #[test]
    fn discover_docs_empty_for_bare_directory() {
        let dir = TempDir::new().unwrap();
        assert!(discover_docs(dir.path()).is_empty());
    }

    #[test]
    fn detect_build_systems_recognizes_multiple_manifests() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();

        let systems = detect_build_systems(dir.path());
        let names: Vec<_> = systems.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"cargo"));
        assert!(names.contains(&"npm"));
    }

    #[test]
    fn detect_build_systems_empty_without_manifests() {
        let dir = TempDir::new().unwrap();
        assert!(detect_build_systems(dir.path()).is_empty());
    }

    #[test]
    fn detect_external_trackers_from_github_directory() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".github")).unwrap();

        let trackers = detect_external_trackers(dir.path());
        assert_eq!(trackers.len(), 1);
        assert_eq!(trackers[0].name, "GitHub Issues");
    }

    #[test]
    fn detect_external_trackers_empty_without_evidence() {
        let dir = TempDir::new().unwrap();
        assert!(detect_external_trackers(dir.path()).is_empty());
    }

    #[test]
    fn infer_conventions_detects_rustfmt_and_lockfile() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("rustfmt.toml"), "").unwrap();
        fs::write(dir.path().join("Cargo.lock"), "").unwrap();

        let conventions = infer_conventions(dir.path());
        assert_eq!(conventions.len(), 2);
    }

    #[test]
    fn infer_conventions_empty_without_config_files() {
        let dir = TempDir::new().unwrap();
        assert!(infer_conventions(dir.path()).is_empty());
    }

    #[test]
    fn assess_folds_detected_build_system_into_assumption_not_question() {
        let build_systems = vec![BuildSystem {
            name: "cargo".to_string(),
            manifest_path: "Cargo.toml".to_string(),
            build_command: vec!["cargo".to_string(), "build".to_string()],
            test_command: vec!["cargo".to_string(), "test".to_string()],
        }];
        let (assumptions, questions) = assess(&build_systems, &[], &[]);
        assert!(questions.is_empty());
        assert!(assumptions.iter().any(|a| a.text.contains("cargo build")));
    }

    #[test]
    fn assess_raises_blocking_question_without_build_system() {
        let (_, questions) = assess(&[], &[], &[]);
        assert_eq!(questions.len(), 1);
        assert!(questions[0].blocking);
    }

    #[test]
    fn attach_report_blocking_open_questions_filters_resolved() {
        let mut q = Question::new("what builds this?", true, "no manifest found");
        q.resolution = Some("make".to_string());
        let report = AttachReport {
            project_root: PathBuf::from("/tmp"),
            files_indexed: 0,
            chunks_indexed: 0,
            commits_ingested: 0,
            symbol_graph_built: true,
            docs: vec![],
            build_systems: vec![],
            external_trackers: vec![],
            conventions: vec![],
            assumptions: vec![],
            questions: vec![q],
            root_ticket: TicketId::new("T-001").unwrap(),
        };
        assert!(report.blocking_open_questions().is_empty());
    }
}
