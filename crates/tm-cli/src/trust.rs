//! `tm trust`: review and allow the repo-committed config that can run code or relax approvals
//! (`hooks.toml`, `acp.toml`, `oversight.toml`). See [`tm_types::trust`].

use std::path::Path;

use tm_types::{TrustPolicy, TrustStatus};

use crate::args::TrustArgs;
use crate::project::Project;
use crate::render::Renderer;

/// Drop control characters (keeping newline and tab) so a hostile file can't drive the terminal
/// while it is shown for review.
fn display_safe(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

/// `tm trust [--revoke] [--status]`.
pub fn dispatch_trust(
    args: &TrustArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    run(args, &project.root, &TrustPolicy::from_env(), renderer)
}

fn run(
    args: &TrustArgs,
    root: &Path,
    policy: &TrustPolicy,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    if args.revoke {
        let removed = policy.revoke(root)?;
        renderer.note(if removed {
            "No longer trusting this project's hooks.toml, acp.toml and oversight.toml."
        } else {
            "This project wasn't trusted."
        });
        return Ok(());
    }
    if args.status {
        let line = match policy.status(root) {
            TrustStatus::NoConfig => {
                "No hooks.toml, acp.toml or oversight.toml here; nothing to trust.".to_string()
            }
            TrustStatus::Trusted => "Trusted: these files will be loaded.".to_string(),
            TrustStatus::Untrusted(f) => format!(
                "Not trusted ({}). Run `tm trust` after reviewing them.",
                f.join(", ")
            ),
            TrustStatus::Changed(f) => format!(
                "Changed since you trusted them ({}). Review, then run `tm trust`.",
                f.join(", ")
            ),
        };
        renderer.note(&line);
        return Ok(());
    }
    let files = TrustPolicy::present_files(root);
    if files.is_empty() {
        renderer.note("No hooks.toml, acp.toml or oversight.toml here; nothing to trust.");
        return Ok(());
    }
    for name in &files {
        let body = std::fs::read_to_string(root.join(name)).unwrap_or_default();
        renderer.note(&format!("--- {name} ---\n{}", display_safe(&body)));
    }
    policy.trust(root)?;
    renderer.note(&format!(
        "Trusted {} for {}. Editing any of them (for example with git pull) asks again.",
        files.join(", "),
        root.display()
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_command_trusts_then_revokes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(root.join("acp.toml"), "[agent]\n").expect("write");
        let policy = TrustPolicy::with_store(dir.path().join("trust"));
        let renderer = Renderer::from_flags(false, true, true);
        assert!(policy.require(&root, "acp.toml").is_err());
        run(
            &TrustArgs {
                revoke: false,
                status: false,
            },
            &root,
            &policy,
            &renderer,
        )
        .expect("trust");
        assert!(policy.require(&root, "acp.toml").is_ok());
        run(
            &TrustArgs {
                revoke: true,
                status: false,
            },
            &root,
            &policy,
            &renderer,
        )
        .expect("revoke");
        assert!(policy.require(&root, "acp.toml").is_err());
    }

    #[test]
    fn review_output_drops_terminal_escapes() {
        assert_eq!(display_safe("a\x1b]52;c;x\x07b\n"), "a]52;c;xb\n");
    }
}
